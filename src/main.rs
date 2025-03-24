use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{ErrorKind, Write},
    net::{Ipv6Addr, SocketAddr, ToSocketAddrs},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
};

use clap::Parser;

mod args {
    use clap::{Parser, Subcommand};

    /// Loss Lens
    #[derive(Parser)]
    #[command(version, about, long_about = None)]
    pub struct Args {
        #[command(subcommand)]
        pub command: Commands,
    }

    #[derive(Subcommand)]
    pub enum Commands {
        Client {
            /// Host to connect to
            #[arg(long, default_value = "127.0.0.1:13337")]
            host: String,
        },
        Server {
            /// Listen
            #[arg(long, default_value = "127.0.0.1:13337")]
            host: String,
        },
    }
}

struct ClientSharedState {
    client_sent: AtomicU32,
    done: AtomicBool,
}

fn main() -> eyre::Result<()> {
    use std::net::UdpSocket;
    use std::thread;
    use std::time::{Duration, Instant};

    const CLIENT_TO_SERVER_PACKET_SIZE: usize = 1 + 4 + 4;
    const SERVER_TO_CLIENT_PACKET_SIZE: usize = 1 + 4 + 4;
    const BUF_SIZE: usize = 1 + 4 + 4;

    // const HELLO_PACKET_CONST: u8 = 1;
    const SEQ_NUM_PACKET_CONST: u8 = 2;
    const ACK_PACKET_CONST: u8 = 3;

    let args = args::Args::parse();

    // Number of packets to keep track of
    const LATE_WINDOW: usize = PACKETS_PER_SECOND * 3;
    // Number of milliseconds between packets
    const PACKETS_PER_SECOND: usize = 67;

    match args.command {
        args::Commands::Client { host } => {
            let socket = UdpSocket::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)))?;
            let addr = host.to_socket_addrs()?.next().unwrap();
            socket.connect(addr)?;
            let client_id: u32 = rand::random();

            let state = Arc::new(ClientSharedState {
                client_sent: AtomicU32::new(0),
                done: AtomicBool::new(false),
            });

            let mut cmd = Command::new("zstd")
                .arg("-9")
                .stdin(Stdio::piped())
                .stdout(File::create("out.zst")?)
                .spawn()?;

            ctrlc::set_handler({
                let state = Arc::clone(&state);
                move || {
                    state.done.store(true, Ordering::SeqCst);
                }
            })
            .expect("Error setting Ctrl-C handler");

            let t = thread::spawn({
                let state = Arc::clone(&state);
                let socket = socket.try_clone()?;
                move || -> eyre::Result<()> {
                    let done = &state.done;
                    let client_sent = &state.client_sent;
                    let start_time = Instant::now();
                    let mut buf = [0u8; BUF_SIZE];
                    const SLOT_SIZE: usize = 64;
                    let mut time_slots = VecDeque::<u64>::new();
                    let mut seq_offset = 1;
                    let mut client_received = 0;
                    let mut server_received = 0;
                    let mut last_print = 0;
                    let out = cmd.stdin.as_mut().unwrap();

                    socket.set_read_timeout(Some(Duration::from_millis(50)))?;

                    let rv = (|| {
                        let mut last_recv: Option<Instant> = None;
                        let mut lags = [0; 10];
                        while !done.load(Ordering::SeqCst) {
                            let (n, _addr) = match socket.recv_from(&mut buf) {
                                Ok(x) => Ok(x),
                                Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                                x => x,
                            }?;
                            if let Some(last) = last_recv {
                                let dur = last.elapsed().as_millis() / 100;
                                if dur >= 1 {
                                    dbg!(last.elapsed());
                                    if (dur as usize) < lags.len() {
                                        lags[dur as usize] += 1;
                                    } else {
                                        lags[lags.len() - 1] += 1;
                                    }
                                }
                            }
                            last_recv = Some(Instant::now());
                            if n == SERVER_TO_CLIENT_PACKET_SIZE && buf[0] == ACK_PACKET_CONST {
                                let received_seq =
                                    u32::from_be_bytes(buf[1..5].try_into().unwrap());
                                server_received = u32::from_be_bytes(buf[5..9].try_into().unwrap())
                                    .max(server_received);
                                // account for reordering by keeping track of which sequence numbers have not been responded to yet
                                // remove overly late packets from the datastructure and count them as lost
                                while time_slots.len() * SLOT_SIZE > LATE_WINDOW {
                                    if let Some(packets_received) = time_slots.pop_front() {
                                        let new_rx = packets_received.count_ones();
                                        // TODO: compression
                                        // TODO: write timestamps
                                        out.write_all(&[new_rx as u8])?;
                                        out.flush()?;
                                        seq_offset += SLOT_SIZE;
                                    }
                                }

                                // packet already counted as lost if it didn't arrive within this window
                                if received_seq as usize >= seq_offset {
                                    // make space for new sequence numbers
                                    while received_seq as usize
                                        >= time_slots.len() * SLOT_SIZE + seq_offset
                                    {
                                        time_slots.push_back(0u64);
                                    }
                                    let idx = received_seq as usize - seq_offset;
                                    if time_slots[idx / SLOT_SIZE] & (1 << (idx % SLOT_SIZE)) == 0 {
                                        client_received += 1;
                                    }
                                    time_slots[idx / SLOT_SIZE] |= 1 << (idx % SLOT_SIZE);
                                }

                                if server_received as usize - last_print > PACKETS_PER_SECOND
                                    && server_received > 0
                                {
                                    last_print = server_received as usize;
                                    let elapsed = start_time.elapsed().as_secs_f64();
                                    let client_sent = client_sent.load(Ordering::SeqCst);
                                    let upstream_loss = 100.0
                                        * (1.0 - (server_received as f64 / client_sent as f64));
                                    let downstream_loss = 100.0
                                        * (1.0 - (client_received as f64 / server_received as f64));

                                    println!();
                                    println!(
                                        "Estimated traffic: {:.02} KiB/s",
                                        (((client_sent + server_received) * (54)) as f64
                                            / (1 << 10) as f64)
                                            / elapsed
                                    );
                                    println!("Client sent    : {client_sent}",);
                                    println!("Server received: {server_received}");
                                    println!("Client received: {client_received}");
                                    println!("Client   upstream loss: {upstream_loss:.2}%");
                                    println!("Client downstream loss: {downstream_loss:.2}%");
                                    print!("Lags per hour: ");
                                    let mut lags = lags.clone();
                                    for i in (0..lags.len() - 1).rev() {
                                        lags[i] += lags[i + 1];
                                    }
                                    for (i, x) in lags[1..].iter().enumerate() {
                                        print!("{:.02} (>={}ms), ", *x as f64 / elapsed * 3600.0, (i+1)*100);
                                    }
                                    println!("");
                                    println!("Time elapsed: {elapsed:.2} seconds");
                                }
                            }
                        }
                        Ok(())
                    })();
                    out.flush()?;
                    dbg!(cmd.wait_with_output())?;
                    rv
                }
            });

            for seq in 1u32.. {
                if state.done.load(Ordering::SeqCst) {
                    break;
                }
                let mut buf = [0u8; CLIENT_TO_SERVER_PACKET_SIZE];
                buf[0] = SEQ_NUM_PACKET_CONST;
                buf[1..5].copy_from_slice(&seq.to_be_bytes());
                buf[5..9].copy_from_slice(&client_id.to_be_bytes());

                socket.send_to(&buf, addr)?;

                state.client_sent.fetch_add(1, Ordering::SeqCst);

                thread::sleep(Duration::from_nanos(
                    1_000_000_000 / PACKETS_PER_SECOND as u64,
                ));
            }
            t.join().unwrap()?;
        }
        args::Commands::Server { host } => {
            let socket = UdpSocket::bind(host)?;

            let mut rx_map = HashMap::new();
            let mut buf = [0u8; BUF_SIZE];

            let mut last_check = Instant::now();

            loop {
                match socket.recv_from(&mut buf) {
                    Ok((n, addr)) if n == CLIENT_TO_SERVER_PACKET_SIZE => {
                        let now = Instant::now();
                        if rx_map.len() > 1000 && last_check.elapsed().as_secs() > 1 {
                            last_check = now;
                            rx_map.retain(|_, x: &mut (u32, Instant)| x.1.elapsed().as_secs() < 10)
                        }
                        let client_id = u32::from_be_bytes(buf[5..9].try_into().unwrap());
                        let e = rx_map.entry(client_id).or_insert_with(|| (0, now));
                        e.0 += 1;
                        e.1 = now;
                        buf[0] = ACK_PACKET_CONST;
                        buf[5..9].copy_from_slice(u32::to_be_bytes(e.0).as_slice());
                        socket.send_to(&buf, addr)?;
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}
