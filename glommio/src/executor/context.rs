//! Executor resources retained while a task destructor can release its owner.

use std::{
    cell::RefCell,
    collections::hash_map::Entry,
    future::Future,
    rc::{Rc, Weak},
};

use crate::{task::registry::TaskRegistry, GlommioError, IoRequirements, Latency, Reactor, Shares};

use super::{
    multitask, ExecutorQueues, Result, TaskQueue, TaskQueueHandle, UnobservedPanic, LOCAL_EX,
};

/// Shared resources used by the executor and retained independently during cleanup.
#[derive(Debug)]
pub(crate) struct ExecutorContext {
    pub(super) queues: Rc<RefCell<ExecutorQueues>>,
    pub(super) tasks: Rc<TaskRegistry>,
    pub(super) id: usize,
    pub(super) reactor: Rc<Reactor>,
    /// What a blocking closure's panic does when nobody awaits it.
    pub(super) on_unobserved_panic: UnobservedPanic,
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

    /// The default queue, which `init` creates and `remove_task_queue`
    /// refuses to remove, so it is there for the executor's whole life.
    fn cleanup_queue(&self) -> Rc<RefCell<TaskQueue>> {
        self.queues
            .borrow()
            .available_executors
            .get(&TaskQueueHandle::default().index)
            .cloned()
            .expect("the default task queue is never removed")
    }

