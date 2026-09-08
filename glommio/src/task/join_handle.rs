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

    /// If the task has been closed, the awaiter is notified and `None` is
    /// returned: a queued runnable can outlive executor shutdown, so if the
    /// task is scheduled or running we wait only for the future itself to be
    /// dropped, and the current task's waker is registered until then.
    ///
    /// Even though the awaiter is most likely the current task, it could
    /// also be another task.
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let ptr = self.raw_task.as_ptr();
        let header = ptr as *mut Header;

        unsafe {
            let state = (*header).state;

            if state & CLOSED != 0 {
                if state & FUTURE_DROPPED == 0 || state & RUNNING != 0 {
                    Header::register(&(*header).awaiter, cx.waker());
                    return Poll::Pending;
                }

                Header::notify(&{ &*header }.awaiter, Some(cx.waker()));
                return Poll::Ready(None);
            }

            if state & COMPLETED == 0 {
                Header::register(&(*header).awaiter, cx.waker());

                return Poll::Pending;
            }

            (*header).state = (state | CLOSED) & !OUTPUT_PRESENT;

            Header::notify(&(*header).awaiter, Some(cx.waker()));

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
