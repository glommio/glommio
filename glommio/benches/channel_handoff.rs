//! What a channel hand-off costs: across cores through shared channels, and
//! between two tasks on one executor.
//!
//! Each case settles the premise of a pending change to the messaging or wake
//! path, and says which one on its function. `shared_channel.rs` and
//! `local_channel.rs` cover similar ground with hand-written timing loops,
//! which are fine for settling a premise but give no spread to compare arms
//! by.
//!
//! Cross-core cases pin each executor with `Placement::Fixed`, using CPU 0 for
//! the receiving side and CPUs 1 and up for senders. Linux usually numbers SMT
//! siblings apart (0 and 32 on a 32-core part), so these are distinct physical
//! cores. A case that needs more CPUs than the machine has is skipped.
//!
//! The cross-core cases build fresh executors for every sample and exchange
//! `WARM_UP` messages before the timed window opens, so thread start, io_uring
//! setup and channel connection stay outside it. A `Barrier` lines the
//! executors up so that every timed loop starts together, and the slowest one
//! is the sample.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use glommio::{
    channels::{
        channel_mesh::{MeshBuilder, Role},
        local_channel, shared_channel,
    },
    enclose, spawn_local, ExecutorJoinHandle, LocalExecutorBuilder, Placement,
};
use std::{
    future::Future,
    hint::black_box,
    rc::Rc,
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};

/// Messages exchanged before each timed window, so both ends run hot and the
/// executors have parked and woken at least once.
const WARM_UP: u64 = 2_000;

/// The CPU every receiving executor is pinned to.
const RECEIVER_CPU: usize = 0;

/// Senders in the fan-in case, each on its own core.
const FAN_IN_PRODUCERS: usize = 4;

/// Whether this machine has `n` CPUs to pin to, reporting the skip if not.
fn have_cpus(case: &str, n: usize) -> bool {
    let available = std::thread::available_parallelism().map_or(1, |n| n.get());
    if available < n {
        eprintln!("skipping {case}: needs {n} CPUs, {available} available");
    }
    available >= n
}

/// Runs `fut_gen` on a new executor pinned to `cpu`.
fn pinned<G, F, T>(cpu: usize, fut_gen: G) -> ExecutorJoinHandle<T>
where
    G: FnOnce() -> F + Send + 'static,
    F: Future<Output = T> + 'static,
    T: Send + 'static,
{
    LocalExecutorBuilder::new(Placement::Fixed(cpu))
        .name(&format!("bench-cpu{cpu}"))
        .spawn(fut_gen)
        .expect("pinned executor")
}

/// The slowest of the timed windows the executors report.
fn slowest(handles: Vec<ExecutorJoinHandle<Duration>>) -> Duration {
    handles
        .into_iter()
        .map(|h| h.join().expect("executor thread"))
        .max()
        .unwrap_or(Duration::ZERO)
}

/// One sender on CPU 1 streams `iters` messages to one receiver on CPU 0.
fn shared_one_way(capacity: usize, iters: u64) -> Duration {
    let (tx, rx) = shared_channel::new_bounded::<u64>(capacity);
    let barrier = Arc::new(Barrier::new(2));

    let producer = pinned(
        RECEIVER_CPU + 1,
        enclose! {(barrier) move || async move {
            let tx = tx.connect().await;
            for i in 0..WARM_UP {
                tx.send(i).await.unwrap();
            }
            barrier.wait();
            let started = Instant::now();
            for i in 0..iters {
                tx.send(i).await.unwrap();
            }
            started.elapsed()
        }},
    );

    let consumer = pinned(RECEIVER_CPU, move || async move {
        let rx = rx.connect().await;
        for _ in 0..WARM_UP {
            rx.recv().await.unwrap();
        }
        barrier.wait();
        let started = Instant::now();
        for _ in 0..iters {
            black_box(rx.recv().await.unwrap());
        }
        started.elapsed()
    });

    slowest(vec![producer, consumer])
}

/// One executor on CPU 1 sends to CPU 0 and waits for the echo, `iters` times.
fn shared_round_trip(iters: u64) -> Duration {
    let (ping_tx, ping_rx) = shared_channel::new_bounded::<u64>(1);
    let (pong_tx, pong_rx) = shared_channel::new_bounded::<u64>(1);
    let barrier = Arc::new(Barrier::new(2));

    let client = pinned(
        RECEIVER_CPU + 1,
        enclose! {(barrier) move || async move {
            let ping = ping_tx.connect().await;
            let pong = pong_rx.connect().await;
            for i in 0..WARM_UP {
                ping.send(i).await.unwrap();
                pong.recv().await.unwrap();
            }
            barrier.wait();
            let started = Instant::now();
            for i in 0..iters {
                ping.send(i).await.unwrap();
                black_box(pong.recv().await.unwrap());
            }
            started.elapsed()
        }},
    );

    let echo = pinned(RECEIVER_CPU, move || async move {
        let ping = ping_rx.connect().await;
        let pong = pong_tx.connect().await;
        for _ in 0..WARM_UP {
            pong.send(ping.recv().await.unwrap()).await.unwrap();
        }
        barrier.wait();
        let started = Instant::now();
        for _ in 0..iters {
            pong.send(ping.recv().await.unwrap()).await.unwrap();
        }
        started.elapsed()
    });

    slowest(vec![client, echo])
}

