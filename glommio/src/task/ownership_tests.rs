// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience.

//! Lifetimes when runnable ownership moves through scheduling callbacks.

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use super::{
    task_impl,
    test_support::{executor, AllocationProbe, DropGuard, DropProbe},
    Task,
};

struct OwnedFuture<const N: usize> {
    output: Option<DropGuard>,
    polls: Rc<Cell<usize>>,
    pending: usize,
    panic_on_poll: bool,
    _guard: DropGuard,
    _padding: [u8; N],
}

impl<const N: usize> Future for OwnedFuture<N> {
    type Output = DropGuard;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.set(self.polls.get() + 1);
        assert!(!self.panic_on_poll, "intentional future panic");
        if self.pending > 0 {
            self.pending -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(self.output.take().expect("future polled after completion"))
        }
    }
}

fn future<const N: usize>(
    future_drops: &DropProbe,
    output_drops: &DropProbe,
    pending: usize,
    panic_on_poll: bool,
) -> (OwnedFuture<N>, Rc<Cell<usize>>) {
    let polls = Rc::new(Cell::new(0));
    let future = OwnedFuture {
        output: Some(output_drops.guard()),
        polls: polls.clone(),
        pending,
        panic_on_poll,
        _guard: future_drops.guard(),
        _padding: [0; N],
    };
    assert_eq!(std::mem::size_of_val(&future) >= 2048, N >= 2048);
    (future, polls)
}

fn drop_initial_runnable<const N: usize>(drop_handle_first: bool) {
    let ex = executor();
    let future_drops = DropProbe::new();
    let output_drops = DropProbe::new();
    let schedule_drops = DropProbe::new();
    let schedule_guard = schedule_drops.guard();
    let (future, polls) = future::<N>(&future_drops, &output_drops, 0, false);
    let scheduled = Rc::new(RefCell::new(None));
    let schedule_slot = scheduled.clone();
    let allocation = ex.run(async {
        let (task, handle) = task_impl::spawn_local(
            ex.id(),
            &ex.tasks,
            future,
            move |task| {
                let _ = &schedule_guard;
                *schedule_slot.borrow_mut() = Some(task);
            },
            false,
        );
        let allocation = AllocationProbe::track_handle(&handle);
        task.schedule();
        let task = scheduled
            .borrow_mut()
            .take()
            .expect("initial runnable was not scheduled");
        if drop_handle_first {
            drop(handle);
            allocation.assert_live();
            drop(task);
        } else {
            drop(task);
            future_drops.assert_dropped_once();
            output_drops.assert_dropped_once();
            allocation.assert_live();
            assert!(handle.await.is_none());
        }
        allocation.assert_freed();
        allocation
    });
    assert_eq!(polls.get(), 0);
    future_drops.assert_dropped_once();
    output_drops.assert_dropped_once();
    schedule_drops.assert_dropped_once();
    allocation.assert_freed();
}

fn run_initial_runnable<const N: usize>(drop_handle_first: bool, right_away: bool) {
    let ex = executor();
    let future_drops = DropProbe::new();
    let output_drops = DropProbe::new();
    let schedule_drops = DropProbe::new();
    let schedule_guard = schedule_drops.guard();
    let (future, polls) = future::<N>(&future_drops, &output_drops, 0, false);
    let run = if right_away {
        Task::run_right_away
    } else {
        Task::run
    };
    let scheduled = Rc::new(RefCell::new(None));
    let schedule_slot = scheduled.clone();
    let allocation = ex.run(async {
        let (task, handle) = task_impl::spawn_local(
            ex.id(),
            &ex.tasks,
            future,
            move |task| {
                let _ = &schedule_guard;
                *schedule_slot.borrow_mut() = Some(task);
            },
            false,
        );
        let allocation = AllocationProbe::track_handle(&handle);
        let task = if right_away {
            task
        } else {
            task.schedule();
            scheduled
                .borrow_mut()
                .take()
                .expect("initial runnable was not scheduled")
        };
        if drop_handle_first {
            drop(handle);
            assert!(!run(task));
        } else {
            assert!(!run(task));
            future_drops.assert_dropped_once();
            output_drops.assert_not_dropped();
            allocation.assert_live();
            let output = handle.await.expect("completed output was lost");
            allocation.assert_freed();
            output_drops.assert_not_dropped();
            drop(output);
        }
        allocation.assert_freed();
        allocation
    });
    assert_eq!(polls.get(), 1);
    future_drops.assert_dropped_once();
    output_drops.assert_dropped_once();
    schedule_drops.assert_dropped_once();
    allocation.assert_freed();
}

