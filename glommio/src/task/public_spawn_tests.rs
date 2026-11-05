//! Public spawning before an executor runs and while another executor runs.

use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
    thread,
};

use super::test_support::{executor, AllocationProbe, DropGuard, DropProbe};

struct PendingFuture<const N: usize> {
    waker: Rc<RefCell<Option<Waker>>>,
    polls: Rc<Cell<usize>>,
    _guard: DropGuard,
    _padding: [u8; N],
}

impl<const N: usize> PendingFuture<N> {
    fn new(guard: DropGuard) -> Self {
        let future = Self {
            waker: Rc::default(),
            polls: Rc::default(),
            _guard: guard,
            _padding: [0; N],
        };
        assert_eq!(std::mem::size_of_val(&future) >= 2048, N >= 2048);
        future
    }
}

impl<const N: usize> Future for PendingFuture<N> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.polls.set(self.polls.get() + 1);
        *self.waker.borrow_mut() = Some(cx.waker().clone());
        Poll::Pending
    }
}

enum LateWakerAction {
    Drop,
    Wake,
    WakeByRef,
}

fn no_run_shutdown<const N: usize>(foreign: bool, action: LateWakerAction) {
    let owner = executor();
    let future_drops = DropProbe::new();
    let future = PendingFuture::<N>::new(future_drops.guard());
    let saved_waker = future.waker.clone();
    let polls = future.polls.clone();
    let handle = owner.spawn(future).detach();
    let allocation = AllocationProbe::track_handle(&handle);
    let waker = saved_waker
        .borrow_mut()
        .take()
        .expect("spawn did not poll the future");

    assert_eq!(polls.get(), 1);
    drop(handle);
    future_drops.assert_not_dropped();
    drop(owner);
    future_drops.assert_dropped_once();
    allocation.assert_live();

    let release = move || match action {
        LateWakerAction::Drop => drop(waker),
        LateWakerAction::Wake => waker.wake(),
        LateWakerAction::WakeByRef => {
            let allocation = AllocationProbe::track_waker(&waker);
            waker.wake_by_ref();
            allocation.assert_live();
            drop(waker);
        }
    };
    if foreign {
        thread::spawn(release)
            .join()
            .expect("late foreign waker operation panicked");
    } else {
        release();
    }

    assert_eq!(polls.get(), 1, "shutdown task was polled again");
    future_drops.assert_dropped_once();
    allocation.assert_freed();
}

fn cross_executor_spawn<const N: usize>() {
    let owner = executor();
    let other = executor();
    let future_drops = DropProbe::new();
    let future = PendingFuture::<N>::new(future_drops.guard());
    let saved_waker = future.waker.clone();
    let polls = future.polls.clone();
    let (handle, allocation) = other.run(async {
        let handle = owner.spawn(future).detach();
        let allocation = AllocationProbe::track_handle(&handle);
        assert_eq!(crate::executor().id(), other.id());
        (handle, allocation)
    });
    let waker = saved_waker
        .borrow_mut()
        .take()
        .expect("spawn did not poll the future");

    drop(other);
    future_drops.assert_not_dropped();
    allocation.assert_live();
    drop(owner);
    future_drops.assert_dropped_once();
    assert_eq!(polls.get(), 1);
    assert!(futures_lite::future::block_on(handle).is_none());
    allocation.assert_live();
    drop(waker);
    allocation.assert_freed();
}

struct CompletedFuture<const N: usize> {
    output: Option<DropGuard>,
    _guard: DropGuard,
    _padding: [u8; N],
}

impl<const N: usize> Future for CompletedFuture<N> {
    type Output = DropGuard;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<DropGuard> {
        Poll::Ready(
            self.get_mut()
                .output
                .take()
                .expect("completed future was polled again"),
        )
    }
}

fn completed_output_after_shutdown<const N: usize>() {
    for consume in [false, true] {
        let owner = executor();
        let future_drops = DropProbe::new();
        let output_drops = DropProbe::new();
        let future = CompletedFuture::<N> {
            output: Some(output_drops.guard()),
            _guard: future_drops.guard(),
            _padding: [0; N],
        };
        assert_eq!(std::mem::size_of_val(&future) >= 2048, N >= 2048);
        let handle = owner.spawn(future).detach();
        let allocation = AllocationProbe::track_handle(&handle);

        future_drops.assert_dropped_once();
        output_drops.assert_not_dropped();
        drop(owner);
        output_drops.assert_not_dropped();
        allocation.assert_live();

        if consume {
            let output = futures_lite::future::block_on(handle)
                .expect("completed output was lost during shutdown");
            allocation.assert_freed();
            output_drops.assert_not_dropped();
            drop(output);
        } else {
            drop(handle);
        }
        future_drops.assert_dropped_once();
        output_drops.assert_dropped_once();
        allocation.assert_freed();
    }
}