/// `FAN_IN_PRODUCERS` executors on CPUs 1 and up each send `iters` messages
/// through a partial mesh to one consumer on CPU 0, which reads each producer
/// from its own task.
fn mesh_fan_in(capacity: usize, iters: u64) -> Duration {
    let mesh = MeshBuilder::<u64, _>::partial(FAN_IN_PRODUCERS + 1, capacity);
    let barrier = Arc::new(Barrier::new(FAN_IN_PRODUCERS + 1));

    let mut handles: Vec<_> = (1..=FAN_IN_PRODUCERS)
        .map(|cpu| {
            pinned(
                RECEIVER_CPU + cpu,
                enclose! {(mesh, barrier) move || async move {
                    let (senders, _) = mesh.join(Role::Producer).await.unwrap();
                    assert_eq!(senders.nr_consumers(), 1);
                    for i in 0..WARM_UP {
                        senders.send_to(0, i).await.unwrap();
                    }
                    barrier.wait();
                    let started = Instant::now();
                    for i in 0..iters {
                        senders.send_to(0, i).await.unwrap();
                    }
                    started.elapsed()
                }},
            )
        })
        .collect();

    handles.push(pinned(RECEIVER_CPU, move || async move {
        let (_, mut receivers) = mesh.join(Role::Consumer).await.unwrap();
        let drain = |count: u64, streams: Vec<_>| {
            streams
                .into_iter()
                .map(|rx: shared_channel::ConnectedReceiver<u64>| {
                    spawn_local(async move {
                        for _ in 0..count {
                            black_box(rx.recv().await.unwrap());
                        }
                        rx
                    })
                })
                .collect::<Vec<_>>()
        };

        let streams = receivers.streams().into_iter().map(|(_, rx)| rx).collect();
        let mut streams_back = Vec::with_capacity(FAN_IN_PRODUCERS);
        for task in drain(WARM_UP, streams) {
            streams_back.push(task.await);
        }
        barrier.wait();
        let started = Instant::now();
        for task in drain(iters, streams_back) {
            task.await;
        }
        started.elapsed()
    }));

    slowest(handles)
}

/// Shared channel throughput between two cores: one message per iteration
/// one way, two (there and back) for the round trip.
///
/// Capacity 1024 is the busy pipe, where neither side parks. It decides the
/// load-before-CAS in `SleepNotifier::notify` and the `try_send`-first
/// `ConnectedSender::send`, each of which removes a cross-core RMW or read
/// from every message. Capacity 16 is the control: the sender parks on most
/// refills and an eventfd write dominates, so neither change should move it.
/// The round trip is the latency regime: each hop usually finds the peer
/// parked and wakes it through its eventfd, which is the cost those changes
/// must not raise.
fn shared_channel_cross_core(c: &mut Criterion) {
    if !have_cpus("shared_channel_cross_core", RECEIVER_CPU + 2) {
        return;
    }
    let mut group = c.benchmark_group("shared_channel_cross_core");

    group.throughput(Throughput::Elements(1));
    for capacity in [16usize, 1024] {
        group.bench_function(BenchmarkId::new("one_way", capacity), |b| {
            b.iter_custom(|iters| shared_one_way(capacity, iters))
        });
    }

    group.throughput(Throughput::Elements(2));
    group.bench_function("round_trip", |b| b.iter_custom(shared_round_trip));

    group.finish();
}

/// Throughput of four producers on four cores into one consumer, counted as
/// four messages per iteration.
///
/// Every message writes the consumer's `SleepNotifier` line from one of the
/// producers' cores, so this is where padding `should_notify` onto its own
/// line could show, and where the load-before-CAS in `notify` keeps that line
/// shared instead of bouncing it. Run it with and without each.
fn shared_channel_fan_in(c: &mut Criterion) {
    if !have_cpus("shared_channel_fan_in", RECEIVER_CPU + FAN_IN_PRODUCERS + 1) {
        return;
    }
    let mut group = c.benchmark_group("shared_channel_fan_in");

    group.throughput(Throughput::Elements(FAN_IN_PRODUCERS as u64));
    group.bench_function(BenchmarkId::new("mesh_4_to_1", 1024), |b| {
        b.iter_custom(|iters| mesh_fan_in(1024, iters))
    });

    group.finish();
}

/// Two tasks on one executor passing messages through a local channel, one
/// message per iteration.
///
/// At capacity 1 every send wakes the parked receiver and every receive wakes
/// the parked sender, so the iteration is two task wakes. It decides whether
/// `RawTask::wake` should hand its reference to the scheduled task instead of
/// taking a new one and dropping its own. Capacity 1024 is the control, where
/// wakes are rare and the change should not show.
///
/// `make` pins the calling thread, so this group runs last.
fn local_channel_contended(c: &mut Criterion) {
    let ex = LocalExecutorBuilder::new(Placement::Fixed(RECEIVER_CPU))
        .make()
        .expect("pinned executor");
    let mut group = c.benchmark_group("local_channel");

    group.throughput(Throughput::Elements(1));
    for capacity in [1usize, 1024] {
        let case = if capacity == 1 {
            "contended"
        } else {
            "uncontended"
        };
        group.bench_function(BenchmarkId::new(case, capacity), |b| {
            b.iter_custom(|iters| {
                ex.run(async move {
                    let (tx, rx) = local_channel::new_bounded::<u64>(capacity);
                    let rx = Rc::new(rx);
                    let drain = |count: u64| {
                        let rx = rx.clone();
                        spawn_local(async move {
                            for _ in 0..count {
                                black_box(rx.recv().await.unwrap());
                            }
                        })
                    };

                    let warm = drain(WARM_UP);
                    for i in 0..WARM_UP {
                        tx.send(i).await.unwrap();
                    }
                    warm.await;

                    let started = Instant::now();
                    let measured = drain(iters);
                    for i in 0..iters {
                        tx.send(i).await.unwrap();
                    }
                    measured.await;
                    started.elapsed()
                })
            })
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    shared_channel_cross_core,
    shared_channel_fan_in,
    local_channel_contended
);
criterion_main!(benches);
