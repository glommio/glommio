//! What idle shared channels cost a reactor pass.
//!
//! The measured executor holds N connected but idle shared channels, the way
//! a channel mesh leaves them, and times a no-op io_uring round trip, which
//! goes through the reactor once.
//!
//! ```bash
//! cargo bench -p glommio --features bench --bench shared_channel_registry
//! ```

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use glommio::{channels::shared_channel, nop::NopSubmitter, LocalExecutorBuilder, Placement};
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

const MEASURED_CPU: usize = 10;
const PEER_CPU: usize = 12;

struct Rig {
    run: mpsc::Sender<u64>,
    elapsed: mpsc::Receiver<Duration>,
}

fn rig(channels: usize) -> Rig {
    let mut for_measured = Vec::new();
    let mut for_peer = Vec::new();
    for _ in 0..channels {
        let (tx, rx) = shared_channel::new_bounded::<u64>(1);
        for_measured.push(rx);
        for_peer.push(tx);
    }

    std::thread::spawn(move || {
        LocalExecutorBuilder::new(Placement::Fixed(PEER_CPU))
            .spawn(move || async move {
                let mut held = Vec::new();
                for tx in for_peer {
                    held.push(tx.connect().await);
                }
                futures_lite::future::pending::<()>().await;
                drop(held);
            })
            .unwrap()
            .join()
            .unwrap();
    });

    let (run, commands) = mpsc::channel::<u64>();
    let (report, elapsed) = mpsc::channel::<Duration>();
    std::thread::spawn(move || {
        LocalExecutorBuilder::new(Placement::Fixed(MEASURED_CPU))
            .spawn(move || async move {
                let mut held = Vec::new();
                for rx in for_measured {
                    held.push(rx.connect().await);
                }
                let nop = NopSubmitter::new();
                while let Ok(iters) = commands.recv() {
                    let start = Instant::now();
                    for _ in 0..iters {
                        nop.run_nop().await.unwrap();
                    }
                    report.send(start.elapsed()).unwrap();
                }
                drop(held);
            })
            .unwrap()
            .join()
            .unwrap();
    });
    Rig { run, elapsed }
}

fn registry(c: &mut Criterion) {
    let mut group = c.benchmark_group("idle_shared_channels");
    for channels in [0usize, 16, 64, 256, 1024] {
        let rig = rig(channels);
        group.bench_function(BenchmarkId::new("nop round trip", channels), |b| {
            b.iter_custom(|iters| {
                rig.run.send(iters).unwrap();
                rig.elapsed.recv().unwrap()
            })
        });
    }
    group.finish();
}

criterion_group!(benches, registry);
criterion_main!(benches);