    /// Provides queue and I/O context without retaining the public executor.
    pub(super) fn with_cleanup<T>(
        &self,
        queue: Option<Rc<RefCell<TaskQueue>>>,
        f: impl FnOnce() -> T,
    ) -> T {
        let was_active = self.tasks.cleanup_active.replace(true);
        defer!(self.tasks.cleanup_active.set(was_active));
        self.enter(|| {
            let queue = queue
                .or_else(|| self.queues.borrow().active_executing.clone())
                .unwrap_or_else(|| self.cleanup_queue());
            let requirements = queue.borrow().io_requirements;
            let previous_requirements = self.reactor.io_requirements();
            let previous = self.queues.borrow_mut().active_executing.replace(queue);
            self.reactor.inform_io_requirements(requirements);
            defer! {
                if !was_active && self.tasks.is_shutting_down() {
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

    /// Drains shutdown work once the outermost cleanup callback has returned.
    /// Take one runnable at a time so destructors run without queue borrows and
    /// any tasks or queues they create are included in the next iteration.
    fn finish_shutdown(&self) {
        loop {
            if self.tasks.shutdown_next() {
                continue;
            }
            let task = self
                .queues
                .borrow()
                .available_executors
                .values()
                .find_map(|queue| queue.borrow_mut().get_task());
            let Some(task) = task else { break };
            drop(task);
        }
    }

    /// A removed queue must defer destruction until reactor borrows are released.
    ///
    /// The cancelled runnable goes to the default queue directly, never to a
    /// window: a window keeps runnable the queue of the runnable it holds,
    /// which is the removed queue here, so a held cleanup would leave the
    /// default queue idle with the cleanup still to run. No borrow is held
    /// while it is cancelled, since cancelling wakes a task awaiting it, whose
    /// schedule borrows the queues.
    pub(super) fn schedule_cleanup(&self, runnable: multitask::Runnable) {
        self.forget_preempted(&runnable);
        runnable.cancel();
        let queue = self.cleanup_queue();
        queue.borrow_mut().runnables.push_back(runnable);
        self.queues.borrow_mut().maybe_activate(queue);
    }

    /// Activates the owning executor's queue even while another executor runs.
    ///
    /// The task that was running when the last slice was preempted is held
    /// rather than queued, until the window that slice opened ends.
    #[inline]
    pub(super) fn schedule(&self, queue: Rc<RefCell<TaskQueue>>, runnable: multitask::Runnable) {
        let mut queues = self.queues.borrow_mut();
        let runnable = if queues.preempted.is_some() {
            match queues.hold(&queue, runnable) {
                Ok(displaced) => {
                    drop(queues);
                    if let Some(displaced) = displaced {
                        self.requeue(displaced);
                    }
                    return;
                }
                Err(runnable) => runnable,
            }
        } else {
            runnable
        };
        queue.borrow_mut().runnables.push_back(runnable);
        queues.maybe_activate(queue);
    }

    /// Forgets `runnable`'s task in the open window, if one remembers it and
    /// holds nothing of it, before the task is destroyed without being held:
    /// cancelled for cleanup, or dropped in cleanup for another executor. A
    /// task allocated at its address later is then not taken for it.
    pub(super) fn forget_preempted(&self, runnable: &multitask::Runnable) {
        if let Some(preempted) = self.queues.borrow_mut().preempted.as_mut() {
            preempted.forget(runnable.raw_task.as_ptr().addr());
        }
    }

    /// Queues a runnable a window held or displaced at the back of its queue,
    /// as a wake would, without holding it again. One whose queue is gone goes
    /// to cleanup.
    pub(super) fn requeue(&self, runnable: multitask::Runnable) {
        let Some(queue) = self.get_queue(&TaskQueueHandle {
            index: runnable.task_queue_index(),
        }) else {
            self.schedule_cleanup(runnable);
            return;
        };
        queue.borrow_mut().runnables.push_back(runnable);
        self.queues.borrow_mut().maybe_activate(queue);
    }

    /// Ends the window a preempted slice opened: forgets which tasks were
    /// running and queues those it held behind the tasks woken in the
    /// window, the inline tasks first, in the order they were queued.
    ///
    /// Called when the next round starts, before the executor parks, and when
    /// `run` returns, so a held runnable is never left behind for a park,
    /// another `run`, or shutdown. Each is queued the way a wake would queue
    /// it, so a queue that is gone sends it to cleanup.
    pub(super) fn end_preempted_window(&self) {
        let Some(preempted) = self.queues.borrow_mut().preempted.take() else {
            return;
        };
        for held in preempted.inline.into_iter().chain(preempted.last) {
            if let Some(runnable) = held.runnable {
                self.requeue(runnable);
            }
        }
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

    /// The removed queue is dropped after the queues are released: a dynamic
    /// shares manager it owns may run destructors that reach the executor.
    ///
    /// The default queue is refused: `spawn` and `run` place work in it, and a
    /// task woken in a missing queue is cancelled.
    pub(super) fn remove_task_queue(&self, handle: TaskQueueHandle) -> Result<()> {
        if handle == TaskQueueHandle::default() {
            return Err(GlommioError::queue_still_active(handle.index));
        }
        let removed = {
            let mut queues = self.queues.borrow_mut();
            if queues
                .active_executing
                .as_ref()
                .is_some_and(|queue| queue.borrow().stats.index == handle)
            {
                return Err(GlommioError::queue_still_active(handle.index));
            }

            let queue_entry = queues.available_executors.entry(handle.index);
            let Entry::Occupied(entry) = queue_entry else {
                return Err(GlommioError::queue_not_found(handle.index));
            };
            if entry.get().borrow().is_active() {
                return Err(GlommioError::queue_still_active(handle.index));
            }
            entry.remove()
        };
        drop(removed);
        Ok(())
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

    /// Spawns a task into the running queue and polls it at once, inside the
    /// poll of the task spawning it.
    ///
    /// If the preempt timer fired during that poll, the spawned task, not its
    /// spawner, spent the slice, and the window a preempted slice opens is
    /// opened for it there and then, unless the poll finished it: a finished
    /// task can be neither queued nor woken, and once its handle is gone its
    /// address can go to a task spawned later in the slice. It was queued as its poll returned if it
    /// woke itself, so it is at the back of its queue and is held from there,
    /// before anything its spawner queues after it. The timer is sampled
    /// before the poll as well, so that a task spawned after it fired is not
    /// taken for the one that spent the slice.
    pub(super) fn spawn_internal<T>(&self, future: impl Future<Output = T>) -> multitask::Task<T> {
        let tq = self
            .queues
            .borrow()
            .active_executing
            .clone()
            .or_else(|| self.get_queue(&TaskQueueHandle { index: 0 }))
            .unwrap();

        let scheduler = tq.borrow().scheduler.clone();
        let was_preempting = self.need_preempt();
        let (task, address) = scheduler.spawn_and_run(&self.tasks, tq, future);
        if !was_preempting && self.need_preempt() && !task.is_finished() {
            self.queues.borrow_mut().preempted_inline(address);
        }
        task
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

        Ok(scheduler.spawn_and_schedule(&self.tasks, tq, future))
    }
}

/// Tasks can recover an idle owner's context without keeping its resources alive.
#[derive(Debug)]
#[cfg_attr(test, derive(Default))]
pub(crate) struct WeakExecutorContext {
    queues: Weak<RefCell<ExecutorQueues>>,
    id: usize,
    reactor: Weak<Reactor>,
    on_unobserved_panic: UnobservedPanic,
}

impl WeakExecutorContext {
    pub(super) fn new(
        id: usize,
        queues: &Rc<RefCell<ExecutorQueues>>,
        reactor: &Rc<Reactor>,
        on_unobserved_panic: UnobservedPanic,
    ) -> Self {
        Self {
            queues: Rc::downgrade(queues),
            id,
            reactor: Rc::downgrade(reactor),
            on_unobserved_panic,
        }
    }

    pub(crate) fn upgrade(&self, tasks: &Rc<TaskRegistry>) -> Option<ExecutorContext> {
        Some(ExecutorContext {
            queues: self.queues.upgrade()?,
            tasks: tasks.clone(),
            id: self.id,
            reactor: self.reactor.upgrade()?,
            on_unobserved_panic: self.on_unobserved_panic.clone(),
        })
    }
}
