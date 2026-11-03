//! What a split connection costs, per round trip, against the alternative:
//! `futures_lite::io::split`, which wraps the stream in an `Arc<Mutex>` and
//! locks it on every poll.
//!
//! Each arm runs `CONNECTIONS` pipelined echo clients on one executor and
//! reports the executor thread's **CPU time** per round trip: wall clock on
//! loopback is mostly waiting for the peer, and waiting is not the thing being
//! measured. The server side is the same unsplit echo loop in both arms, and
//! because the clients write ahead it echoes several messages per read in
//! both, so the arms differ only in how the client reads and writes. A
//! lockstep client, one message in flight, is a different workload, with one
//! server read per round trip, and is not a comparison for either arm.
//!
//! ```bash
//! cargo bench --bench stream_split
//! ```

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use futures_lite::{
    future::zip,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
};
use glommio::{
    net::{TcpListener, TcpStream},
    LocalExecutorBuilder, Placement,
};
use std::time::Duration;

const CONNECTIONS: usize = 64;
const MSG: &[u8] = b"GET / HTTP/1.1\r\nhost: localhost\r\n\r\n";

/// The calling thread's CPU time, which is what this benchmark reports.
fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Echoes until the peer closes.
async fn echo(mut server: TcpStream) {
    let mut buf = [0u8; 256];
    loop {
        let n = server.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        server.write_all(&buf[..n]).await.unwrap();
    }
}

/// `rounds` messages out through `tx` while `rounds` echoes come back through
/// `rx`, as two tasks that never wait for each other.
async fn pump<R, W>(mut rx: R, mut tx: W, rounds: u64)
where
    R: AsyncRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let writer = glommio::spawn_local(async move {
        for _ in 0..rounds {
            tx.write_all(MSG).await.unwrap();
        }
        tx
    });
    let mut buf = [0u8; MSG.len()];
    for _ in 0..rounds {
        rx.read_exact(&mut buf).await.unwrap();
    }
    drop(writer.await);
}

/// Which way the client side reads and writes.
#[derive(Clone, Copy)]
enum Arm {
    /// `futures_lite::io::split`: an `Arc<Mutex>` around the stream.
    MutexSplit,
    /// `TcpStream::split`.
    NativeSplit,
}

/// `rounds` round trips on each of `CONNECTIONS` connections, measured as the
/// executor thread's CPU time.
fn run(arm: Arm, rounds: u64) -> Duration {
    LocalExecutorBuilder::new(Placement::Unbound)
        .spawn(move || async move {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();

            let mut clients = Vec::with_capacity(CONNECTIONS);
            let mut servers = Vec::with_capacity(CONNECTIONS);
            for _ in 0..CONNECTIONS {
                let (server, client) = zip(listener.accept(), TcpStream::connect(addr)).await;
                servers.push(glommio::spawn_local(echo(server.unwrap())));
                clients.push(client.unwrap());
            }

            let start = thread_cpu();
            let mut tasks = Vec::with_capacity(CONNECTIONS);
            for client in clients {
                tasks.push(glommio::spawn_local(async move {
                    match arm {
                        Arm::MutexSplit => {
                            let (rx, tx) = futures_lite::io::split(client);
                            pump(rx, tx, rounds).await;
                        }
                        Arm::NativeSplit => {
                            let (rx, tx) = client.split();
                            pump(rx, tx, rounds).await;
                        }
                    }
                }));
            }
            for task in tasks {
                task.await;
            }
            let elapsed = thread_cpu() - start;

            for server in servers {
                server.await;
            }
            elapsed
        })
        .unwrap()
        .join()
        .unwrap()
}

fn stream_split(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_split");
    group.throughput(Throughput::Elements(CONNECTIONS as u64));
    group.bench_function("futures-lite split", |b| {
        b.iter_custom(|iters| run(Arm::MutexSplit, iters))
    });
    group.bench_function("native split", |b| {
        b.iter_custom(|iters| run(Arm::NativeSplit, iters))
    });
    group.finish();
}

criterion_group!(benches, stream_split);
criterion_main!(benches);
