// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
// This product includes software developed at Datadog (https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//
//! Tracks tasks whose thread-local resources still belong to an executor.

use std::{cell::Cell, ptr};

use crate::executor::{ExecutorContext, WeakExecutorContext};

use super::header::Header;

/// An executor's intrusive list of tasks requiring owner-thread cleanup.
/// The hot list head stays ahead of the cold cleanup context.
#[derive(Debug)]
#[repr(C)]
pub(crate) struct TaskRegistry {
    head: Cell<*mut Header>,
    context: WeakExecutorContext,
    shutting_down: Cell<bool>,
}

impl TaskRegistry {
    pub(crate) fn new(context: WeakExecutorContext) -> Self {
        Self {
            head: Cell::new(ptr::null_mut()),
            context,
            shutting_down: Cell::new(false),
        }
    }

    /// Retains the resources needed while task cleanup can destroy its executor.
    pub(crate) fn context(&self) -> Option<ExecutorContext> {
        self.context.upgrade()
    }

    /// Cleanup contexts must also drain tasks spawned after reentrant shutdown.
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.get()
    }

    /// Registers a task protected by its counted executor reference.
    ///
    /// The task and registry must remain alive until the task is removed. All
    /// registry operations must happen on the owning thread.
    pub(crate) unsafe fn insert(&self, task: *mut Header) {
        debug_assert!((*task).prev_link.is_null());
        let head = self.head.get();
        // The registry lives in an Rc and tasks stay in their original
        // allocation, so both kinds of link target remain stable until unlink.
        (*task).prev_link = self.head.as_ptr();
        (*task).next = head;
        if !head.is_null() {
            (*head).prev_link = ptr::addr_of_mut!((*task).next);
        }
        self.head.set(task);
    }

    /// Unlinks a registered task without releasing its executor reference.
    pub(crate) unsafe fn remove(task: *mut Header) {
        let prev_link = (*task).prev_link;
        debug_assert!(!prev_link.is_null());
        let next = (*task).next;
        *prev_link = next;
        if !next.is_null() {
            (*next).prev_link = prev_link;
        }
        (*task).prev_link = ptr::null_mut();
        (*task).next = ptr::null_mut();
    }

    /// Cancels and cleans each task while its executor is still available.
    pub(crate) fn shutdown(&self) {
        self.shutting_down.set(true);
        loop {
            let task = self.head.get();
            if task.is_null() {
                break;
            }
            // Shutdown unlinks this task. Read the head again afterward: its
            // destructors can also remove other tasks or create new tasks, so
            // retaining a next pointer across the callback would be unsafe.
            unsafe { ((*task).vtable.shutdown)(task.cast()) };
        }
    }
}