fn reschedule_queued_runnable<const N: usize>() {
    let ex = executor();
    let future_drops = DropProbe::new();
    let output_drops = DropProbe::new();
    let schedule_drops = DropProbe::new();
    let schedule_guard = schedule_drops.guard();
    let (future, polls) = future::<N>(&future_drops, &output_drops, 3, false);
    let queue = Rc::new(RefCell::new(VecDeque::new()));
    let schedule_queue = queue.clone();
    let allocation = ex.run(async {
        let (task, handle) = task_impl::spawn_local(
            ex.id(),
            &ex.tasks,
            future,
            move |task| {
                let _ = &schedule_guard;
                schedule_queue.borrow_mut().push_back(task);
            },
            false,
        );
        let allocation = AllocationProbe::track_handle(&handle);
        task.schedule();
        for _ in 0..4 {
            let task = queue.borrow_mut().pop_front().expect("runnable was lost");
            task.schedule();
            assert_eq!(queue.borrow().len(), 1);
            assert_eq!(polls.get(), 0);
        }
        for expected_poll in 1..=4 {
            let task = queue.borrow_mut().pop_front().expect("wake was lost");
            assert!(queue.borrow().is_empty());
            assert_eq!(task.run(), expected_poll < 4);
            assert_eq!(polls.get(), expected_poll);
        }
        assert!(queue.borrow().is_empty());
        future_drops.assert_dropped_once();
        allocation.assert_live();
        drop(handle.await.expect("completed output was lost"));
        allocation.assert_freed();
        allocation
    });
    future_drops.assert_dropped_once();
    output_drops.assert_dropped_once();
    schedule_drops.assert_dropped_once();
    allocation.assert_freed();
}

#[derive(Clone, Copy)]
enum Callback {
    Run,
    Reschedule,
    Drop,
    PanicAfterDrop,
    PollPanic,
}

fn captured_callback<const N: usize>(callback: Callback) {
    let ex = executor();
    let future_drops = DropProbe::new();
    let output_drops = DropProbe::new();
    let schedule_drops = DropProbe::new();
    let schedule_guard = schedule_drops.guard();
    let callback_drops = schedule_drops.clone();
    let (future, polls) = future::<N>(
        &future_drops,
        &output_drops,
        usize::from(matches!(callback, Callback::Reschedule)),
        matches!(callback, Callback::PollPanic),
    );
    let allocation = ex.run(async {
        let (task, handle) = task_impl::spawn_local(
            ex.id(),
            &ex.tasks,
            future,
            move |task| {
                // Keep an independent observer alive while consuming the last runnable.
                let callback_drops = callback_drops.clone();
                match callback {
                    Callback::Run | Callback::Reschedule | Callback::PollPanic => {
                        task.run();
                    }
                    Callback::Drop | Callback::PanicAfterDrop => drop(task),
                }
                callback_drops.assert_not_dropped();
                std::hint::black_box(&schedule_guard);
                if matches!(callback, Callback::PanicAfterDrop) {
                    panic!("intentional schedule panic");
                }
            },
            false,
        );
        let allocation = AllocationProbe::track_handle(&handle);
        drop(handle);
        let result = catch_unwind(AssertUnwindSafe(|| task.schedule()));
        match callback {
            Callback::Run | Callback::Reschedule | Callback::Drop => {
                result.expect("schedule callback panicked")
            }
            Callback::PanicAfterDrop | Callback::PollPanic => {
                let panic = result.expect_err("schedule callback did not propagate its panic");
                let expected = if matches!(callback, Callback::PollPanic) {
                    "intentional future panic"
                } else {
                    "intentional schedule panic"
                };
                assert_eq!(panic.downcast_ref::<&str>(), Some(&expected));
            }
        }
        allocation.assert_freed();
        allocation
    });
    assert_eq!(
        polls.get(),
        match callback {
            Callback::Reschedule => 2,
            Callback::Run | Callback::PollPanic => 1,
            Callback::Drop | Callback::PanicAfterDrop => 0,
        },
    );
    future_drops.assert_dropped_once();
    output_drops.assert_dropped_once();
    schedule_drops.assert_dropped_once();
    allocation.assert_freed();
}

