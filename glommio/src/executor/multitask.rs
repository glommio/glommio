//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
//! An executor for running async tasks.

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations)]

use crate::{
    executor::{with_executor_context, TaskQueue},
    task::{registry::TaskRegistry, task_impl, JoinHandle},
    Latency,
};
use std::{
    cell::RefCell,
    future::Future,
    panic::{RefUnwindSafe, UnwindSafe},
    pin::Pin,
    rc::{Rc, Weak},
    task::{Context, Poll},
};

/// A runnable future, ready for execution.
///
/// When a future is internally spawned using `task::spawn()` or
/// `task::spawn_local()`, we get back two values:
///
/// 1. an `task::Task<()>`, which we refer to as a `Runnable`
/// 2. an `task::JoinHandle<T, ()>`, which is wrapped inside a `Task<T>`
///
/// Once a `Runnable` is run, it "vanishes" and only reappears when its future
/// is woken. When it's woken up, its schedule function is called, which means
/// the `Runnable` gets pushed into a task queue in an executor.
pub(crate) type Runnable = task_impl::Task;

/// A spawned future.
///
/// Tasks are also futures themselves and yield the output of the spawned
/// future.
///
/// When a task is dropped, its gets canceled and won't be polled again. To
/// cancel a task a bit more gracefully and wait until it stops running, use the
/// [`cancel()`][Task::cancel()] method.
///
/// Tasks that panic get immediately canceled. Awaiting a canceled task also
/// causes a panic.
///
/// If a task panics, the panic will be thrown by the [`Ticker::tick()`]
/// invocation that polled it.
///
/// ```
#[must_use = "tasks get canceled when dropped, use `.detach()` to run them in the background"]
#[derive(Debug)]
pub(crate) struct Task<T>(Option<JoinHandle<T>>);

impl<T> Task<T> {
    /// Detaches the task to let it keep running in the background.
    pub(crate) fn detach(mut self) -> JoinHandle<T> {
        self.0.take().unwrap()
    }

    /// Cancels the task and waits for it to stop running.
    pub(crate) async fn cancel(self) -> Option<T> {
        let mut task = self;
        let handle = task.0.take().unwrap();
        handle.cancel();
        handle.await
    }
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        if let Some(handle) = &self.0 {
            handle.cancel();
        }
    }
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0.as_mut().unwrap())
            .poll(cx)
            .map(|output| output.expect("task has failed"))
    }
}

/// Shared scheduling metadata outlives queue removal without retaining its owner.
#[derive(Debug)]
pub(super) struct Scheduler {
    queue: Weak<RefCell<TaskQueue>>,
    owner: Weak<TaskRegistry>,
    owner_id: usize,
}

impl Scheduler {
    #[inline]
    fn schedule(&self, runnable: Runnable) {
        with_executor_context(|context| match context {
            Some(context) if context.id() == self.owner_id => match self.queue.upgrade() {
                Some(queue) => context.schedule(queue, runnable),
                None => context.schedule_cleanup(runnable),
            },
            _ => self.schedule_inactive(runnable),
        });
    }

    #[cold]
    fn schedule_inactive(&self, runnable: Runnable) {
        if let Some(context) = self.owner.upgrade().and_then(|owner| owner.context()) {
            match self.queue.upgrade() {
                Some(queue) if !runnable.is_cancelled() => context.schedule(queue, runnable),
                queue => context.with_cleanup(queue, || drop(runnable)),
            }
        }
    }
}

impl UnwindSafe for Scheduler {}

impl RefUnwindSafe for Scheduler {}

impl Scheduler {
    /// Creates the scheduling state shared by a queue and its tasks.
    pub(super) fn new(
        queue: Weak<RefCell<TaskQueue>>,
        owner_id: usize,
        registry: &Rc<TaskRegistry>,
    ) -> Self {
        Self {
            queue,
            owner: Rc::downgrade(registry),
            owner_id,
        }
    }

    /// Transfers the caller's scheduler reference into a new task.
    fn spawn<T>(
        self: Rc<Self>,
        owner_id: usize,
        registry: &Rc<TaskRegistry>,
        tq: Rc<RefCell<TaskQueue>>,
        future: impl Future<Output = T>,
    ) -> (Runnable, JoinHandle<T>) {
        let latency_matters = match tq.borrow().io_requirements.latency_req {
            Latency::Matters(_) => true,
            Latency::NotImportant => false,
        };
        let schedule = move |runnable: Runnable| self.schedule(runnable);

        // Create a task, push it into the queue by scheduling it, and return its `Task`
        // handle.
        task_impl::spawn_local(owner_id, registry, future, schedule, latency_matters)
    }

    pub(crate) fn spawn_and_run<T>(
        self: Rc<Self>,
        executor_id: usize,
        registry: &Rc<TaskRegistry>,
        tq: Rc<RefCell<TaskQueue>>,
        future: impl Future<Output = T>,
    ) -> Task<T> {
        let (runnable, handle) = self.spawn(executor_id, registry, tq, future);
        runnable.run_right_away();
        Task(Some(handle))
    }

    pub(crate) fn spawn_and_schedule<T>(
        self: Rc<Self>,
        executor_id: usize,
        registry: &Rc<TaskRegistry>,
        tq: Rc<RefCell<TaskQueue>>,
        future: impl Future<Output = T>,
    ) -> Task<T> {
        let (runnable, handle) = self.spawn(executor_id, registry, tq, future);
        runnable.schedule();
        Task(Some(handle))
    }
}
