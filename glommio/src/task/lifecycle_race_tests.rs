//! Concurrent waker operations around task completion and executor shutdown.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    task::{Context, Poll, Waker},
    thread::{self, JoinHandle as ThreadHandle},
    time::Duration,
};

use super::test_support::{completed_waker, executor, AllocationProbe, DropGuard, DropProbe};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const ROUNDS: usize = 16;
const WORKERS: usize = 4;

fn join_workers(workers: Vec<ThreadHandle<()>>) {
    // Join all workers even if one panicked, before inspecting any task state.
    let outcomes: Vec<_> = workers.into_iter().map(ThreadHandle::join).collect();
    for outcome in outcomes {
        outcome.expect("foreign waker worker panicked");
    }
}

fn exercise_waker(waker: Waker, worker: usize, notify: bool) {
    for operation in 0..16 {
        let clone = waker.clone();
        if notify {
            clone.wake_by_ref();
        }
        if notify && operation % 2 == 0 {
            clone.wake();
        } else {
            drop(clone);
        }
    }
    if notify && worker.is_multiple_of(2) {
        waker.wake();
    } else {
        drop(waker);
    }
}

fn ordered_foreign_wakes<const N: usize>(shutdown_first: bool) {
    let executor = executor();
    let (waker, allocation, future_drop) = completed_waker::<N>(&executor);
    let (ready_tx, ready_rx) = mpsc::channel();
    let (start_tx, start_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        ready_tx.send(()).expect("owner dropped readiness receiver");
        start_rx
            .recv_timeout(HANDSHAKE_TIMEOUT)
            .expect("owner did not release foreign waker worker");
        exercise_waker(waker, 0, true);
    });
    ready_rx
        .recv_timeout(HANDSHAKE_TIMEOUT)
        .expect("foreign waker worker did not become ready");

    let mut executor = Some(executor);
    if shutdown_first {
        drop(executor.take());
    }
    let freed_before_release = allocation.deallocations();
    start_tx.send(()).expect("foreign waker worker exited");
    join_workers(vec![worker]);
    // In the other ordering, all notifications are queued before shutdown.
    // There is deliberately no second run() to drain them.
    drop(executor);

    assert_eq!(freed_before_release, 0, "live waker lost its allocation");
    future_drop.assert_dropped_once();
    allocation.assert_freed();
}

#[test]
fn foreign_wakes_queued_before_shutdown_inline() {
    ordered_foreign_wakes::<0>(false);
}

#[test]
fn foreign_wakes_queued_before_shutdown_boxed() {
    ordered_foreign_wakes::<4096>(false);
}

#[test]
fn foreign_wakes_released_after_shutdown_inline() {
    ordered_foreign_wakes::<0>(true);
}

#[test]
fn foreign_wakes_released_after_shutdown_boxed() {
    ordered_foreign_wakes::<4096>(true);
}

struct WorkerStart {
    wakers: Vec<Sender<Waker>>,
    ready: Receiver<()>,
    starts: Vec<Sender<()>>,
}

impl WorkerStart {
    fn release(self, waker: &Waker) {
        for sender in self.wakers {
            sender
                .send(waker.clone())
                .expect("foreign waker worker exited before receiving its waker");
        }
        for _ in 0..WORKERS {
            self.ready
                .recv_timeout(HANDSHAKE_TIMEOUT)
                .expect("foreign waker worker did not become ready");
        }
        for sender in self.starts {
            sender.send(()).expect("foreign waker worker exited");
        }
    }
}

fn waiting_workers(notify: bool) -> (WorkerStart, Vec<ThreadHandle<()>>) {
    let (ready_tx, ready_rx) = mpsc::channel();
    let mut wakers = Vec::new();
    let mut starts = Vec::new();
    let mut workers = Vec::new();
    for worker in 0..WORKERS {
        let (waker_tx, waker_rx) = mpsc::channel();
        let (start_tx, start_rx) = mpsc::channel();
        let ready_tx = ready_tx.clone();
        workers.push(thread::spawn(move || {
            let waker = waker_rx
                .recv_timeout(HANDSHAKE_TIMEOUT)
                .expect("owner did not send a task waker");
            ready_tx.send(()).expect("owner dropped readiness receiver");
            start_rx
                .recv_timeout(HANDSHAKE_TIMEOUT)
                .expect("owner did not release foreign waker worker");
            exercise_waker(waker, worker, notify);
        }));
        wakers.push(waker_tx);
        starts.push(start_tx);
    }
    (
        WorkerStart {
            wakers,
            ready: ready_rx,
            starts,
        },
        workers,
    )
}

fn assert_rounds_freed(observations: Vec<(AllocationProbe, DropProbe)>) {
    for (round, (allocation, future_drop)) in observations.into_iter().enumerate() {
        future_drop.assert_dropped_once();
        assert_eq!(
            allocation.deallocations(),
            1,
            "round {round}: task allocation was not freed exactly once",
        );
    }
}

fn race_completed_task_shutdown<const N: usize>() {
    let mut observations = Vec::new();
    for round in 0..ROUNDS {
        let executor = executor();
        let (waker, allocation, future_drop) = completed_waker::<N>(&executor);
        // Drop-only rounds also race final references without queued wakes
        // keeping the task alive.
        let (start, workers) = waiting_workers(round % 2 == 0);

        // The start handshakes allow operations to race shutdown. They do not
        // force any particular interleaving inside the runtime.
        start.release(&waker);
        drop(waker);
        drop(executor);
        join_workers(workers);
        observations.push((allocation, future_drop));
    }
    assert_rounds_freed(observations);
}

#[test]
fn concurrent_foreign_wakes_and_shutdown_inline() {
    race_completed_task_shutdown::<0>();
}

#[test]
fn concurrent_foreign_wakes_and_shutdown_boxed() {
    race_completed_task_shutdown::<4096>();
}

struct CompletingFuture<const N: usize> {
    start: Option<WorkerStart>,
    polls: Arc<AtomicUsize>,
    _guard: DropGuard,
    _padding: [u8; N],
}

impl<const N: usize> Future for CompletingFuture<N> {
    type Output = AllocationProbe;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.polls.fetch_add(1, Ordering::SeqCst);
        let allocation = AllocationProbe::track_waker(cx.waker());
        this.start
            .take()
            .expect("completed task was polled again")
            .release(cx.waker());
        // Foreign operations may now overlap completion, future destruction,
        // and run()'s release of the root task's join handle.
        Poll::Ready(allocation)
    }
}

fn race_completion_and_handle_release<const N: usize>() {
    assert_eq!(
        std::mem::size_of::<CompletingFuture<N>>() >= 2048,
        N >= 2048
    );
    let mut observations = Vec::new();
    for round in 0..ROUNDS {
        let executor = executor();
        let future_drop = DropProbe::new();
        let polls = Arc::new(AtomicUsize::new(0));
        let (start, workers) = waiting_workers(round % 2 == 0);
        let future = CompletingFuture {
            start: Some(start),
            polls: polls.clone(),
            _guard: future_drop.guard(),
            _padding: [0; N],
        };
        let allocation = executor.run(future);
        join_workers(workers);
        // No further run() may be necessary to reclaim a completed task.
        drop(executor);
        assert_eq!(polls.load(Ordering::SeqCst), 1, "completed task repolled");
        observations.push((allocation, future_drop));
    }
    assert_rounds_freed(observations);
}

#[test]
fn concurrent_foreign_wakes_completion_and_handle_release_inline() {
    race_completion_and_handle_release::<0>();
}

#[test]
fn concurrent_foreign_wakes_completion_and_handle_release_boxed() {
    race_completion_and_handle_release::<4096>();
}
