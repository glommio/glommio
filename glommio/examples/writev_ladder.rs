//! What one vectored write is worth against three scalar ones.
//!
//! An HTTP response is a status line, headers and a body: three buffers a
//! server would rather not concatenate. Three ways to put them on a socket:
//!
//! 1. three `write_all` calls -- what a caller had to do before glommio had a
//!    `poll_write_vectored`, since the futures-io default writes only the
//!    first slice;
//! 2. one `write_vectored`;
//! 3. concatenate into one buffer, then one `write_all` -- the workaround,
//!    which trades a copy for the syscalls.
//!
//! Two things are measured, because they are different claims: wall time per
//! response, and **segments put on the wire** (`OutSegs` from
//! `/proc/net/snmp`). With `TCP_NODELAY` set -- which a latency-sensitive
//! server does set -- three writes can mean three packets for one response,
//! and that costs far more off loopback than the syscalls do.
//!
//! Run with:
//! ```bash
//! cargo run --release --example writev_ladder
//! ```

use futures_lite::io::AsyncWriteExt;
use glommio::{net::TcpStream, LocalExecutorBuilder, Placement};
use std::{
    io::{IoSlice, Read},
    net::TcpListener,
    time::Instant,
};

const RESPONSES: usize = 100_000;
const STATUS: &[u8] = b"HTTP/1.1 200 OK\r\n";
const HEADERS: &[u8] = b"content-type: text/plain\r\ncontent-length: 13\r\n\r\n";

/// Two body sizes, because they answer different questions. A tiny body
/// isolates the syscall and segment counts; a large one is where
/// concatenating means copying the whole payload per response, which is the
/// only place `write_vectored` can beat it on work done rather than on calls
/// made.
const SMALL_BODY: usize = 13;
const LARGE_BODY: usize = 64 * 1024;

/// TCP segments this host has sent, so a run can be charged for the packets it
/// caused rather than the packets it hoped to cause.
fn out_segs() -> u64 {
    let snmp = std::fs::read_to_string("/proc/net/snmp").expect("no /proc/net/snmp");
    let mut lines = snmp.lines().filter(|line| line.starts_with("Tcp:"));
    let names = lines.next().expect("no Tcp: header");
    let values = lines.next().expect("no Tcp: values");
    let column = names
        .split_whitespace()
        .position(|name| name == "OutSegs")
        .expect("no OutSegs column");
    values
        .split_whitespace()
        .nth(column)
        .and_then(|value| value.parse().ok())
        .expect("OutSegs not a number")
}

fn drain_server(listener: TcpListener) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 65536];
        while let Ok(read) = stream.read(&mut buf) {
            if read == 0 {
                break;
            }
        }
    })
}

#[derive(Clone, Copy)]
enum Mode {
    ThreeWrites,
    Vectored,
    ConcatThenWrite,
}

fn run(mode: Mode, body: Vec<u8>, label: &str) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = drain_server(listener);

    let before = out_segs();
    let elapsed = LocalExecutorBuilder::new(Placement::Unbound)
        .spawn(move || async move {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            // Without this the kernel coalesces the three writes and the
            // comparison measures Nagle rather than the code under test.
            stream.set_nodelay(true).unwrap();

            let concatenated: Vec<u8> = [STATUS, HEADERS, &body].concat();

            let start = Instant::now();
            for _ in 0..RESPONSES {
                match mode {
                    Mode::ThreeWrites => {
                        stream.write_all(STATUS).await.unwrap();
                        stream.write_all(HEADERS).await.unwrap();
                        stream.write_all(&body).await.unwrap();
                    }
                    Mode::Vectored => {
                        let slices = [
                            IoSlice::new(STATUS),
                            IoSlice::new(HEADERS),
                            IoSlice::new(&body),
                        ];
                        // A large body will not go out in one call, so this
                        // is the short-write loop a caller must write anyway.
                        let mut sent = 0;
                        while sent < concatenated.len() {
                            sent += if sent == 0 {
                                stream.write_vectored(&slices).await.unwrap()
                            } else {
                                stream.write_all(&concatenated[sent..]).await.unwrap();
                                concatenated.len() - sent
                            };
                        }
                    }
                    Mode::ConcatThenWrite => {
                        // The copy is the point: a caller without vectored
                        // writes pays it on every response.
                        let joined: Vec<u8> = [STATUS, HEADERS, &body].concat();
                        stream.write_all(&joined).await.unwrap();
                    }
                }
            }
            let elapsed = start.elapsed();
            stream.close().await.unwrap();
            elapsed
        })
        .unwrap()
        .join()
        .unwrap();

    server.join().unwrap();
    let segments = out_segs() - before;

    println!(
        "{label:<34} {:>8.0} ns/response   {:>5.2} segments/response",
        elapsed.as_nanos() as f64 / RESPONSES as f64,
        segments as f64 / RESPONSES as f64,
    );
}

fn main() {
    for size in [SMALL_BODY, LARGE_BODY] {
        let body = vec![b'x'; size];
        println!(
            "\n{} responses, {}-byte body ({} bytes total), TCP_NODELAY on, loopback",
            RESPONSES,
            size,
            STATUS.len() + HEADERS.len() + size
        );
        run(Mode::ThreeWrites, body.clone(), "three write_all calls");
        run(Mode::Vectored, body.clone(), "one write_vectored");
        run(
            Mode::ConcatThenWrite,
            body.clone(),
            "concat, then one write_all",
        );
    }
}