/// Dropping a `ScopedTask` must drop its future before `drop` returns: the
/// future may borrow from the scope that is ending.
fn dropped_scoped_task_drops_its_future<const N: usize>(queued: bool) {
    executor().run(async {
        let future_drops = DropProbe::new();
        let future = PendingFuture::<N>::new(future_drops.guard());
        let saved_waker = future.waker.clone();
        let polls = future.polls.clone();
        let task = unsafe {
            if queued {
                crate::spawn_scoped_local_into(future, crate::executor().current_task_queue())
                    .unwrap()
            } else {
                crate::spawn_scoped_local(future)
            }
        };
        let polled = usize::from(!queued);
        assert_eq!(polls.get(), polled);

        drop(task);
        future_drops.assert_dropped_once();
        if let Some(waker) = saved_waker.borrow_mut().take() {
            waker.wake();
        }
        futures_lite::future::yield_now().await;
        future_drops.assert_dropped_once();
        assert_eq!(polls.get(), polled);
    });
}

/// `cancel()` must not return before the future is dropped, nor leave it
/// behind if the `cancel()` future is itself dropped.
#[test]
fn cancelled_scoped_task_drops_its_future() {
    executor().run(async {
        let future_drops = DropProbe::new();
        let task =
            unsafe { crate::spawn_scoped_local(PendingFuture::<0>::new(future_drops.guard())) };
        let output = futures_lite::future::poll_once(task.cancel()).await;
        future_drops.assert_dropped_once();
        assert_eq!(output, Some(None));
    });
}

/// A future cannot be dropped while it is being polled, so a task that drops
/// its own `ScopedTask` has its future dropped once that poll returns.
#[test]
fn scoped_task_dropped_by_its_own_future() {
    executor().run(async {
        let future_drops = DropProbe::new();
        let guard = future_drops.guard();
        let probe = future_drops.clone();
        let slot: Rc<RefCell<Option<crate::ScopedTask<'_, ()>>>> = Rc::default();
        let inner = slot.clone();
        let task = unsafe {
            crate::spawn_scoped_local_into(
                async move {
                    let _guard = guard;
                    drop(inner.borrow_mut().take());
                    probe.assert_not_dropped();
                    std::future::pending::<()>().await;
                },
                crate::executor().current_task_queue(),
            )
            .unwrap()
        };
        *slot.borrow_mut() = Some(task);
        futures_lite::future::yield_now().await;
        assert!(slot.borrow().is_none(), "the task did not run");
        future_drops.assert_dropped_once();
    });
}

macro_rules! test_sizes {
    ($inline:ident, $boxed:ident, $body:ident $(, $arg:expr)*) => {
        #[test]
        fn $inline() { $body::<0>($($arg),*); }
        #[test]
        fn $boxed() { $body::<4096>($($arg),*); }
    };
}

test_sizes!(
    no_run_owner_drop_inline,
    no_run_owner_drop_boxed,
    no_run_shutdown,
    false,
    LateWakerAction::Drop
);
test_sizes!(
    no_run_foreign_drop_inline,
    no_run_foreign_drop_boxed,
    no_run_shutdown,
    true,
    LateWakerAction::Drop
);
test_sizes!(
    no_run_owner_wake_inline,
    no_run_owner_wake_boxed,
    no_run_shutdown,
    false,
    LateWakerAction::Wake
);
test_sizes!(
    no_run_foreign_wake_inline,
    no_run_foreign_wake_boxed,
    no_run_shutdown,
    true,
    LateWakerAction::Wake
);
test_sizes!(
    no_run_owner_wake_by_ref_inline,
    no_run_owner_wake_by_ref_boxed,
    no_run_shutdown,
    false,
    LateWakerAction::WakeByRef
);
test_sizes!(
    no_run_foreign_wake_by_ref_inline,
    no_run_foreign_wake_by_ref_boxed,
    no_run_shutdown,
    true,
    LateWakerAction::WakeByRef
);
test_sizes!(
    cross_executor_spawn_inline,
    cross_executor_spawn_boxed,
    cross_executor_spawn
);
test_sizes!(
    completed_output_after_shutdown_inline,
    completed_output_after_shutdown_boxed,
    completed_output_after_shutdown
);
test_sizes!(
    dropped_polled_scoped_task_inline,
    dropped_polled_scoped_task_boxed,
    dropped_scoped_task_drops_its_future,
    false
);
test_sizes!(
    dropped_queued_scoped_task_inline,
    dropped_queued_scoped_task_boxed,
    dropped_scoped_task_drops_its_future,
    true
);
