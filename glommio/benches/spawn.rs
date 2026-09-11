//! What a spawn costs, and what it costs when many cores spawn at once.
//!
//! The scaling group is the one that matters for a thread-per-core runtime:
//! executors that share nothing at the application level should not slow each
//! other down, so growth there is the runtime serialising cores against each
//! other.

mod common;

use common::Glommio;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use glommio::{spawn_local, LocalExecutor};
use std::{
    hint::black_box,
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};

fn spawn(c: &mut Criterion) {
    let ex = Glommio::default();
    let mut group = c.benchmark_group("spawn");

    group.bench_function("spawn_local, awaited", |b| {
        b.to_async(&ex)
            .iter(|| async { black_box(spawn_local(async { 1u32 }).await) })
    });

    group.bench_function("spawn_local, detached", |b| {
        b.to_async(&ex).iter(|| async {
            // Not awaited on purpose: the cost being measured is spawn plus
            // detach, not the task running to completion.
            let handle = spawn_local(async { 1u32 }).detach();
            black_box(handle);
        })
    });

    // A capture large enough that the task is boxed rather than stored inline.
    group.bench_function("spawn_local, 512B capture", |b| {
        b.to_async(&ex).iter(|| async {
            let payload = [0u8; 512];
            black_box(spawn_local(async move { payload[0] }).await)
        })
    });

    group.finish();
}

/// Per-spawn cost as a function of how many executors are spawning at once.
///
/// Timed by hand through `iter_custom` because the quantity is the wall time of
/// a round across every thread, which criterion's per-iteration timing cannot
/// express.
fn spawn_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("spawn_scaling");

    for threads in [1usize, 2, 4, 8, 16, 32, 64] {
        group.throughput(Throughput::Elements(threads as u64));
        group.bench_function(BenchmarkId::from_parameter(threads), |b| {
            b.iter_custom(|iters| {
                let barrier = Arc::new(Barrier::new(threads));
                let handles: Vec<_> = (0..threads)
                    .map(|_| {
                        let barrier = barrier.clone();
                        std::thread::spawn(move || {
                            let ex = LocalExecutor::default();
                            ex.run(async move {
                                // Warm the allocator and the task pool before
                                // the measured window opens.
                                for _ in 0..1_000 {
                                    black_box(spawn_local(async { 1u32 }).await);
                                }
                                barrier.wait();

                                let started = Instant::now();
                                for _ in 0..iters {
                                    black_box(spawn_local(async { 1u32 }).await);
                                }
                                started.elapsed()
                            })
                        })
                    })
                    .collect();

                // The slowest thread is the round: a fast one that finished
                // early was not contending for the rest of the window.
                handles
                    .into_iter()
                    .map(|h| h.join().expect("executor thread"))
                    .max()
                    .unwrap_or(Duration::ZERO)
            })
        });
    }

    group.finish();
}

criterion_group!(benches, spawn, spawn_scaling);
criterion_main!(benches);
