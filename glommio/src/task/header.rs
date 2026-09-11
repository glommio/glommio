//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
use core::{fmt, task::Waker};
#[cfg(feature = "debugging")]
use std::cell::Cell;
use std::sync::atomic::{AtomicI32, Ordering};

use crate::task::{raw::TaskVTable, state::*, utils::abort_on_panic};

pub(crate) type RefCount = i32;
pub(crate) type AtomicRefCount = AtomicI32;

/// The header of a task.
///
/// This header is stored right at the beginning of every heap-allocated task.
pub(crate) struct Header {
    /// ID of the executor to which task belongs to or in other words by which
    /// task was spawned by.
    ///
    /// Deliberately an id rather than an `Arc<SleepNotifier>`. Holding the
    /// notifier here would mean resolving it from the global registry on every
    /// spawn, which serialises all executors on a single lock, and would keep
    /// the executor's eventfd alive for as long as any task outlives it. The
    /// notifier is instead resolved from the id on the foreign-wake path, which
    /// is rare. See `RawTask::notifier`.
    pub(crate) executor_id: usize,

    /// Index of the task queue this task belongs to.
    ///
    /// As with `executor_id`, this is an index rather than an
    /// `Rc<RefCell<TaskQueue>>` or a `Weak` to one. Capturing the queue in the
    /// schedule closure made that closure non-zero-sized, which in turn forced
    /// `RawTask::schedule` to clone and drop a waker as a lifetime guard on
    /// every single wake -- two atomic read-modify-writes per task switch, for
    /// eight bytes of capture. Resolving the queue from this index on the
    /// owning thread keeps the closure zero-sized and skips the guard entirely.
    ///
    pub(crate) task_queue_index: usize,

    /// Current state of the task.
    pub(crate) state: u8,

    /// Latency matters or not
    pub(crate) latency_matters: bool,

    /// Current reference count of the task.
    pub(crate) references: AtomicRefCount,

    /// The task that is blocked on the `JoinHandle`.
    ///
    /// This waker needs to be woken up once the task completes or is closed.
    pub(crate) awaiter: Option<Waker>,

    /// The virtual table.
    ///
    /// In addition to the actual waker virtual table, it also contains pointers
    /// to several other methods necessary for bookkeeping the
    /// heap-allocated task.
    pub(crate) vtable: &'static TaskVTable,

    #[cfg(feature = "debugging")]
    pub(crate) debugging: Cell<bool>,
}

impl Header {
    /// Cancels the task.
    ///
    /// This method will mark the task as closed, but it won't reschedule the
    /// task or drop its future.
    pub(crate) fn cancel(&mut self) {
        // If the task has been completed or closed, it can't be canceled.
        if self.state & (COMPLETED | CLOSED) != 0 {
            return;
        }

        // Mark the task as closed.
        self.state |= CLOSED;
    }

    /// Notifies the awaiter blocked on this task.
    ///
    /// If the awaiter is the same as the current waker, it will not be
    /// notified.
    #[inline]
    pub(crate) fn notify(&mut self, current: Option<&Waker>) {
        // Take the waker out.
        let waker = self.awaiter.take();

        if let Some(w) = waker {
            // We need a safeguard against panics because waking can panic.
            abort_on_panic(|| match current {
                None => w.wake(),
                Some(c) if !w.will_wake(c) => w.wake(),
                Some(_) => {}
            });
        }
    }

    /// Registers a new awaiter blocked on this task.
    ///
    /// This method is called when `JoinHandle` is polled and the task has not
    /// completed.
    #[inline]
    pub(crate) fn register(&mut self, waker: &Waker) {
        // Put the waker into the awaiter field.
        abort_on_panic(|| self.awaiter = Some(waker.clone()));
    }

    #[cfg(feature = "debugging")]
    pub(crate) fn to_compact_string(&self) -> String {
        let state = self.state;

        macro_rules! test {
            ($flag:tt) => {
                if state & $flag != 0 {
                    stringify!($flag)
                } else {
                    ""
                }
            };
        }

        format!(
            "thread:{}|{:>9}|{:>7}|{:>9}|{:>6}|{:>6}|refs:{}",
            self.executor_id,
            test!(SCHEDULED),
            test!(RUNNING),
            test!(COMPLETED),
            test!(CLOSED),
            test!(HANDLE),
            self.references.load(Ordering::Relaxed)
        )
    }
}

impl fmt::Debug for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.state;
        let refcount = self.references.load(Ordering::Relaxed);

        f.debug_struct("Header")
            .field("ptr", &(self as *const Self))
            .field(
                "current_thread_id",
                &crate::executor::executor_id().unwrap_or(usize::MAX),
            )
            .field("thread_id", &self.executor_id)
            .field("scheduled", &(state & SCHEDULED != 0))
            .field("running", &(state & RUNNING != 0))
            .field("completed", &(state & COMPLETED != 0))
            .field("closed", &(state & CLOSED != 0))
            .field("handle", &(state & HANDLE != 0))
            .field("refcount", &refcount)
            .finish()
    }
}
