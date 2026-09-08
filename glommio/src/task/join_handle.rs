//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
use core::{
    fmt,
    future::Future,
    marker::{PhantomData, Unpin},
    pin::Pin,
    ptr::NonNull,
    task::{Context, Poll},
};

#[cfg(feature = "debugging")]
use crate::task::debugging::TaskDebugger;
use crate::{
    dbg_context,
    task::{header::Header, state::*},
};

/// A handle that awaits the result of a task.
///
/// This type is a future that resolves to an `Option<R>` where:
///
/// * `None` indicates the task has panicked or was canceled.
/// * `Some(result)` indicates the task has completed with `result` of type `R`.
pub struct JoinHandle<R> {
    /// A raw task pointer.
    pub(crate) raw_task: NonNull<()>,

    /// A marker capturing generic types `R`.
    pub(crate) _marker: PhantomData<R>,
}

impl<R> Unpin for JoinHandle<R> {}

impl<R> JoinHandle<R> {
    /// Cancels the task.
    ///
    /// If the task has already completed, calling this method will have no
    /// effect.
    ///
    /// When a task is canceled, its future will not be polled again.
    pub fn cancel(&self) {
        let ptr = self.raw_task.as_ptr();
        dbg_context!(ptr, "cancel", {
            let header = ptr as *const Header;
            unsafe {
                ((*header).vtable.cancel)(ptr);
            }
        });
    }
}

impl<R> Drop for JoinHandle<R> {
    fn drop(&mut self) {
        let ptr = self.raw_task.as_ptr();
        dbg_context!(ptr, "drop_join_handle", {
            let header = ptr as *const Header;
            unsafe {
                ((*header).vtable.drop_handle)(ptr);
            }
        });
    }
}

impl<R> Future for JoinHandle<R> {
    type Output = Option<R>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let ptr = self.raw_task.as_ptr();
        let header = ptr as *mut Header;

        unsafe {
            let state = (*header).state;

            // If the task has been closed, notify the awaiter and return `None`.
            if state & CLOSED != 0 {
                // A queued runnable can outlive executor shutdown. Wait only for the
                // future itself to be dropped.
                if state & FUTURE_DROPPED == 0 || state & RUNNING != 0 {
                    // Replace the waker with one associated with the current task.
                    Header::register(header, cx.waker());
                    return Poll::Pending;
                }

                // Even though the awaiter is most likely the current task, it could also be
                // another task.
                Header::notify(header, Some(cx.waker()));
                return Poll::Ready(None);
            }

            // If the task is not completed, register the current task.
            if state & COMPLETED == 0 {
                // Replace the waker with one associated with the current task.
                Header::register(header, cx.waker());

                return Poll::Pending;
            }

            (*header).state = (state | CLOSED) & !OUTPUT_PRESENT;

            // Notify the awaiter. Even though the awaiter is most likely the current
            // task, it could also be another task.
            Header::notify(header, Some(cx.waker()));

            // Take the output from the task.
            let output = ((*header).vtable.get_output)(ptr) as *mut R;
            Poll::Ready(Some(output.read()))
        }
    }
}

impl<R> fmt::Debug for JoinHandle<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ptr = self.raw_task.as_ptr();
        let header = ptr as *const Header;

        f.debug_struct("JoinHandle")
            .field("header", unsafe { &(*header) })
            .finish()
    }
}
