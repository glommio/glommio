//! Test-only observers for task allocations and thread-local destructors.

use std::{
    alloc::Layout,
    collections::HashMap,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    task::{Context, Poll, Waker},
    thread::{self, ThreadId},
};

use crate::{task::JoinHandle, LocalExecutor, LocalExecutorBuilder, PoolPlacement};

fn allocations() -> &'static Mutex<HashMap<usize, Weak<AtomicUsize>>> {
    static ALLOCATIONS: OnceLock<Mutex<HashMap<usize, Weak<AtomicUsize>>>> = OnceLock::new();
    ALLOCATIONS.get_or_init(Mutex::default)
}

/// Observes the allocator release independently of task debugger registration.
pub(super) struct AllocationProbe {
    address: usize,
    freed: Arc<AtomicUsize>,
}

impl AllocationProbe {
    fn track(address: usize) -> Self {
        let mut allocations = allocations().lock().unwrap();
        let freed = allocations
            .get(&address)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let freed = Arc::new(AtomicUsize::new(0));
                allocations.insert(address, Arc::downgrade(&freed));
                freed
            });
        Self { address, freed }
    }

    /// The waker must have been obtained from a Glommio task's poll context.
    pub(super) fn track_waker(waker: &Waker) -> Self {
        Self::track(waker.data() as usize)
    }

    pub(super) fn track_handle<R>(handle: &JoinHandle<R>) -> Self {
        Self::track(handle.raw_task.as_ptr() as usize)
    }

    pub(super) fn deallocations(&self) -> usize {
        self.freed.load(Ordering::SeqCst)
    }

    pub(super) fn assert_live(&self) {
        assert_eq!(self.deallocations(), 0, "task allocation freed prematurely");
    }

    pub(super) fn assert_freed(&self) {
        assert_eq!(
            self.deallocations(),
            1,
            "task allocation was not freed exactly once"
        );
    }
}

impl Drop for AllocationProbe {
    fn drop(&mut self) {
        let mut allocations = allocations().lock().unwrap();
        if Arc::strong_count(&self.freed) == 1
            && allocations
                .get(&self.address)
                .is_some_and(|entry| Weak::ptr_eq(entry, &Arc::downgrade(&self.freed)))
        {
            // Failed leak tests must not leave observer entries in the global registry.
            allocations.remove(&self.address);
        }
    }
}

/// Performs the real deallocation before recording it, without holding the map lock.
///
/// # Safety
/// `ptr` and `layout` must satisfy `alloc::alloc::dealloc`'s requirements.
pub(super) unsafe fn deallocate(ptr: *mut u8, layout: Layout) {
    // Remove the old address before it can be reused by another allocation.
    let observer = allocations()
        .lock()
        .unwrap()
        .remove(&(ptr as usize))
        .and_then(|entry| entry.upgrade());
    alloc::alloc::dealloc(ptr, layout);
    if let Some(observer) = observer {
        observer.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Drops {
    count: AtomicUsize,
    wrong_thread: AtomicUsize,
}

/// A thread-safe observer; its guards deliberately cannot be sent between threads.
#[derive(Clone)]
pub(super) struct DropProbe {
    owner: ThreadId,
    drops: Arc<Drops>,
}

impl DropProbe {
    pub(super) fn new() -> Self {
        Self {
            owner: thread::current().id(),
            drops: Arc::default(),
        }
    }

    pub(super) fn guard(&self) -> DropGuard {
        DropGuard {
            probe: self.clone(),
            _local: PhantomData,
        }
    }

    pub(super) fn drops(&self) -> usize {
        self.drops.count.load(Ordering::SeqCst)
    }

    pub(super) fn assert_not_dropped(&self) {
        assert_eq!(self.drops(), 0, "local resource dropped prematurely");
    }

    pub(super) fn assert_dropped_once(&self) {
        assert_eq!(
            self.drops(),
            1,
            "local resource was not dropped exactly once"
        );
        assert_eq!(
            self.drops.wrong_thread.load(Ordering::SeqCst),
            0,
            "local resource was dropped on a foreign thread",
        );
    }
}

pub(super) struct DropGuard {
    probe: DropProbe,
    _local: PhantomData<Rc<()>>,
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        // Never panic here: task destructors are wrapped in abort_on_panic.
        if thread::current().id() != self.probe.owner {
            self.probe.drops.wrong_thread.fetch_add(1, Ordering::SeqCst);
        }
        self.probe.drops.count.fetch_add(1, Ordering::SeqCst);
    }
}

pub(super) fn executor() -> LocalExecutor {
    LocalExecutorBuilder::default()
        .io_memory(0)
        .blocking_thread_pool_placement(PoolPlacement::Unbound(1))
        .make()
        .expect("failed to create test executor")
}

struct ReturnWaker<const N: usize> {
    _guard: DropGuard,
    _padding: [u8; N],
}

impl<const N: usize> Future for ReturnWaker<N> {
    type Output = Waker;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(cx.waker().clone())
    }
}

pub(super) fn completed_waker<const N: usize>(
    executor: &LocalExecutor,
) -> (Waker, AllocationProbe, DropProbe) {
    let future_drop = DropProbe::new();
    let future = ReturnWaker {
        _guard: future_drop.guard(),
        _padding: [0; N],
    };
    assert_eq!(std::mem::size_of_val(&future) >= 2048, N >= 2048);
    let waker = executor.run(future);
    let allocation = AllocationProbe::track_waker(&waker);
    future_drop.assert_dropped_once();
    allocation.assert_live();
    (waker, allocation, future_drop)
}