fn zero_sized_callback<const N: usize, const RUN: bool>() {
    let ex = executor();
    let future_drops = DropProbe::new();
    let output_drops = DropProbe::new();
    let (future, polls) = future::<N>(&future_drops, &output_drops, 0, false);
    ex.run(async {
        let schedule = |task: Task| {
            if RUN {
                task.run();
            } else {
                drop(task);
            }
        };
        assert_eq!(std::mem::size_of_val(&schedule), 0);
        let (task, handle) = task_impl::spawn_local(ex.id(), &ex.tasks, future, schedule, false);
        let allocation = AllocationProbe::track_handle(&handle);
        drop(handle);
        task.schedule();
        allocation.assert_freed();
    });
    assert_eq!(polls.get(), usize::from(RUN));
    future_drops.assert_dropped_once();
    output_drops.assert_dropped_once();
}

macro_rules! both_layouts {
    ($inline:ident, $boxed:ident, $helper:ident $(, $arg:expr)*) => {
        #[test]
        fn $inline() {
            $helper::<0>($($arg),*);
        }

        #[test]
        fn $boxed() {
            $helper::<4096>($($arg),*);
        }
    };
}

both_layouts!(
    initial_drop_inline,
    initial_drop_boxed,
    drop_initial_runnable,
    false
);
both_layouts!(
    detached_initial_drop_inline,
    detached_initial_drop_boxed,
    drop_initial_runnable,
    true
);
both_layouts!(
    initial_run_inline,
    initial_run_boxed,
    run_initial_runnable,
    false,
    false
);
both_layouts!(
    detached_initial_run_inline,
    detached_initial_run_boxed,
    run_initial_runnable,
    true,
    false
);
both_layouts!(
    initial_run_right_away_inline,
    initial_run_right_away_boxed,
    run_initial_runnable,
    false,
    true
);
both_layouts!(
    queued_reschedule_inline,
    queued_reschedule_boxed,
    reschedule_queued_runnable
);
both_layouts!(
    captured_callback_run_inline,
    captured_callback_run_boxed,
    captured_callback,
    Callback::Run
);
both_layouts!(
    captured_callback_drop_inline,
    captured_callback_drop_boxed,
    captured_callback,
    Callback::Drop
);
both_layouts!(
    captured_callback_reschedule_inline,
    captured_callback_reschedule_boxed,
    captured_callback,
    Callback::Reschedule
);
both_layouts!(
    captured_callback_panic_inline,
    captured_callback_panic_boxed,
    captured_callback,
    Callback::PanicAfterDrop
);
both_layouts!(
    captured_callback_poll_panic_inline,
    captured_callback_poll_panic_boxed,
    captured_callback,
    Callback::PollPanic
);

#[test]
fn zero_sized_callback_run_inline() {
    zero_sized_callback::<0, true>();
}

#[test]
fn zero_sized_callback_run_boxed() {
    zero_sized_callback::<4096, true>();
}

#[test]
fn zero_sized_callback_drop_inline() {
    zero_sized_callback::<0, false>();
}

#[test]
fn zero_sized_callback_drop_boxed() {
    zero_sized_callback::<4096, false>();
}
