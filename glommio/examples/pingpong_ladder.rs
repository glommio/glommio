//! What the early readiness poll in `poll_read` is worth where it should win.
//!
//! After a short read, `net/stream.rs` arms a readiness poll on the guess that
//! the next read will have to wait:
//!
//! ```ignore
//! if result > 0 && result < buf.len() {
//!     self.source_rx = Some(reactor.poll_read_ready(self.stream.as_raw_fd()));
//! }
//! ```
//!
//! It is a bet. It costs an SQE, a kernel enter and a CQE on every short read.
//! It pays when the next read would otherwise spend a wasted `recv` returning
//! `EAGAIN` before registering a poll of its own.
//!
//! Strict request/response is the workload where it should pay: the server
//! answers, then the next read genuinely blocks until the client speaks again,
//! so every arming is used. `recv_ladder` measures the opposite case, where
//! data is always waiting and the bet never pays.
//!
//! Server CPU per round trip is the meter. The client is a plain blocking
//! socket on another core so it cannot be the thing being measured.
//!
//! ```sh
//! cargo run --release --example pingpong_ladder
//! ```

use futures_lite::{AsyncReadExt, AsyncWriteExt};
use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

const CONNECTIONS: usize = 32;
const ROUNDS: usize = 400;
const WARMUP: usize = 100;
const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nhost: localhost\r\n\r\n";
const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
const CPU_SERVER: usize = 4;
const CPU_CLIENT: usize = 0;

fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

fn pin(cpu: usize) {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

fn main() {
    pin(CPU_CLIENT);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();

    let server = glommio::LocalExecutorBuilder::new(glommio::Placement::Fixed(CPU_SERVER))
        .spawn(move || async move {
            let listener = glommio::net::TcpListener::bind(addr).unwrap();
            ready_tx.send(()).unwrap();
            let mut streams = Vec::with_capacity(CONNECTIONS);
            for _ in 0..CONNECTIONS {
                streams.push(listener.accept().await.unwrap());
            }

            let mut cpu = Duration::ZERO;
            let mut buf = [0u8; 256];
            for round in 0..WARMUP + ROUNDS {
                let start = thread_cpu();
                for stream in streams.iter_mut() {
                    let read = stream.read(&mut buf).await.unwrap();
                    assert_eq!(read, REQUEST.len(), "short request");
                    stream.write_all(RESPONSE).await.unwrap();
                }
                if round >= WARMUP {
                    cpu += thread_cpu() - start;
                }
            }
            cpu
        })
        .unwrap();

    ready_rx.recv().unwrap();
    let mut clients: Vec<TcpStream> = (0..CONNECTIONS)
        .map(|_| {
            let s = TcpStream::connect(addr).unwrap();
            s.set_nodelay(true).unwrap();
            s
        })
        .collect();

    let mut sink = [0u8; 256];
    for _ in 0..WARMUP + ROUNDS {
        for client in clients.iter_mut() {
            client.write_all(REQUEST).unwrap();
            let read = client.read(&mut sink).unwrap();
            assert_eq!(read, RESPONSE.len(), "short response");
        }
    }

    let cpu = server.join().unwrap();
    let trips = (CONNECTIONS * ROUNDS) as u32;
    println!(
        "request/response, {CONNECTIONS} connections: {:>6} ns cpu / round trip",
        (cpu / trips).as_nanos()
    );
}
