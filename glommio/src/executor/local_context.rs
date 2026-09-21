//! Scoped access to the executor running on the current thread.

use std::cell::Cell;

use super::ExecutorContext;

thread_local! {
    static EXECUTOR: Cell<*const ExecutorContext> = const { Cell::new(std::ptr::null()) };
}

/// Accesses the concrete TLS key without storing an indirect `LocalKey` reference.
pub(super) static LOCAL_EX: LocalContext = LocalContext;

/// A stateless interface to the current thread's executor slot.
pub(super) struct LocalContext;

struct Reset<'a> {
    slot: &'a Cell<*const ExecutorContext>,
    previous: *const ExecutorContext,
}

impl Drop for Reset<'_> {
    fn drop(&mut self) {
        self.slot.set(self.previous);
    }
}

impl LocalContext {
    /// Installs an executor for the duration of the closure, restoring the slot
    /// on both normal return and unwinding. The guard borrows the thread's slot
    /// and cannot escape the closure or move to another thread.
    #[inline(always)]
    pub(super) fn set<R>(&self, executor: &ExecutorContext, f: impl FnOnce() -> R) -> R {
        EXECUTOR.with(|slot| {
            let _reset = Reset {
                slot,
                previous: slot.replace(executor),
            };
            f()
        })
    }

    /// Calls the closure if an executor is installed, with a single TLS read.
    ///
    /// The pointer is installed only by `set`, whose executor borrow stays live
    /// until the reset guard runs. The closure's argument lifetime cannot
    /// escape through `R`, so the temporary reference cannot outlive that scope.
    #[inline(always)]
    pub(super) fn try_with<R>(&self, f: impl FnOnce(&ExecutorContext) -> R) -> Option<R> {
        EXECUTOR.with(|slot| unsafe { slot.get().as_ref().map(f) })
    }

    /// Calls the closure with the installed context, if any, using one TLS read.
    #[inline(always)]
    pub(super) fn with_optional<R>(&self, f: impl FnOnce(Option<&ExecutorContext>) -> R) -> R {
        EXECUTOR.with(|slot| unsafe { f(slot.get().as_ref()) })
    }

    /// Calls the closure with the installed executor, panicking if none is set.
    #[inline(always)]
    pub(super) fn with<R>(&self, f: impl FnOnce(&ExecutorContext) -> R) -> R {
        self.try_with(f)
            .expect("cannot access the executor outside LocalExecutor::run")
    }

    /// Returns whether this thread is currently running an executor.
    #[cfg(test)]
    #[inline(always)]
    pub(super) fn is_set(&self) -> bool {
        EXECUTOR.with(|slot| !slot.get().is_null())
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use super::LOCAL_EX;
    use crate::executor::LocalExecutor;

    #[test]
    fn restores_previous_context_on_return_and_unwind() {
        let outer = LocalExecutor::default();
        let inner = LocalExecutor::default();
        assert!(!LOCAL_EX.is_set());
        LOCAL_EX.set(&outer.context, || {
            LOCAL_EX.set(&inner.context, || {
                assert!(LOCAL_EX.with(|ex| std::ptr::eq(ex, &inner.context)));
            });
            assert!(LOCAL_EX.with(|ex| std::ptr::eq(ex, &outer.context)));
            let result = catch_unwind(AssertUnwindSafe(|| {
                LOCAL_EX.set(&inner.context, || {
                    assert!(LOCAL_EX.with(|ex| std::ptr::eq(ex, &inner.context)));
                    panic!("unwind the inner executor scope");
                });
            }));
            assert!(result.is_err());
            assert!(LOCAL_EX.with(|ex| std::ptr::eq(ex, &outer.context)));
        });
        assert!(!LOCAL_EX.is_set());
    }

    #[test]
    fn executor_context_is_thread_local() {
        let executor = LocalExecutor::default();
        LOCAL_EX.set(&executor.context, || {
            std::thread::spawn(|| assert!(LOCAL_EX.try_with(|ex| ex.id()).is_none()))
                .join()
                .unwrap();
            assert_eq!(LOCAL_EX.with(|ex| ex.id()), executor.id());
        });
    }

    #[test]
    fn absent_executor_preserves_optional_and_required_access() {
        assert!(LOCAL_EX
            .try_with(|_| panic!("no executor is installed"))
            .is_none());
        assert!(crate::executor::executor_id().is_none());
        futures_lite::future::block_on(async {
            crate::yield_if_needed().await;
            crate::executor().yield_task_queue_now().await;
        });
        assert!(catch_unwind(|| crate::executor().need_preempt()).is_err());
    }

    #[test]
    fn nested_run_rejection_preserves_outer_executor() {
        let outer = LocalExecutor::default();
        let inner = LocalExecutor::default();
        outer.run(async {
            let result = catch_unwind(AssertUnwindSafe(|| inner.run(async {})));
            assert!(result.is_err());
            assert_eq!(crate::executor::executor_id(), Some(outer.id()));
        });
        assert!(crate::executor::executor_id().is_none());
    }
}
