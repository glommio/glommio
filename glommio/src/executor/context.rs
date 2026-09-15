//! Executor resources retained while a task destructor can release its owner.

use std::{
    cell::RefCell,
    future::Future,
    rc::{Rc, Weak},
};

use crate::{task::registry::TaskRegistry, GlommioError, IoRequirements, Latency, Reactor, Shares};

use super::{multitask, ExecutorQueues, Result, TaskQueue, TaskQueueHandle, LOCAL_EX};

/// A temporary owner context built from the executor's existing shared resources.
#[derive(Debug)]
pub(crate) struct ExecutorContext {
    pub(super) queues: Rc<RefCell<ExecutorQueues>>,
    pub(super) tasks: Rc<TaskRegistry>,
    pub(super) id: usize,
    pub(super) reactor: Rc<Reactor>,
}

impl ExecutorContext {
    pub(super) fn id(&self) -> usize {
        self.id
    }

    #[inline(always)]
    pub(super) fn need_preempt(&self) -> bool {
        self.reactor.need_preempt()
    }

    /// Restores the previous context on both normal return and unwinding.
    pub(super) fn enter<T>(&self, f: impl FnOnce() -> T) -> T {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX.set(self, f);

        #[cfg(all(nightly, feature = "native-tls"))]
        unsafe {
            let previous = LOCAL_EX;
            defer!(LOCAL_EX = previous);
            LOCAL_EX = self as *const Self;
            f()
        }
    }

    fn cleanup_queue(&self) -> Rc<RefCell<TaskQueue>> {
        let queue = {
            let queues = self.queues.borrow();
            queues
                .available_executors
                .get(&TaskQueueHandle::default().index)
                .or_else(|| queues.available_executors.values().next())
                .cloned()
        };
        if let Some(queue) = queue {
            return queue;
        }
        let handle = self.create_task_queue(Shares::default(), Latency::NotImportant, "cleanup");
        self.get_queue(&handle).expect("cleanup queue disappeared")
    }

    /// Provides queue and I/O context without retaining the public executor.
    pub(super) fn with_cleanup<T>(
        &self,
        queue: Option<Rc<RefCell<TaskQueue>>>,
        f: impl FnOnce() -> T,
    ) -> T {
        self.enter(|| {
            let queue = queue
                .or_else(|| self.queues.borrow().active_executing.clone())
                .unwrap_or_else(|| self.cleanup_queue());
            let requirements = queue.borrow().io_requirements;
            let previous_requirements = self.reactor.io_requirements();
            let previous = self.queues.borrow_mut().active_executing.replace(queue);
            self.reactor.inform_io_requirements(requirements);
            defer! {
                if self.tasks.is_shutting_down() {
                    self.finish_shutdown();
                }
                let current = std::mem::replace(
                    &mut self.queues.borrow_mut().active_executing,
                    previous,
                );
                self.reactor.inform_io_requirements(previous_requirements);
                drop(current);
            }
            f()
        })
    }

    /// Runs before removing cleanup TLS, including after nested executor drops.
    fn finish_shutdown(&self) {
        self.tasks.shutdown();
        let queues: Vec<_> = self
            .queues
            .borrow()
            .available_executors
            .values()
            .cloned()
            .collect();
        for queue in queues {
            loop {
                let task = queue.borrow_mut().get_task();
                let Some(task) = task else { break };
                drop(task);
            }
        }
    }

    /// A removed queue must defer destruction until reactor borrows are released.
    pub(super) fn schedule_cleanup(&self, runnable: multitask::Runnable) {
        runnable.cancel();
        let queue = self.cleanup_queue();
        self.schedule(queue, runnable);
    }

    /// Activates the owning executor's queue even while another executor runs.
    #[inline]
    pub(super) fn schedule(&self, queue: Rc<RefCell<TaskQueue>>, runnable: multitask::Runnable) {
        queue.borrow_mut().runnables.push_back(runnable);
        self.queues.borrow_mut().maybe_activate(queue);
    }

    pub(super) fn get_reactor(&self) -> Rc<Reactor> {
        self.reactor.clone()
    }

    pub(super) fn create_task_queue<S>(
        &self,
        shares: Shares,
        latency: Latency,
        name: S,
    ) -> TaskQueueHandle
    where
        S: Into<String>,
    {
        let index = {
            let mut ex = self.queues.borrow_mut();
            let index = ex.executor_index;
            ex.executor_index += 1;
            index
        };

        let io_requirements = IoRequirements::new(latency, index);
        let tq = TaskQueue::new(
            TaskQueueHandle { index },
            name,
            shares,
            io_requirements,
            self.id,
            &self.tasks,
        );

        self.queues
            .borrow_mut()
            .available_executors
            .insert(index, tq);
        TaskQueueHandle { index }
    }

    pub(super) fn get_queue(&self, handle: &TaskQueueHandle) -> Option<Rc<RefCell<TaskQueue>>> {
        self.queues
            .borrow()
            .available_executors
            .get(&handle.index)
            .cloned()
    }

    pub(super) fn current_task_queue(&self) -> TaskQueueHandle {
        self.queues
            .borrow()
            .active_executing
            .as_ref()
            .unwrap()
            .borrow()
            .stats
            .index
    }

    pub(super) fn mark_me_for_yield(&self) {
        let queues = self.queues.borrow();
        let mut me = queues.active_executing.as_ref().unwrap().borrow_mut();
        me.yielded = true;
    }

    pub(super) fn spawn_internal<T>(&self, future: impl Future<Output = T>) -> multitask::Task<T> {
        let tq = self
            .queues
            .borrow()
            .active_executing
            .clone()
            .or_else(|| self.get_queue(&TaskQueueHandle { index: 0 }))
            .unwrap();

        let id = self.id;
        let scheduler = tq.borrow().scheduler.clone();
        scheduler.spawn_and_run(id, &self.tasks, tq, future)
    }

    pub(super) fn spawn_into<T, F>(
        &self,
        future: F,
        handle: TaskQueueHandle,
    ) -> Result<multitask::Task<T>>
    where
        F: Future<Output = T>,
    {
        let tq = self
            .get_queue(&handle)
            .ok_or_else(|| GlommioError::queue_not_found(handle.index))?;
        let scheduler = tq.borrow().scheduler.clone();
        let id = self.id;

        Ok(scheduler.spawn_and_schedule(id, &self.tasks, tq, future))
    }
}

/// Tasks can recover an idle owner's context without keeping its resources alive.
#[derive(Debug)]
pub(crate) struct WeakExecutorContext {
    queues: Weak<RefCell<ExecutorQueues>>,
    tasks: Weak<TaskRegistry>,
    id: usize,
    reactor: Weak<Reactor>,
}

impl WeakExecutorContext {
    pub(super) fn new(
        id: usize,
        queues: &Rc<RefCell<ExecutorQueues>>,
        reactor: &Rc<Reactor>,
        tasks: Weak<TaskRegistry>,
    ) -> Self {
        Self {
            queues: Rc::downgrade(queues),
            tasks,
            id,
            reactor: Rc::downgrade(reactor),
        }
    }

    pub(crate) fn upgrade(&self) -> Option<ExecutorContext> {
        Some(ExecutorContext {
            queues: self.queues.upgrade()?,
            tasks: self.tasks.upgrade()?,
            id: self.id,
            reactor: self.reactor.upgrade()?,
        })
    }
}
