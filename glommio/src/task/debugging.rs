//! A task debugger that captures the state of in-flight tasks and allows
//! third-party code to introspect into the state of the scheduler.
//! Use the `debugging` feature flag to enable.

use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use crate::task::header::Header;

thread_local! {
    static DEBUGGER: RefCell<Option<TaskDebugger>> = const { RefCell::new(None) };
}

/// Provide facilities to inspect the lifecycle of glommio tasks
#[derive(Debug)]
pub struct TaskDebugger {
    label: Option<&'static str>,
    registry: HashMap<*const (), TaskInfo>,
    filter: fn(Option<&'static str>) -> bool,
    task_count: Arc<AtomicUsize>,
    current_task: Option<*const ()>,
    context: Vec<&'static str>,
}

impl TaskDebugger {
    /// Set label for the next glommio task to be spawned. Labels are
    /// used for filtering tasks to inspect.
    pub fn set_label(label: &'static str) {
        Self::with(|dbg| {
            dbg.label = Some(label);
        });
    }

    /// Set the filter function
    pub fn set_filter(filter: fn(Option<&str>) -> bool) {
        Self::with(|dbg| {
            dbg.filter = filter;
        });
    }

    /// Print the list of tasks which are older than the specified duration.
    pub fn debug_aged_tasks(older_than: Duration) {
        Self::with(|dbg| {
            let mut count = 0;
            for v in dbg.registry.values() {
                let age = v.ts.elapsed();
                if age > older_than {
                    count += 1;
                    dbg.debug_task_info(v, format!("age: {age:?}").as_str());
                }
            }
            if count > 0 {
                log::debug!("found {count} tasks older than {older_than:?}");
            }
        })
    }

    /// Returns a count of tasks which are not destroyed yet.
    pub fn task_count() -> usize {
        Self::with(|dbg| dbg.task_count.load(Ordering::Relaxed))
    }
}

impl TaskDebugger {
    fn with<F, R>(f: F) -> R
    where
        F: FnOnce(&mut TaskDebugger) -> R,
    {
        Self::try_with(f).expect("task debugger is unavailable during thread teardown")
    }

    /// Executors may destroy tasks after this thread-local debugger is gone.
    fn try_with<F, R>(f: F) -> Option<R>
    where
        F: FnOnce(&mut TaskDebugger) -> R,
    {
        DEBUGGER
            .try_with(|dbg| {
                let mut dbg = dbg.borrow_mut();
                if dbg.is_none() {
                    *dbg = Some(TaskDebugger {
                        label: None,
                        registry: HashMap::new(),
                        filter: has_label,
                        task_count: Arc::new(AtomicUsize::new(0)),
                        current_task: None,
                        context: Vec::new(),
                    });
                }
                f(dbg.as_mut().unwrap())
            })
            .ok()
    }

    pub(crate) fn counter() -> Arc<AtomicUsize> {
        let counter = Self::try_with(|dbg| Arc::clone(&dbg.task_count))
            .unwrap_or_else(|| Arc::new(AtomicUsize::new(0)));
        counter.fetch_add(1, Ordering::Relaxed);
        counter
    }

    pub(crate) fn register(ptr: *const ()) -> bool {
        Self::try_with(|dbg| {
            let label = dbg.label.take();
            if (dbg.filter)(label) {
                dbg.registry.insert(ptr, TaskInfo::new(ptr, label));
                true
            } else {
                false
            }
        })
        .unwrap_or(false)
    }

    pub(crate) fn detach(ptr: *const ()) {
        Self::try_with(|dbg| {
            dbg.registry.remove(&ptr);
        });
    }

    #[allow(dead_code)]
    pub(crate) fn update(ptr: *const ()) {
        Self::try_with(|dbg| {
            if let Some(info) = dbg.registry.get_mut(&ptr) {
                if dbg.label.is_some() {
                    info.label = dbg.label;
                }
            }
        });
    }

    pub(crate) fn enter(ptr: *const (), ctx: &'static str) -> bool {
        if unsafe { (*(ptr as *const Header)).owner_thread } != std::thread::current().id() {
            return false;
        }

        Self::try_with(|dbg| {
            if let Some(info) = dbg.registry.get(&ptr) {
                dbg.context.push(ctx);
                dbg.debug_task(info, "");
                return true;
            }
            false
        })
        .unwrap_or(false)
    }

    pub(crate) fn leave() {
        Self::try_with(|dbg| {
            dbg.context.pop();
        });
    }

    fn debug_task(&self, info: &TaskInfo, msg: &str) {
        self.debug_task_info(info, msg);
    }

    fn debug_task_info(&self, info: &TaskInfo, msg: &str) {
        let header = unsafe { &*(info.ptr as *const Header) };
        log::debug!(
            "[{:?}] [{}] [label:{}] [{}] {}",
            info.ptr,
            header.to_compact_string(),
            info.label.unwrap_or(""),
            self.context.join("|"),
            msg,
        )
    }

    pub(crate) fn set_current_task(ptr: *const ()) {
        Self::try_with(|dbg| {
            dbg.current_task = Some(ptr);
        });
    }
}

#[derive(Debug)]
struct TaskInfo {
    ptr: *const (),
    label: Option<&'static str>,
    ts: Instant,
}

impl TaskInfo {
    fn new(ptr: *const (), label: Option<&'static str>) -> Self {
        Self {
            ptr,
            label,
            ts: Instant::now(),
        }
    }
}

fn has_label(label: Option<&'static str>) -> bool {
    label.is_some()
}
