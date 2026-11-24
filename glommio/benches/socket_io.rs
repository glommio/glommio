//! What the reactor spends on a socket read, per read.
//!
//! A single ping-pong is the wrong shape for this: it pays for a whole trip
//! around the reactor loop per message, so per-read bookkeeping is a rounding
//! error against parking and waking. Reading from many connections that became
//! readable together amortises the loop and leaves the per-read work visible,
//! which is also what a server actually does.
//!
//! The meter is the executor thread's **CPU time**, not wall clock: most of a
//! loopback round trip is waiting for the peer, and waiting is not the thing
//! being measured.
//!
//! The file rung is here because that is the only place `record_io_latencies`
//! does anything: `Reactor::read_dma` and `Reactor::read_buffered` are the only
//! two paths that install a latency collection function, so a socket source
//! never has one whatever the setting. Measuring reads with it on is what says
//! the collection still costs what it used to.
//!
//! ```bash
//! cargo bench --bench socket_io -- --save-baseline before
//! cargo bench --bench socket_io -- --baseline before
//! ```

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
use glommio::{
    io::DmaFile,
    net::{TcpListener, TcpStream},
    LocalExecutorBuilder, Placement,
};
use std::{hint::black_box, time::Duration};

const CONNECTIONS: usize = 64;
const MSG: &[u8] = b"GET / HTTP/1.1\r\nhost: localhost\r\n\r\n";
const READS: usize = 64;

/// The calling thread's CPU time, which is what this benchmark reports.
fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Reads one message from each of `CONNECTIONS` sockets, `rounds` times.
///
/// The executor is built once per measurement rather than once per round: it
/// costs microseconds to construct, which would swamp what is being measured.
fn rounds(rounds: u64) -> Duration {
    LocalExecutorBuilder::new(Placement::Unbound)
        .spawn(move || async move {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();

            let mut writers = Vec::with_capacity(CONNECTIONS);
            let mut readers = Vec::with_capacity(CONNECTIONS);
            for _ in 0..CONNECTIONS {
                let client: TcpStream = TcpStream::connect(addr).await.unwrap();
                client.set_nodelay(true).unwrap();
                writers.push(client);
                readers.push(listener.accept().await.unwrap());
            }

            let mut buf = [0u8; 64];
            let mut cpu = Duration::ZERO;

            for round in 0..rounds + 1 {
                for writer in writers.iter_mut() {
                    writer.write_all(MSG).await.unwrap();
                }

                let start = thread_cpu();
                for reader in readers.iter_mut() {
                    let read = reader.read(&mut buf).await.unwrap();
                    debug_assert_eq!(read, MSG.len());
                    black_box(read);
                }
                // The first round pays for the first source of each kind.
                if round > 0 {
                    cpu += thread_cpu() - start;
                }
            }

            cpu
        })
        .unwrap()
        .join()
        .unwrap()
}

/// Reads a warm file `READS` times per round, with latency collection on.
///
/// This is the configuration the gate has to leave alone: a source that does
/// collect latencies still takes all three readings, and pays one branch for
/// the privilege.
fn file_reads(rounds: u64, record_io_latencies: bool) -> Duration {
    LocalExecutorBuilder::new(Placement::Unbound)
        .record_io_latencies(record_io_latencies)
        .spawn(move || async move {
            let path =
                std::env::temp_dir().join(format!("glommio-socket-io-{}", std::process::id()));
            let file = DmaFile::create(&path).await.unwrap();
            for i in 0..READS {
                let mut buf = file.alloc_dma_buffer(4096);
                buf.as_bytes_mut().fill(7);
                file.write_at(buf, (i * 4096) as u64).await.unwrap();
            }
            file.fdatasync().await.unwrap();
            file.close().await.unwrap();

            let file = DmaFile::open(&path).await.unwrap();
            let mut cpu = Duration::ZERO;
            for round in 0..rounds + 1 {
                let start = thread_cpu();
                for i in 0..READS {
                    let read = file.read_at((i * 4096) as u64, 4096).await.unwrap();
                    black_box(read.len());
                }
                if round > 0 {
                    cpu += thread_cpu() - start;
                }
            }
            file.close().await.unwrap();
            std::fs::remove_file(&path).ok();
            cpu
        })
        .unwrap()
        .join()
        .unwrap()
}

/// One connection, one byte each way, so every I/O blocks and pays for its own
/// trip into the kernel. The other rung amortises that across 64 connections.
fn ping_pong(iters: u64) -> Duration {
    LocalExecutorBuilder::new(Placement::Unbound)
        .spawn(move || async move {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();

            let echo = glommio::spawn_local(async move {
                let mut stream = listener.accept().await.unwrap();
                let mut byte = [0u8; 1];
                loop {
                    match stream.read(&mut byte).await.unwrap() {
                        0 => break,
                        _ => stream.write_all(&byte).await.unwrap(),
                    }
                }
            })
            .detach();

            let mut client: TcpStream = TcpStream::connect(addr).await.unwrap();
            client.set_nodelay(true).unwrap();
            let mut byte = [0u8; 1];
            client.write_all(b"x").await.unwrap();
            client.read_exact(&mut byte).await.unwrap();

            let start = thread_cpu();
            for _ in 0..iters {
                client.write_all(b"x").await.unwrap();
                client.read_exact(&mut byte).await.unwrap();
                black_box(&byte);
            }
            let elapsed = thread_cpu() - start;

            client.close().await.unwrap();
            echo.await;
            elapsed
        })
        .unwrap()
        .join()
        .unwrap()
}

fn socket_io(c: &mut Criterion) {
    let mut group = c.benchmark_group("socket_io");
    group.throughput(Throughput::Elements(CONNECTIONS as u64));

    group.bench_function("socket reads", |b| b.iter_custom(rounds));
    group.bench_function("ping pong", |b| b.iter_custom(ping_pong));
    group.finish();

    let mut group = c.benchmark_group("file_io");
    group.throughput(Throughput::Elements(READS as u64));
    group.bench_function("dma reads, recording latencies", |b| {
        b.iter_custom(|iters| file_reads(iters, true))
    });
    group.finish();
}

criterion_group!(benches, socket_io);
criterion_main!(benches);
