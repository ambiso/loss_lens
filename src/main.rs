use std::{
    collections::VecDeque,
    fs::File,
    io::{stdout, BufWriter, Write},
    net::{Ipv6Addr, SocketAddr, ToSocketAddrs},
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

fn main() -> eyre::Result<()> {
    use std::net::UdpSocket;
    use std::thread;
    use std::time::{Duration, Instant};

    const PACKET_SIZE: usize = 5;

    const HELLO_PACKET_CONST: u8 = 1;
    const SEQ_NUM_PACKET_CONST: u8 = 2;
    const ACK_PACKET_CONST: u8 = 3;

    let args = args::Args::parse();

    // Number of packets to keep track of
    const LATE_WINDOW: usize = PACKETS_PER_SECOND * 3;
    // Number of milliseconds between packets
    const PACKETS_PER_SECOND: usize = 6700;

    match args.command {
        args::Commands::Client { host } => {
            let socket = UdpSocket::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)))?;
            let addr = host.to_socket_addrs()?.next().unwrap();
            socket.connect(addr)?;

            thread::spawn({
                let socket = socket.try_clone()?;
                move || -> eyre::Result<()> {
                    let start_time = Instant::now();
                    let mut buf = [0u8; PACKET_SIZE];
                    const SLOT_SIZE: usize = 64;
                    let mut time_slots = VecDeque::<u64>::new();
                    let mut seq_offset = 1;
                    let mut received = 0;
                    let mut total_packets = 0;
                    let mut last_print = 0;
                    // let mut out = stdout().lock();

                    let mut out = BufWriter::new(File::create("out")?);
                    loop {
                        let (n, _addr) = socket.recv_from(&mut buf)?;
                        if n == PACKET_SIZE && buf[0] == ACK_PACKET_CONST {
                            let received_seq = u32::from_be_bytes(buf[1..5].try_into().unwrap());
                            // account for reordering by keeping track of which sequence numbers have not been responded to yet
                            // remove overly late packets from the datastructure and count them as lost
                            while time_slots.len() * SLOT_SIZE > LATE_WINDOW {
                                if let Some(packets_received) = time_slots.pop_front() {
                                    let new_rx = packets_received.count_ones();
                                    received += new_rx;
                                    out.write_all(&[new_rx as u8])?;
                                    out.flush()?;
                                    total_packets += SLOT_SIZE;
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
                                time_slots[idx / SLOT_SIZE] |= 1 << (idx % SLOT_SIZE);
                            }

                            if total_packets - last_print > PACKETS_PER_SECOND && total_packets > 0
                            {
                                last_print = total_packets;
                                let elapsed = start_time.elapsed().as_secs_f64();
                                let loss_percentage =
                                    100.0 * (1.0 - (received as f64 / total_packets as f64));

                                println!();
                                println!(
                                    "Estimated traffic: {:.02} KiB/s",
                                    2.0 * ((received_seq * (20 + 8 + 4)) as f64 / (1 << 10) as f64)
                                        / start_time.elapsed().as_secs_f64()
                                );
                                println!("Packets sent: {}", total_packets);
                                println!("Packets received: {}", received);
                                println!("Packet loss: {:.2}%", loss_percentage);
                                println!("Time elapsed: {:.2} seconds", elapsed);
                            }
                        }
                    }
                    // Ok(())
                }
            });

            for seq in 0u32.. {
                let mut buf = [0u8; PACKET_SIZE];
                buf[0] = SEQ_NUM_PACKET_CONST;
                buf[1..5].copy_from_slice(&seq.to_be_bytes());

                socket.send_to(&buf, addr)?;

                thread::sleep(Duration::from_nanos(
                    1_000_000_000 / PACKETS_PER_SECOND as u64,
                ));
            }
        }
        args::Commands::Server { host } => {
            let socket = UdpSocket::bind(host)?;

            loop {
                let mut buf = [0u8; PACKET_SIZE];

                match socket.recv_from(&mut buf) {
                    Ok((n, addr)) if n >= 5 => {
                        buf[0] = ACK_PACKET_CONST;
                        // if rand::random_bool(0.5) {
                        socket.send_to(&buf, addr)?;
                        // }
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}
