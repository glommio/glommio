//! That a task queue is preempted, and that two of them share the cpu.
//!
//! The preempt timer is armed by the reactor and kept armed while the same
//! duration is wanted. Both halves of that need holding down: a timer that is
//! never rearmed stops preempting, and one that is rearmed too eagerly never
//! fires. These read the behaviour through the public API rather than the
//! reactor's counters, so they say what a user would see.

use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
use glommio::{
    executor,
    net::{TcpListener, TcpStream},
    spawn_local, spawn_local_into, Latency, LocalExecutorBuilder, Placement, Shares,
};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

/// Short enough to keep the tests quick, long enough that a loop doing I/O
/// crosses it many times over.
const PREEMPT: Duration = Duration::from_millis(10);
const RUN: Duration = Duration::from_millis(400);

/// Connects to an echo server on this executor and hands back the client end.
async fn echo_pair() -> (TcpStream, glommio::Task<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = spawn_local(async move {
        let mut stream = listener.accept().await.unwrap();
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte).await.unwrap() {
                0 => break,
                _ => stream.write_all(&byte).await.unwrap(),
            }
        }
    });
    (TcpStream::connect(addr).await.unwrap(), server)
}

/// A queue doing nothing but I/O still gets preempted.
///
/// Every round trip takes the reactor through `wait`, which is where the
/// preempt timer is decided. Rearming it there resets the deadline, so a
/// timer that is reinstalled on every call can never reach it and the queue
/// runs unpreempted for as long as it keeps doing I/O.
#[test]
fn a_queue_doing_continuous_io_is_preempted() {
    let preempted = LocalExecutorBuilder::new(Placement::Unbound)
        .preempt_timer(PREEMPT)
        .spawn(|| async move {
            let (mut client, server) = echo_pair().await;
            let mut byte = [0u8; 1];
            let start = Instant::now();
            let mut preempted = false;

            while start.elapsed() < RUN {
                client.write_all(b"x").await.unwrap();
                client.read_exact(&mut byte).await.unwrap();
                if executor().need_preempt() {
                    preempted = true;
                    break;
                }
            }

            client.close().await.unwrap();
            server.await;
            preempted
        })
        .unwrap()
        .join()
        .unwrap();

    assert!(
        preempted,
        "a queue doing continuous io ran for {RUN:?} against a {PREEMPT:?} preempt timer \
         without ever being preempted"
    );
}

/// Two equally weighted queues doing I/O get equal turns.
#[test]
fn two_equal_queues_share_the_cpu() {
    let (trips, gaps) = LocalExecutorBuilder::new(Placement::Unbound)
        .preempt_timer(PREEMPT)
        .spawn(|| async move {
            let mut pairs = Vec::new();
            for _ in 0..2 {
                pairs.push(echo_pair().await);
            }

            let results: Rc<RefCell<Vec<(u64, u128)>>> = Rc::new(RefCell::new(Vec::new()));
            let mut running = Vec::new();
            let mut servers = Vec::new();

            for (index, (mut client, server)) in pairs.into_iter().enumerate() {
                servers.push(server);
                let queue = executor().create_task_queue(
                    Shares::Static(1000),
                    Latency::NotImportant,
                    if index == 0 { "q0" } else { "q1" },
                );
                let results = results.clone();
                running.push(
                    spawn_local_into(
                        async move {
                            let mut byte = [0u8; 1];
                            let start = Instant::now();
                            let (mut trips, mut longest_gap) = (0u64, 0u128);
                            let mut last = Instant::now();
                            while start.elapsed() < RUN {
                                client.write_all(b"x").await.unwrap();
                                client.read_exact(&mut byte).await.unwrap();
                                trips += 1;
                                longest_gap = longest_gap.max(last.elapsed().as_micros());
                                last = Instant::now();
                            }
                            client.close().await.unwrap();
                            results.borrow_mut().push((trips, longest_gap));
                        },
                        queue,
                    )
                    .unwrap(),
                );
            }

            for task in running {
                task.await;
            }
            for server in servers {
                server.await;
            }

            let done = results.borrow();
            let trips: Vec<u64> = done.iter().map(|(t, _)| *t).collect();
            let gaps: Vec<u128> = done.iter().map(|(_, g)| *g).collect();
            (trips, gaps)
        })
        .unwrap()
        .join()
        .unwrap();

    let (low, high) = (
        *trips.iter().min().unwrap() as f64,
        *trips.iter().max().unwrap() as f64,
    );
    assert!(low > 0.0, "a queue got no turns at all: {trips:?}");

    // A loose bound on purpose. The point is that neither queue is shut out,
    // not that the split is exact: this runs on whatever CI machine is free,
    // and a tight ratio here would report load as a scheduling regression.
    assert!(
        low / high > 0.5,
        "two equally weighted queues split the cpu {:.3}, which is not a share: {trips:?}",
        low / high
    );

    assert!(
        gaps.iter().all(|&g| g < RUN.as_micros()),
        "a queue waited its whole run for a turn: {gaps:?}us"
    );
}
