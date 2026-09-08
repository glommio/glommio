//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
//! Task abstraction for building executors.
//!
//! # Spawning
//!
//! To spawn a future onto the Glommio executor, we first need to allocate it on
//! the heap and keep some state alongside it. The state indicates whether the
//! future is ready for polling, waiting to be woken up, or completed. Such a
//! future is called a *task*.
//!
//! When a task is run, its future gets polled. If polling does not complete the
//! task, that means it's waiting for another future and needs to go to sleep.
//! When woken up, its schedule function will be invoked, pushing it back into
//! the queue so that it can be run again.
//!
//! Paired with a task there usually is a [`JoinHandle`] that can be used to
//! wait for the task's completion.
//!
//! # Cancellation
//!
//! Both [`Task`] and [`JoinHandle`] have methods that cancel the task. When
//! canceled, the task's future will not be polled again and will get dropped
//! instead.
//!
//! If canceled by the [`Task`] instance, the task is destroyed immediately. If
//! canceled by the [`JoinHandle`] instance, it will be scheduled one more time
//! and the next attempt to run it will simply destroy it.
//!
//! The `JoinHandle` future will then evaluate to `None`, but only after the
//! task's future is dropped.
//!
//! # Performance
//!
//! Task construction incurs a single allocation that holds its state, the
//! schedule function, and the future or the result of the future if completed.
//!
//! A header tracks the task's references and membership in its executor's
//! cleanup registry. It is followed by the schedule function and a union of the
//! future and its output. The executor drops thread-local resources before
//! releasing its reference; surviving wakers can then free the allocation on
//! any thread.
//!
//! [`spawn_local`]: fn.spawn_local.html
//! [`Task`]: struct.Task.html
//! [`JoinHandle`]: struct.JoinHandle.html
//! [`Waker`]: https://doc.rust-lang.org/std/task/struct.Waker.html

#![warn(missing_docs, missing_debug_implementations)]

#[cfg(test)]
mod cleanup_tests;
#[cfg(feature = "debugging")]
pub mod debugging;
pub(crate) mod header;
pub(crate) mod join_handle;
#[cfg(test)]
mod lifecycle_race_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod ownership_tests;
#[cfg(test)]
mod payload_tests;
#[cfg(test)]
mod public_spawn_tests;
pub(crate) mod raw;
pub(crate) mod registry;
pub(crate) mod state;
pub(crate) mod task_impl;
#[cfg(test)]
mod test_support;
mod tests;
pub(crate) mod utils;
pub(crate) mod waker_fn;

pub use crate::task::{join_handle::JoinHandle, task_impl::Task};

/// Mark context for task operations
#[macro_export]
macro_rules! dbg_context {
    ($ptr:expr, $name:tt, $($body:tt)*) => {{
        #[cfg(feature = "debugging")]
        let entered = TaskDebugger::enter($ptr, $name);

        #[cfg(feature = "debugging")]
        defer! {
            if entered {
                TaskDebugger::leave();
            }
        }

        $($body)*
    }};
}
