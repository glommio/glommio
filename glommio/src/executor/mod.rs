//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
//! Async executor.
//!
//! This crate offers two kinds of executors: single-threaded and
//! multi-threaded.
//!
//! # Examples
//!
//! Run four single-threaded executors concurrently:
//!
//! ```
//! use glommio::{
//!     timer::Timer,
//!     LocalExecutor,
//!     LocalExecutorBuilder,
//!     LocalExecutorPoolBuilder,
//!     PoolPlacement,
//! };
//!
//! LocalExecutorPoolBuilder::new(PoolPlacement::Unbound(4))
//!     .on_all_shards(move || async {
//!         Timer::new(std::time::Duration::from_millis(100)).await;
//!         println!("Hello world!");
//!     })
//!     .expect("failed to spawn local executors")
//!     .join_all();
//! ```

#![warn(missing_docs, missing_debug_implementations)]

use crate::{
    error::BuilderErrorKind,
    executor::stall::StallDetector,
    io::DmaBuffer,
    reactor,
    sys::{self, blocking::BlockingThreadPool},
    task::{self, registry::TaskRegistry},
    GlommioError, IoRequirements, IoStats, Latency, Shares,
};
use ahash::AHashMap;
use futures_lite::pin;
use latch::{Latch, LatchState};
use log::warn;
pub use placement::{CpuSet, Placement, PoolPlacement};
use smallvec::SmallVec;
use std::{
    cell::RefCell,
    collections::{btree_map, BTreeMap, BinaryHeap, VecDeque},
    fmt,
    future::Future,
    io,
    marker::PhantomData,
    mem,
    ops::Deref,
    pin::Pin,
    rc::Rc,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    thread::{Builder, JoinHandle},
    time::{Duration, Instant},
};
use tracing::trace;

mod context;
mod latch;
#[cfg(any(not(nightly), not(feature = "native-tls")))]
mod local_context;
mod multitask;
mod placement;
pub mod stall;
#[cfg(test)]
mod window_model;

pub(crate) use context::{ExecutorContext, WeakExecutorContext};

pub(crate) const DEFAULT_EXECUTOR_NAME: &str = "unnamed";
pub(crate) const DEFAULT_PREEMPT_TIMER: Duration = Duration::from_millis(100);
pub(crate) const DEFAULT_IO_MEMORY: usize = 10 << 20;
pub(crate) const DEFAULT_RING_SUBMISSION_DEPTH: usize = 128;
/// Bytes per provided receive buffer: what a buffered stream allocates for
/// itself, so a full fill means the same thing either way.
pub(crate) const DEFAULT_RECV_BUFFER_SIZE: usize = crate::net::Preallocated::DEFAULT_BUFFER_SIZE;

/// The receive buffers an executor lends to the kernel for multishot
/// receive: `count` buffers of `size` bytes, `count * size` bytes per
/// executor, allocated at start. See [`LocalExecutorBuilder::recv_buffers`]
/// for what they do and when to want them. Three ways to say it:
///
/// - [`for_connections`](Self::for_connections), from the workload: how many
///   connections, how many arrivals each may have outstanding between two
///   visits to the ring, and the size of a message;
/// - [`budget`](Self::budget), from the memory to spend per executor;
/// - [`exact`](Self::exact), the two numbers, for a workload already measured.
///
/// Whichever way, [`count`](Self::count) and [`size`](Self::size) say what
/// was resolved, worth a line in the startup log, and
/// [`RingIoStats::recv_buffers_exhausted`](crate::RingIoStats::recv_buffers_exhausted)
/// says afterwards whether it was enough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecvBuffers {
    count: usize,
    size: usize,
}

impl RecvBuffers {
    /// The kernel's limit on a buffer ring: 32768 entries.
    pub const MAX_COUNT: usize = 32768;
    /// The kernel's limit on a buffer: its length is 32 bits.
    pub const MAX_SIZE: usize = u32::MAX as usize;
    /// The size used when none is given: 8 KiB, the buffer a buffered stream
    /// allocates for itself, so a full fill means the same thing either way.
    pub const DEFAULT_SIZE: usize = DEFAULT_RECV_BUFFER_SIZE;

    /// No buffers and no multishot, which is the default.
    #[must_use = "The buffers must be given to a builder to be useful"]
    pub fn none() -> Self {
        RecvBuffers {
            count: 0,
            size: Self::DEFAULT_SIZE,
        }
    }

    /// `count` buffers of `size` bytes, exactly. A count of zero is
    /// [`none`](Self::none), whatever the size.
    ///
    /// # Panics
    ///
    /// If `count` is above [`MAX_COUNT`](Self::MAX_COUNT), or `size` is zero
    /// or above [`MAX_SIZE`](Self::MAX_SIZE), with a non-zero count.
    #[must_use = "The buffers must be given to a builder to be useful"]
    pub fn exact(count: usize, size: usize) -> Self {
        if count == 0 {
            return Self::none();
        }
        assert!(
            count <= Self::MAX_COUNT,
            "recv_buffers: count {count} is above the kernel's limit of {}",
            Self::MAX_COUNT
        );
        Self::check_size(size);
        RecvBuffers { count, size }
    }

    /// As many buffers of [`DEFAULT_SIZE`](Self::DEFAULT_SIZE) as fit in
    /// `bytes`, so the budget is never exceeded, and at most
    /// [`MAX_COUNT`](Self::MAX_COUNT). A pool multiplies the budget by its
    /// shard count.
    ///
    /// # Panics
    ///
    /// If `bytes` is below one buffer.
    #[must_use = "The buffers must be given to a builder to be useful"]
    pub fn budget(bytes: usize) -> Self {
        Self::budget_of(bytes, Self::DEFAULT_SIZE)
    }

    /// As [`budget`](Self::budget), with buffers of `size` bytes.
    ///
    /// # Panics
    ///
    /// If `size` is zero or above [`MAX_SIZE`](Self::MAX_SIZE), or `bytes`
    /// is below one buffer.
    #[must_use = "The buffers must be given to a builder to be useful"]
    pub fn budget_of(bytes: usize, size: usize) -> Self {
        Self::check_size(size);
        assert!(
            bytes >= size,
            "recv_buffers: a budget of {bytes} bytes holds no buffer of {size}"
        );
        RecvBuffers {
            count: (bytes / size).min(Self::MAX_COUNT),
            size,
        }
    }

    /// Sized from the workload: `connections` open at once, each with
    /// [`arrivals_each`](RecvBuffersShape::arrivals_each) outstanding, two
    /// unless said otherwise, of
    /// [`message_size`](RecvBuffersShape::message_size) bytes, 8 KiB unless
    /// said otherwise. Zero connections is [`none`](Self::none). The count
    /// is capped at [`MAX_COUNT`](Self::MAX_COUNT), which a shape above it
    /// does not reach; the exhaustion count in the IO stats says so at run
    /// time.
    ///
    /// The product is an upper bound: the kernel runs deferred receives a
    /// batch at a time and buffers come back between batches, so fewer
    /// usually do. The exhaustion count is the way to size downward.
    #[must_use = "The shape must be given to a builder to be useful"]
    pub fn for_connections(connections: usize) -> RecvBuffersShape {
        RecvBuffersShape {
            connections,
            arrivals_each: 2,
            message_size: Self::DEFAULT_SIZE,
        }
    }

    /// How many buffers; zero means multishot is off.
    pub fn count(&self) -> usize {
        self.count
    }

    /// The bytes in each buffer, the most one completion carries.
    pub fn size(&self) -> usize {
        self.size
    }

    /// The memory this costs per executor. Cannot overflow: the count and
    /// size limits keep the product under 2^47.
    pub fn bytes(&self) -> usize {
        self.count * self.size
    }

    /// The kernel describes a buffer with a 32-bit length; a larger one
    /// would be registered as its length modulo 2^32 and read back whole,
    /// which is a corrupt stream, not an error.
    fn check_size(size: usize) {
        assert!(
            size > 0,
            "recv_buffers: a receive buffer of zero bytes cannot hold a message"
        );
        assert!(
            size <= Self::MAX_SIZE,
            "recv_buffers: a receive buffer of {size} bytes is above the kernel's 32-bit length"
        );
    }
}

/// Off: no buffers, no multishot, and nothing allocated. Multishot trades
/// CPU for throughput on a core that would otherwise idle, which is a
/// choice the application makes, not the runtime.
impl Default for RecvBuffers {
    fn default() -> Self {
        Self::none()
    }
}

/// A [`RecvBuffers`] stated from the workload, see
/// [`RecvBuffers::for_connections`]. Passed to the builder as is, or
/// resolved with [`resolve`](Self::resolve) to read the numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecvBuffersShape {
    connections: usize,
    arrivals_each: usize,
    message_size: usize,
}

impl RecvBuffersShape {
    /// How many arrivals one connection may have outstanding between two
    /// visits to the ring: the pipeline depth for a pipelined protocol, one
    /// or two for request and response. Zero is [`RecvBuffers::none`].
    #[must_use = "The shape must be given to a builder to be useful"]
    pub fn arrivals_each(mut self, arrivals: usize) -> Self {
        self.arrivals_each = arrivals;
        self
    }

    /// The bytes one buffer holds: the protocol's usual message. A larger
    /// message arrives in several buffers.
    ///
    /// # Panics
    ///
    /// If `bytes` is zero or above [`RecvBuffers::MAX_SIZE`], here rather
    /// than in the builder, so the panic names the call that caused it.
    #[must_use = "The shape must be given to a builder to be useful"]
    pub fn message_size(mut self, bytes: usize) -> Self {
        RecvBuffers::check_size(bytes);
        self.message_size = bytes;
        self
    }

    /// The count and size this shape comes to.
    #[must_use = "The buffers must be given to a builder to be useful"]
    pub fn resolve(self) -> RecvBuffers {
        self.into()
    }
}

impl From<RecvBuffersShape> for RecvBuffers {
    fn from(shape: RecvBuffersShape) -> Self {
        let wanted = shape.connections.saturating_mul(shape.arrivals_each);
        RecvBuffers::exact(wanted.min(RecvBuffers::MAX_COUNT), shape.message_size)
    }
}

/// Result type alias that removes the need to specify a type parameter
/// that's only valid in the channel variants of the error. Otherwise, it
/// might be confused with the error (`E`) that a result usually has in
/// the second type parameter.
use crate::Result;

#[cfg(all(nightly, feature = "native-tls"))]
#[thread_local]
static mut LOCAL_EX: *const ExecutorContext = std::ptr::null();

#[cfg(any(not(nightly), not(feature = "native-tls")))]
use local_context::LOCAL_EX;

/// Returns a proxy struct to the [`LocalExecutor`]
#[inline(always)]
pub fn executor() -> ExecutorProxy {
    ExecutorProxy {}
}

/// You probably don't need to call this explicitly unless you have created threads before
/// you constructed an executor. You may also want to call this explicitly if you're sandboxing
/// your app and dropping permissions required to use privated expedited membarrier commands to
/// improve Glommio performance.
///
/// This run some early initialization that may save ~30-80ms if you create threads before you
/// tough Glommio code & don't otherwise register with membarrier. This is an idempotent call
/// that can be invoked as many times, but only really makes sense to do early in main before
/// you've constructed any threads. Additionally, the explicit membarrier registration this
/// attempts is also useful if you're later dropping the privilege required to using private
/// expedited commands so that the membarrier synchronization strategy can be used within
/// the executor hot loop.
///
/// The detailed motivation is that internally glommio prefers to use [membarrier](https://man7.org/linux/man-pages/man2/membarrier.2.html)
/// as a high performance synchronization barrier of the underlying io_uring memory
/// shared with the kernel (if the feature is available). If there exist any threads in the program,
/// registering the barrier has been observed to take a relatively long and highly variable amount
/// of time (as high as 30-80ms). If we register before any threads are constructed, this takes almost no time.
/// That's why this can be useful if you're integrating Glommio into an existing application to move this to
/// the front of main.
///
/// The suspicion is that the membarrier registration makes a synchronous IPI to enable the use of cheap
/// asynchronous IPI within the reactor hot loop. This means the startup cost will depend on the specific
/// number of cores; 30-80ms was observed on a 32-core machine running a high end Intel consumer CPU.
/// That's probably on the high end for consumer machines and smartphones today and on the low-end for
/// server-class hardware.
pub fn early_init() {
    sys::initialize_membarrier_strategy();
}

pub(crate) fn executor_id() -> Option<usize> {
    #[cfg(any(not(nightly), not(feature = "native-tls")))]
    {
        LOCAL_EX.try_with(|ex| ex.id)
    }

    #[cfg(all(nightly, feature = "native-tls"))]
    unsafe {
        LOCAL_EX.as_ref().map(|ex| ex.id)
    }
}

#[derive(Default, Debug, Copy, Clone, Eq, PartialEq, Hash)]
/// An opaque handle indicating in which queue a group of tasks will execute.
/// Tasks in the same group will execute in FIFO order but no guarantee is made
/// about ordering on different task queues.
pub struct TaskQueueHandle {
    index: usize,
}

impl TaskQueueHandle {
    /// Returns a numeric ID that uniquely identifies this Task queue
    pub fn index(&self) -> usize {
        self.index
    }
}

#[derive(Debug)]
pub(crate) struct TaskQueue {
    scheduler: Rc<multitask::Scheduler>,
    /// Tasks retain scheduling metadata, while the queue owns runnable tasks.
    runnables: VecDeque<multitask::Runnable>,
    active: bool,
    shares: Shares,
    vruntime: u64,
    /// When this queue last entered the heap of runnable queues, by the
    /// executor's count of entries: between queues of equal vruntime, the
    /// one that entered first runs first.
    entry: u64,
    io_requirements: IoRequirements,
    name: String,
    last_adjustment: Instant,
    /// For dynamic shares classes.
    yielded: bool,
    stats: TaskQueueStats,
}

/// Impl a custom order so we use a min-heap: the least vruntime first, and
/// between queues of equal vruntime the one that entered the heap first.
///
/// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
impl Ord for TaskQueue {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .vruntime
            .cmp(&self.vruntime)
            .then_with(|| other.entry.cmp(&self.entry))
    }
}

impl PartialOrd for TaskQueue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Equal exactly when `Ord` finds them equal.
impl PartialEq for TaskQueue {
    fn eq(&self, other: &Self) -> bool {
        self.vruntime == other.vruntime && self.entry == other.entry
    }
}

impl Eq for TaskQueue {}

impl TaskQueue {
    /// The index this queue is registered under in the executor's queue map.
    ///
    /// Stored in each task's header so its schedule function can find the
    /// queue without capturing a reference to it.
    pub(crate) fn index(&self) -> usize {
        self.stats.index.index()
    }

    fn new<S>(
        index: TaskQueueHandle,
        name: S,
        shares: Shares,
        ioreq: IoRequirements,
        owner_id: usize,
        registry: &Rc<TaskRegistry>,
    ) -> Rc<RefCell<Self>>
    where
        S: Into<String>,
    {
        Rc::new(RefCell::new(TaskQueue {
            scheduler: Rc::new(multitask::Scheduler::new(owner_id, registry)),
            runnables: VecDeque::new(),
            active: false,
            stats: TaskQueueStats::new(index, shares.reciprocal_shares()),
            shares,
            vruntime: 0,
            entry: 0,
            io_requirements: ioreq,
            name: name.into(),
            last_adjustment: Instant::now(),
            yielded: false,
        }))
    }

    fn is_active(&self) -> bool {
        self.active
    }

    fn get_task(&mut self) -> Option<multitask::Runnable> {
        self.runnables.pop_front()
    }

    fn yielded(&self) -> bool {
        self.yielded
    }

    fn prepare_to_run(&mut self, now: Instant) {
        self.yielded = false;
        if let Shares::Dynamic(bm) = &self.shares {
            if now.saturating_duration_since(self.last_adjustment) > bm.adjustment_period() {
                self.last_adjustment = now;
                self.stats.reciprocal_shares = self.shares.reciprocal_shares();
            }
        }
    }

    /// Charges `delta` of runtime to this queue, scaled by its shares, and
    /// returns the new vruntime, or `None` when adding it would overflow.
    ///
    /// The scaling is done in 128 bits: at one share the product passes
    /// `u64::MAX` once a single run exceeds about 73 minutes, though the
    /// shifted result still fits. A delta too large to fit even then saturates.
    ///
    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    fn account_vruntime(&mut self, delta: Duration) -> Option<u64> {
        let delta_scaled =
            (u128::from(self.stats.reciprocal_shares).saturating_mul(delta.as_nanos()) >> 12)
                .try_into()
                .unwrap_or(u64::MAX);
        self.stats.runtime += delta;
        self.stats.queue_selected += 1;
        self.active = !self.runnables.is_empty();

        let vruntime = self.vruntime.checked_add(delta_scaled);
        if let Some(x) = vruntime {
            self.vruntime = x;
        }
        vruntime
    }
}

pub(crate) fn bind_to_cpu_set(cpus: impl IntoIterator<Item = usize>) -> Result<()> {
    let mut cpuset = nix::sched::CpuSet::new();
    for cpu in cpus {
        cpuset.set(cpu).map_err(io::Error::from)?;
    }
    let pid = nix::unistd::Pid::from_raw(0);
    nix::sched::sched_setaffinity(pid, &cpuset).map_err(io::Error::from)?;
    Ok(())
}

#[derive(Debug, Copy, Clone, Default)]
/// Allows information about the current state of this executor to be consumed
/// by applications.
///
/// The counters cover the interval since the stats were last read:
/// [`executor_stats`] returns them and resets them to zero. Two readers each
/// see only part of the total, so keep a single reader and sum its readings
/// for a running total.
///
/// [`executor_stats`]: crate::ExecutorProxy::executor_stats
pub struct ExecutorStats {
    executor_runtime: Duration,
    /// `total_runtime` includes poll io time, excludes spin loop time.
    total_runtime: Duration,
    scheduler_runs: u64,
    tasks_executed: u64,
}

impl ExecutorStats {
    fn new() -> Self {
        Self {
            executor_runtime: Duration::from_nanos(0),
            total_runtime: Duration::from_nanos(0),
            scheduler_runs: 0,
            tasks_executed: 0,
        }
    }

    /// The total amount of runtime in this executor since the stats were last
    /// read.
    ///
    /// This is especially important for spinning executors, since the amount of
    /// CPU time you will see in the operating system will be a far cry from
    /// the CPU time it actually spent executing. Sleeping or Spinning are
    /// not accounted here
    pub fn executor_runtime(&self) -> Duration {
        self.executor_runtime
    }

    /// The total amount of runtime in this executor, plus poll io time
    pub fn total_runtime(&self) -> Duration {
        self.total_runtime
    }

    /// Returns the amount of times the scheduler loop was called. Glommio
    /// scheduler selects a task queue to run and runs many tasks in that
    /// task queue. This number corresponds to the amount of times was
    /// called upon to select a new queue.
    pub fn scheduler_runs(&self) -> u64 {
        self.scheduler_runs
    }

    /// Returns the amount of tasks executed in the system, over all queues.
    pub fn tasks_executed(&self) -> u64 {
        self.tasks_executed
    }
}

#[derive(Debug, Copy, Clone)]
/// Allows information about the current state of a particular task queue to be
/// consumed by applications.
///
/// [`runtime`] and [`queue_selected`] cover the interval since this queue's
/// stats were last read: [`task_queue_stats`] and [`all_task_queue_stats`]
/// both return them and reset them to zero, so reading one also resets what
/// the other will see. Two readers each see only part of the total, so keep
/// a single reader and sum its readings for a running total. [`index`] and
/// [`current_shares`] are not reset.
///
/// [`runtime`]: TaskQueueStats::runtime
/// [`queue_selected`]: TaskQueueStats::queue_selected
/// [`index`]: TaskQueueStats::index
/// [`current_shares`]: TaskQueueStats::current_shares
/// [`task_queue_stats`]: crate::ExecutorProxy::task_queue_stats
/// [`all_task_queue_stats`]: crate::ExecutorProxy::all_task_queue_stats
pub struct TaskQueueStats {
    index: TaskQueueHandle,
    /// So we can easily produce a handle.
    reciprocal_shares: u64,
    queue_selected: u64,
    runtime: Duration,
}

impl TaskQueueStats {
    fn new(index: TaskQueueHandle, reciprocal_shares: u64) -> Self {
        Self {
            index,
            reciprocal_shares,
            runtime: Duration::from_nanos(0),
            queue_selected: 0,
        }
    }

    /// Returns a numeric ID that uniquely identifies this Task queue
    pub fn index(&self) -> TaskQueueHandle {
        self.index
    }

    /// Returns the current number of shares in this task queue.
    ///
    /// If the task queue is configured to use static shares this will never
    /// change. If the task queue is configured to use dynamic shares, this
    /// returns a sample of the shares values the last time the scheduler
    /// ran.
    pub fn current_shares(&self) -> usize {
        ((1u64 << 22) / self.reciprocal_shares) as usize
    }

    /// Returns the runtime this task queue has received since its stats were
    /// last read
    pub fn runtime(&self) -> Duration {
        self.runtime
    }

    /// Returns the number of times this queue was selected to be executed since
    /// its stats were last read. In conjunction with the runtime, you can
    /// extract an average of the amount of time this queue tends to run for
    pub fn queue_selected(&self) -> u64 {
        self.queue_selected
    }

    pub(crate) fn take(&mut self) -> Self {
        std::mem::replace(
            self,
            Self {
                index: self.index,
                reciprocal_shares: self.reciprocal_shares,
                queue_selected: Default::default(),
                runtime: Default::default(),
            },
        )
    }
}

/// A task the window a preempted slice opens holds back: its address, and
/// its runnable while it is held.
///
/// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
#[derive(Debug)]
struct Held {
    /// The task's address. It identifies the task's runnable when it is
    /// queued again and is never dereferenced.
    task: usize,
    /// The task's runnable, held back from its queue until the window ends.
    runnable: Option<multitask::Runnable>,
}

/// The window a slice that `need_preempt` ended opens: the tasks whose polls
/// were under way when the preempt timer fired, held back so that the tasks
/// woken until the window ends go ahead of them.
///
/// A queue runs its tasks in the order they were woken. Such a task is woken
/// in the same reactor pass as the tasks whose wakes came during its slice,
/// or queued itself before that pass, so without the window it could run a
/// whole further slice ahead of tasks that have already waited one.
///
/// Those are the task the run loop polled last, and every task polled inline
/// by `spawn_local` during whose first poll the timer fired. The timer is
/// sampled around every inline poll, so only polls under way when it fired
/// see it fire: a chain of tasks each spawned in the poll of the one before,
/// as long as `spawn_local` calls nest within that first poll, never longer
/// for a deeper queue or a heavier load.
///
/// Opened when the slice ends, or earlier, as soon as an inline poll during
/// which the timer fired returns, and kept until the next round of task
/// queues starts, the executor parks, or `run` returns, whichever comes
/// first.
///
/// While it is open the rest of the slice can run and the reactor runs wakers,
/// and either can spawn. A task polled inline is remembered only if its first
/// poll did not finish it, and it is held, or matched and held when woken,
/// until the window ends, so its address stays its own. The last task can have
/// finished, though, and a task spawned in the reactor pass can then be
/// allocated at its address and be held back in its place. That is tolerated:
/// at most one live task has the address, a held runnable keeps its own address
/// alive so no other task can take it, and the cost falls on that one task. It
/// can wait longer than the pass: one taken for the last task is queued after
/// those polled inline, so it waits one slice per inline task released ahead of
/// it, which is one slice unless inline polls nest.
///
/// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
#[derive(Debug)]
struct Preempted {
    /// The tasks polled inline during whose polls the preempt timer fired,
    /// innermost first: the order their polls returned and they queued
    /// themselves.
    inline: SmallVec<[Held; 2]>,
    /// The task the run loop polled last, once the slice has ended.
    last: Option<Held>,
    /// The least vruntime of the runnable queues as the slice began.
    vruntime: u64,
    /// The index of the queue whose slice it is.
    queue: usize,
}

impl Preempted {
    fn new(vruntime: u64, queue: usize) -> Self {
        Self {
            inline: SmallVec::new(),
            last: None,
            vruntime,
            queue,
        }
    }

    /// The slot for the task at `task`, if the window remembers it.
    ///
    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    fn slot(&mut self, task: usize) -> Option<&mut Option<multitask::Runnable>> {
        self.inline
            .iter_mut()
            .chain(self.last.as_mut())
            .find(|held| held.task == task)
            .map(|held| &mut held.runnable)
    }

    /// Forgets the task at `task` if nothing of it is held, as it is about to
    /// be destroyed, so that a task allocated at its address later is not
    /// taken for it.
    ///
    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    fn forget(&mut self, task: usize) {
        self.inline
            .retain(|held| held.task != task || held.runnable.is_some());
        if self
            .last
            .as_ref()
            .is_some_and(|held| held.task == task && held.runnable.is_none())
        {
            self.last = None;
        }
    }

    /// Whether a runnable of the queue at `index` is held.
    ///
    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    fn holds_for(&self, index: usize) -> bool {
        self.inline
            .iter()
            .chain(self.last.as_ref())
            .filter_map(|held| held.runnable.as_ref())
            .any(|runnable| runnable.task_queue_index() == index)
    }
}

#[derive(Debug)]
struct ExecutorQueues {
    active_executors: BinaryHeap<Rc<RefCell<TaskQueue>>>,
    available_executors: AHashMap<usize, Rc<RefCell<TaskQueue>>>,
    active_executing: Option<Rc<RefCell<TaskQueue>>>,
    executor_index: usize,
    default_vruntime: u64,
    /// How many times a queue has entered `active_executors`: the next
    /// `TaskQueue::entry`. At one entry a nanosecond it would take 584 years
    /// to wrap.
    entries: u64,
    preempted: Option<Preempted>,
    /// Whether a task queue's slice is running. Only then can a window open:
    /// the executor also polls tasks while it cleans up between runs and when
    /// it shuts down, with `active_executing` set but no slice spent.
    ///
    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    in_slice: bool,
    /// The index of the queue whose slice is running, while cleanup during
    /// that slice has made another queue the executing one, so that a window
    /// cleanup opens belongs to the slice that spent the time. It is
    /// `usize::MAX`, which names no queue, while cleanup runs as the slice is
    /// set up, with its queue borrowed, and the end of the slice names it.
    slice_owner: Option<usize>,
    /// How many queues ask for each `Latency::Matters` duration, runnable or
    /// idle, so that the preempt timer follows the tightest of them without a
    /// scan.
    latencies: BTreeMap<Duration, usize>,
    preempt_timer_duration: Duration,
    default_preempt_timer_duration: Duration,
    spin_before_park: Option<Duration>,
    stats: ExecutorStats,
}

impl ExecutorQueues {
    /// The default queue holds index 0, so queues created here are numbered
    /// from 1.
    fn new(preempt_timer_duration: Duration, spin_before_park: Option<Duration>) -> Self {
        ExecutorQueues {
            active_executors: BinaryHeap::new(),
            available_executors: AHashMap::new(),
            active_executing: None,
            executor_index: 1,
            default_vruntime: 0,
            entries: 0,
            preempted: None,
            in_slice: false,
            slice_owner: None,
            latencies: BTreeMap::new(),
            preempt_timer_duration,
            default_preempt_timer_duration: preempt_timer_duration,
            spin_before_park,
            stats: ExecutorStats::new(),
        }
    }

    /// Counts the latency of a queue being created toward the preempt timer.
    fn add_latency(&mut self, latency: Latency) {
        if let Latency::Matters(d) = latency {
            *self.latencies.entry(d).or_insert(0) += 1;
            self.reevaluate_preempt_timer();
        }
    }

    /// Stops counting the latency of a queue being removed.
    fn remove_latency(&mut self, latency: Latency) {
        if let Latency::Matters(d) = latency {
            if let btree_map::Entry::Occupied(mut queues) = self.latencies.entry(d) {
                *queues.get_mut() -= 1;
                if *queues.get() == 0 {
                    queues.remove();
                }
            }
            self.reevaluate_preempt_timer();
        }
    }

    /// Sets the preempt timer to the tightest latency among the queues that
    /// exist, or to the default if that is tighter.
    ///
    /// An idle queue counts as much as a runnable one. Its next event can come
    /// due while another queue holds the CPU, and a timer in the wheel is
    /// noticed only in a reactor pass, which comes when the preempt timer
    /// ends the slice. Counting only runnable queues let a sleeping latency
    /// queue wait out the default timer.
    ///
    /// Runs when a queue is created or removed, the only times the answer can
    /// change, so nothing on the scheduling path pays for it.
    fn reevaluate_preempt_timer(&mut self) {
        let default = self.default_preempt_timer_duration;
        self.preempt_timer_duration = self
            .latencies
            .keys()
            .next()
            .map_or(default, |&tightest| tightest.min(default));
    }

    /// Takes the task at `task` from the back of `queue`, which is where a
    /// task that woke itself during its poll is when the poll returns: it is
    /// queued then, after anything it woke. A task that is not at the back
    /// did not queue itself, and its wake, if any, comes through `schedule`.
    ///
    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    fn take_from_back(queue: &Rc<RefCell<TaskQueue>>, task: usize) -> Option<multitask::Runnable> {
        let mut state = queue.borrow_mut();
        if state
            .runnables
            .back()
            .is_some_and(|runnable| runnable.raw_task.as_ptr().addr() == task)
        {
            state.runnables.pop_back()
        } else {
            None
        }
    }

    /// Remembers a task polled inline by `spawn_local` during whose poll the
    /// preempt timer fired, and holds it if it woke itself, before its
    /// spawner queues anything after it. When inline polls nest, each one
    /// under way when the timer fired gets here as it returns, innermost
    /// first.
    ///
    /// Only within a slice: tasks polled inline while the executor cleans up
    /// between runs or shuts down spent no slice, and no run would end the
    /// window. A task polled inline by cleanup during a slice is found in the
    /// queue cleanup runs it in, and the window belongs to the slice's queue.
    ///
    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    #[cold]
    fn preempted_inline(&mut self, task: usize) {
        if !self.in_slice {
            return;
        }
        let Some(queue) = self.active_executing.clone() else {
            return;
        };
        let vruntime = self.default_vruntime;
        let index = self.slice_owner.unwrap_or_else(|| queue.borrow().index());
        let preempted = self
            .preempted
            .get_or_insert_with(|| Preempted::new(vruntime, index));
        let runnable = Self::take_from_back(&queue, task);
        preempted.inline.push(Held { task, runnable });
    }

    /// Remembers `task`, the task the run loop polled last in a slice of
    /// `queue` that `need_preempt` ended, if it polled any, and `vruntime`,
    /// the least vruntime as that slice began, and holds the task if it
    /// yielded. A slice that ended before it polled any task opens no window
    /// of its own, but keeps one that cleanup opened as it was set up.
    ///
    /// Charges `queue` one step past the slice and names it the window's
    /// queue, so that a queue the window wakes that was charged the same sorts
    /// ahead of it, whether it stays runnable or goes idle and is reactivated
    /// in the window. A run that unwinds is not charged the step, and needs
    /// none: the window ends with the run, before anything is placed in it.
    ///
    /// Returns a runnable held already for a last task, which a window that
    /// ends before the next round cannot have; the caller queues it once the
    /// queues are no longer borrowed rather than dropping it here, where its
    /// destructor would run user code under the borrow.
    ///
    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    fn preempted_last(
        &mut self,
        queue: &Rc<RefCell<TaskQueue>>,
        task: Option<usize>,
        vruntime: u64,
    ) -> Option<multitask::Runnable> {
        if task.is_none() && self.preempted.is_none() {
            return None;
        }
        let runnable = task.and_then(|task| Self::take_from_back(queue, task));
        let index = {
            let mut state = queue.borrow_mut();
            state.vruntime = state.vruntime.saturating_add(1);
            state.index()
        };
        let preempted = self
            .preempted
            .get_or_insert_with(|| Preempted::new(vruntime, index));
        preempted.queue = index;
        let task = task?;
        preempted
            .last
            .replace(Held { task, runnable })
            .and_then(|held| held.runnable)
    }

    /// Ends the open window, if any, and hands it back.
    ///
    /// The window placed the queues it woke by their charge, which can be
    /// below the cached least vruntime, so that is lowered to the least
    /// vruntime of the runnable queues as the window ends. A queue activated
    /// after the window is placed behind the least runnable queue then, as
    /// one activated before it was.
    ///
    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    fn close_window(&mut self) -> Option<Preempted> {
        let preempted = self.preempted.take();
        if preempted.is_some() {
            if let Some(least) = self.active_executors.peek() {
                let least = least.borrow().vruntime;
                self.default_vruntime = self.default_vruntime.min(least);
            }
        }
        preempted
    }

    /// Holds `runnable` if the open window remembers its task, activating its
    /// queue as a wake would so the executor does not go idle while it is
    /// held, and hands it back otherwise.
    ///
    /// Returns a runnable the slot held already, which cannot happen while a
    /// task is queued at most once; the caller queues it once the queues are
    /// no longer borrowed rather than dropping it here.
    ///
    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    #[cold]
    fn hold(
        &mut self,
        queue: &Rc<RefCell<TaskQueue>>,
        runnable: multitask::Runnable,
    ) -> std::result::Result<Option<multitask::Runnable>, multitask::Runnable> {
        let task = runnable.raw_task.as_ptr().addr();
        let displaced = match self
            .preempted
            .as_mut()
            .and_then(|preempted| preempted.slot(task))
        {
            Some(slot) => slot.replace(runnable),
            None => return Err(runnable),
        };
        self.maybe_activate(queue.clone());
        Ok(displaced)
    }

    /// The vruntime a queue that was idle is activated with.
    ///
    /// A queue is activated just behind the least vruntime of the runnable
    /// queues, so it waits for none of them to catch up and gains no credit
    /// for the time it slept. Between a preempted slice and the next round,
    /// though, that least vruntime already includes the slice, and a queue
    /// woken by an event that came during it would sort behind the preempted
    /// queue and wait a second slice. So in that window a queue is placed by
    /// the vruntime it has been charged, no earlier than it would have been
    /// placed when woken during the slice and no later than usual. Being
    /// placed by its own charge, a queue that used up its slice and slept
    /// gets nothing for it, and neither does one that ran a lot before it
    /// slept.
    ///
    /// The queue whose slice it was can be reactivated in the window too,
    /// when its slice ended on a task that awaited something and left it
    /// idle. The end of the slice charged it one step past the slice, so it
    /// sorts behind every queue the window woke that was charged no more than
    /// it, ties included, and it is placed between bounds one step behind the
    /// other queues' bounds: behind any queue the window placed at the
    /// earliest mark, and no further back than just behind the usual place,
    /// so that a queue with few shares is not held back by the charge for the
    /// slice it just spent any more than an idle queue is.
    ///
    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    fn activation_vruntime(&self, queue: &TaskQueue) -> u64 {
        let usual = self.default_vruntime.saturating_add(1);
        let Some(preempted) = self.preempted.as_ref() else {
            return usual;
        };
        let woken = preempted
            .vruntime
            .min(self.default_vruntime)
            .saturating_add(1);
        if preempted.queue == queue.index() {
            return queue
                .vruntime
                .clamp(woken.saturating_add(1), usual.saturating_add(1));
        }
        queue.vruntime.clamp(woken, usual)
    }

    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    fn maybe_activate(&mut self, queue: Rc<RefCell<TaskQueue>>) {
        let mut state = queue.borrow_mut();
        if !state.is_active() {
            state.vruntime = self.activation_vruntime(&state);
            state.active = true;
            drop(state);
            self.push_runnable(queue);
        }
    }

    /// Resets every queue's vruntime to zero, as charging a run overflowed,
    /// and rebuilds the heap: the queues it holds now differ only by when
    /// they entered it, which the order it kept did not follow.
    #[cold]
    #[inline(never)]
    fn reset_vruntimes(&mut self) {
        for queue in self.available_executors.values() {
            queue.borrow_mut().vruntime = 0;
        }
        let heap = std::mem::take(&mut self.active_executors).into_vec();
        self.active_executors = BinaryHeap::from(heap);
    }

    /// Pushes `queue` onto the heap of runnable queues, behind every queue of
    /// the same vruntime already there, so that such queues run in the order
    /// they became runnable. That holds for a queue its slice leaves runnable
    /// too: it goes behind the queues of its vruntime that waited during the
    /// slice, not back to the place it had when the slice was picked.
    ///
    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    fn push_runnable(&mut self, queue: Rc<RefCell<TaskQueue>>) {
        queue.borrow_mut().entry = self.entries;
        self.entries += 1;
        self.active_executors.push(queue);
    }
}

/// A wrapper around a [`std::thread::JoinHandle`]
#[derive(Debug)]
pub struct ExecutorJoinHandle<T: Send + 'static>(JoinHandle<Result<T>>);

impl<T: Send + 'static> ExecutorJoinHandle<T> {
    /// See [`std::thread::JoinHandle::thread()`]
    #[must_use]
    pub fn thread(&self) -> &std::thread::Thread {
        self.0.thread()
    }

    /// See [`std::thread::JoinHandle::join()`]
    pub fn join(self) -> Result<T> {
        match self.0.join() {
            Err(err) => Err(GlommioError::BuilderError(BuilderErrorKind::ThreadPanic(
                err,
            ))),
            Ok(Err(err)) => Err(err),
            Ok(Ok(res)) => Ok(res),
        }
    }
}

/// A factory that can be used to configure and create a [`LocalExecutor`].
///
/// Methods can be chained on it in order to configure it.
///
/// The [`spawn`] method will take ownership of the builder and create a
/// `Result` to the [`LocalExecutor`] handle with the given configuration.
///
/// The [`LocalExecutor::default`] free function uses a Builder with default
/// configuration and unwraps its return value.
///
/// You may want to use [`LocalExecutorBuilder::spawn`] instead of
/// [`LocalExecutor::default`], when you want to recover from a failure to
/// launch a thread. The [`LocalExecutor::default`] function will panic where
/// the Builder method will return a `io::Result`.
///
/// # Examples
///
/// ```
/// use glommio::LocalExecutorBuilder;
///
/// let builder = LocalExecutorBuilder::default();
/// let ex = builder.make().unwrap();
/// ```
///
/// [`LocalExecutor`]: struct.LocalExecutor.html
///
/// [`LocalExecutor::default`]: struct.LocalExecutor.html#method.default
///
/// [`LocalExecutorBuilder::spawn`]:
/// struct.LocalExecutorBuilder.html#method.spawn
///
/// [`spawn`]: struct.LocalExecutorBuilder.html#method.spawn
#[derive(Debug)]
pub struct LocalExecutorBuilder {
    /// The placement policy for the [`LocalExecutor`] to create
    placement: Placement,
    /// Spin for duration before parking a reactor
    spin_before_park: Option<Duration>,
    /// A name for the thread-to-be (if any), for identification in panic
    /// messages
    name: String,
    /// Amount of memory to reserve for storage I/O. This will be preallocated
    /// and registered with io_uring. It is still possible to use more than
    /// that, but it will come from the standard allocator and performance
    /// will suffer. Defaults to 10 MiB.
    io_memory: usize,
    /// The depth of the IO rings to create. This influences the level of IO
    /// concurrency. A higher ring depth allows a shard to submit a
    /// greater number of IO requests to the kernel at once.
    ring_depth: usize,
    /// How often to yield to other task queues
    preempt_timer_duration: Duration,
    /// Whether to record the latencies of individual IO requests
    record_io_latencies: bool,
    /// The placement policy of the blocking thread pool
    /// Defaults to one thread using the same placement strategy as the host
    /// executor
    blocking_thread_pool_placement: PoolPlacement,
    /// Whether to detect stalls in unyielding tasks.
    /// [`stall::DefaultStallDetectionHandler`] installs a signal handler for
    /// [`nix::libc::SIGUSR1`], so is disabled by default.
    detect_stalls: Option<Box<dyn stall::StallDetectionHandler + 'static>>,
    /// Receive buffers lent to the kernel for multishot receive
    recv_buffers: RecvBuffers,
    on_unobserved_panic: UnobservedPanic,
}

impl LocalExecutorBuilder {
    /// Generates the base configuration for spawning a [`LocalExecutor`], from
    /// which configuration methods can be chained.
    /// The method's only argument is the [`Placement`] policy by which the
    /// [`LocalExecutor`] is bound to the machine's hardware topology. i.e.
    /// how many and which CPUs to use.
    pub fn new(placement: Placement) -> LocalExecutorBuilder {
        LocalExecutorBuilder {
            placement: placement.clone(),
            on_unobserved_panic: UnobservedPanic::default(),
            spin_before_park: None,
            name: String::from(DEFAULT_EXECUTOR_NAME),
            io_memory: DEFAULT_IO_MEMORY,
            ring_depth: DEFAULT_RING_SUBMISSION_DEPTH,
            preempt_timer_duration: DEFAULT_PREEMPT_TIMER,
            record_io_latencies: false,
            blocking_thread_pool_placement: PoolPlacement::from(placement),
            detect_stalls: None,
            recv_buffers: RecvBuffers::default(),
        }
    }

    /// Spin for duration before parking a reactor
    #[must_use = "The builder must be built to be useful"]
    pub fn spin_before_park(mut self, spin: Duration) -> LocalExecutorBuilder {
        self.spin_before_park = Some(spin);
        self
    }

    /// Chooses what happens to a blocking closure's panic that no task is
    /// waiting for.
    ///
    /// Defaults to [`UnobservedPanic::Ignore`], which is what `tokio` does for
    /// a task whose handle has been dropped. [`UnobservedPanic::Abort`] gives
    /// `rayon`'s behaviour instead.
    #[must_use = "The builder must be built to be useful"]
    pub fn unobserved_panic(mut self, policy: UnobservedPanic) -> Self {
        self.on_unobserved_panic = policy;
        self
    }

    /// Names the thread-to-be. Currently, the name is used for identification
    /// only in panic messages.
    ///
    /// This applies to [`LocalExecutorBuilder::spawn`] and
    /// [`LocalExecutorPoolBuilder`], which create the threads they name.
    /// [`LocalExecutorBuilder::make`] creates no thread: it builds an executor
    /// that runs on the calling thread, and a thread's name is fixed when it
    /// is created and cannot be changed afterwards. A name given to a builder
    /// that is then `make`d therefore has no effect, and panic messages carry
    /// whatever the calling thread was already called.
    #[must_use = "The builder must be built to be useful"]
    pub fn name(mut self, name: &str) -> LocalExecutorBuilder {
        self.name = String::from(name);
        self
    }

    /// Amount of memory to reserve for storage I/O. This will be preallocated
    /// and registered with io_uring. It is still possible to use more than
    /// that, but it will come from the standard allocator and performance
    /// will suffer.
    ///
    /// The system will always try to allocate at least 64 kiB for I/O memory,
    /// and the default is 10 MiB.
    ///
    /// The registration is charged to the `memlock` resource limit, once for
    /// each of three rings. If it does not fit, the memory is used without
    /// being registered. See [Before using Glommio](crate#before-using-glommio).
    #[must_use = "The builder must be built to be useful"]
    pub fn io_memory(mut self, io_memory: usize) -> LocalExecutorBuilder {
        self.io_memory = io_memory;
        self
    }

    /// The depth of the IO rings to create. This influences the level of IO
    /// concurrency. A higher ring depth allows a shard to submit a
    /// greater number of IO requests to the kernel at once.
    ///
    /// The smallest valid depth is 2, since an operation and its linked
    /// timeout must fit in the ring together. The default is 128.
    #[must_use = "The builder must be built to be useful"]
    pub fn ring_depth(mut self, ring_depth: usize) -> LocalExecutorBuilder {
        assert!(ring_depth >= 2, "ring depth should be at least 2");
        self.ring_depth = ring_depth;
        self
    }

    /// The receive buffers this executor lends to the kernel, a
    /// [`RecvBuffers`] or the shape [`RecvBuffers::for_connections`] gives.
    ///
    /// A buffered [`TcpStream`] or [`UnixStream`] arms one multishot receive
    /// for its lifetime instead of submitting a read per message. The kernel
    /// copies each arrival into one of these buffers and posts a completion,
    /// so the executor submits nothing per message and a reply that was
    /// already waiting is a completion already in the queue. The buffers are
    /// handed back as the stream consumes them; an idle connection holds none.
    ///
    /// `count` bounds how many arrivals the kernel can hold between two visits
    /// to the completion queue, not how many connections can be open. When
    /// they run out the kernel drops the request with `ENOBUFS` and the
    /// stream falls back to a plain read until it re-arms. It is at most
    /// 32768. `size` is the most one completion can carry; a message larger
    /// than it arrives in several.
    ///
    /// A connection may hold what its stream can take at once, the stream's
    /// own buffer or two receive buffers, whichever is more; past that its
    /// request is cancelled and its socket backs up to the peer, so a
    /// connection nobody reads leaves the buffers to the others. That ceiling is
    /// checked as completions are reaped, so it holds only while the
    /// executor keeps visiting the ring. After a stall (a long task within
    /// its slice, a slow syscall), a connection nobody reads takes as many
    /// buffers as the data that queued in its socket meanwhile, up to all of
    /// them. Nothing is lost: the other connections still get every byte in
    /// order, but through the plain read, slower, until that connection is
    /// read or dropped, and [`RingIoStats::recv_buffers_exhausted`] counts
    /// it. Reading every connection you accept, or dropping it, avoids this.
    /// Kernels from 6.17 can bound a multishot receive in the kernel;
    /// glommio does not use that yet.
    ///
    /// The memory is `count * size` per executor, allocated at start, and a
    /// pool of shards multiplies it by the number of shards, which is why
    /// both numbers are here rather than chosen for you. The default is
    /// none: multishot is off, buffered streams submit a read per message,
    /// and nothing is allocated. A `count` of zero says the same. A kernel
    /// older than 6.0 behaves as if it were zero. Multishot pays under
    /// fan-in, many connections into one executor; on a single connection
    /// it buys throughput with CPU the core would otherwise have idled, so
    /// measure before turning it on. [`RecvBuffers::for_connections`] does
    /// the arithmetic from the workload, [`RecvBuffers::budget`] from the
    /// memory, and [`RingIoStats::recv_buffers_exhausted`] says afterwards
    /// whether the count was enough.
    ///
    /// [`RingIoStats::recv_buffers_exhausted`]: crate::RingIoStats::recv_buffers_exhausted
    ///
    /// [`TcpStream`]: crate::net::TcpStream
    /// [`UnixStream`]: crate::net::UnixStream
    #[must_use = "The builder must be built to be useful"]
    pub fn recv_buffers(mut self, buffers: impl Into<RecvBuffers>) -> LocalExecutorBuilder {
        self.recv_buffers = buffers.into();
        self
    }

    /// How often [`need_preempt`] will return true by default.
    ///
    /// Lower values mean task queues will switch execution more often, which
    /// can help latency but harm throughput. When individual task queues
    /// are present, this value can still be dynamically lowered through the
    /// [`Latency`] setting.
    ///
    /// Default is 100ms.
    ///
    /// [`need_preempt`]: ExecutorProxy::need_preempt
    /// [`Latency`]: crate::Latency
    #[must_use = "The builder must be built to be useful"]
    pub fn preempt_timer(mut self, dur: Duration) -> LocalExecutorBuilder {
        self.preempt_timer_duration = dur;
        self
    }

    /// Whether to record the latencies of individual IO requests as part of the
    /// IO stats. Recording latency can be expensive. Disabled by default.
    #[must_use = "The builder must be built to be useful"]
    pub fn record_io_latencies(mut self, enabled: bool) -> LocalExecutorBuilder {
        self.record_io_latencies = enabled;
        self
    }

    /// The placement policy of the blocking thread pool.
    /// Defaults to one thread using the same placement strategy as the host
    /// executor.
    #[must_use = "The builder must be built to be useful"]
    pub fn blocking_thread_pool_placement(
        mut self,
        placement: PoolPlacement,
    ) -> LocalExecutorBuilder {
        self.blocking_thread_pool_placement = placement;
        self
    }

    /// Whether to detect stalls in unyielding tasks.
    /// [`stall::DefaultStallDetectionHandler`] installs a signal handler for
    /// [`nix::libc::SIGUSR1`], so is disabled by default.
    /// # Examples
    ///
    /// ```
    /// use glommio::{DefaultStallDetectionHandler, LocalExecutorBuilder};
    ///
    /// let local_ex = LocalExecutorBuilder::default()
    ///     .detect_stalls(Some(Box::new(DefaultStallDetectionHandler {})))
    ///     .make()
    ///     .unwrap();
    /// ```
    #[must_use = "The builder must be built to be useful"]
    pub fn detect_stalls(
        mut self,
        handler: Option<Box<dyn stall::StallDetectionHandler + 'static>>,
    ) -> Self {
        self.detect_stalls = handler;
        self
    }

    /// Make a new [`LocalExecutor`] by taking ownership of the Builder, and
    /// returns a [`Result`](crate::Result) to the executor.
    /// # Examples
    ///
    /// ```
    /// use glommio::LocalExecutorBuilder;
    ///
    /// let local_ex = LocalExecutorBuilder::default().make().unwrap();
    /// ```
    pub fn make(self) -> Result<LocalExecutor> {
        let notifier = sys::new_sleep_notifier()?;
        let mut cpu_set_gen = placement::CpuSetGenerator::one(self.placement)?;
        let mut le = LocalExecutor::new(
            notifier,
            cpu_set_gen.next().cpu_binding(),
            LocalExecutorConfig {
                io_memory: self.io_memory,
                ring_depth: self.ring_depth,
                preempt_timer: self.preempt_timer_duration,
                record_io_latencies: self.record_io_latencies,
                spin_before_park: self.spin_before_park,
                thread_pool_placement: self.blocking_thread_pool_placement,
                detect_stalls: self.detect_stalls,
                recv_buffers: self.recv_buffers,
                on_unobserved_panic: self.on_unobserved_panic,
            },
        )?;
        le.init();
        Ok(le)
    }

    /// Spawn a new [`LocalExecutor`] in a new thread with a given task.
    ///
    /// This `spawn` function is an ergonomic shortcut for calling
    /// `std::thread::spawn`, [`LocalExecutorBuilder::make`] in the spawned
    /// thread, and then [`LocalExecutor::run`]. This `spawn` function takes
    /// ownership of a [`LocalExecutorBuilder`] with the configuration for
    /// the [`LocalExecutor`], spawns that executor in a new thread, and starts
    /// the task given by `fut_gen()` in that thread.
    ///
    /// The indirection of `fut_gen()` here (instead of taking a `Future`)
    /// allows for futures that may not be `Send`-able once started. As this
    /// executor is thread-local it can guarantee that the futures will not
    /// be Sent once started.
    ///
    /// # Panics
    ///
    /// The newly spawned thread panics if creating the executor fails. If you
    /// need more fine-grained error handling consider initializing those
    /// entities manually.
    ///
    /// # Example
    ///
    /// ```
    /// use glommio::LocalExecutorBuilder;
    ///
    /// let handle = LocalExecutorBuilder::default()
    ///     .spawn(|| async move {
    ///         println!("hello");
    ///     })
    ///     .unwrap();
    ///
    /// handle.join().unwrap();
    /// ```
    ///
    /// [`LocalExecutor`]: struct.LocalExecutor.html
    ///
    /// [`LocalExecutorBuilder`]: struct.LocalExecutorBuilder.html
    ///
    /// [`LocalExecutorBuilder::make`]:
    /// struct.LocalExecutorBuilder.html#method.make
    ///
    /// [`LocalExecutor::run`]:struct.LocalExecutor.html#method.run
    #[must_use = "This spawns an executor on a thread, so you may need to call \
                  `JoinHandle::join()` to keep the main thread alive"]
    pub fn spawn<G, F, T>(self, fut_gen: G) -> Result<ExecutorJoinHandle<T>>
    where
        G: FnOnce() -> F + Send + 'static,
        F: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let notifier = sys::new_sleep_notifier()?;
        let name = format!("{}-{}", self.name, notifier.id());
        let mut cpu_set_gen = placement::CpuSetGenerator::one(self.placement)?;
        let io_memory = self.io_memory;
        let ring_depth = self.ring_depth;
        let preempt_timer_duration = self.preempt_timer_duration;
        let spin_before_park = self.spin_before_park;
        let detect_stalls = self.detect_stalls;
        let on_unobserved_panic = self.on_unobserved_panic;
        let record_io_latencies = self.record_io_latencies;
        let blocking_thread_pool_placement = self.blocking_thread_pool_placement;
        let recv_buffers = self.recv_buffers;

        Builder::new()
            .name(name)
            .spawn(move || {
                let mut le = LocalExecutor::new(
                    notifier,
                    cpu_set_gen.next().cpu_binding(),
                    LocalExecutorConfig {
                        io_memory,
                        ring_depth,
                        preempt_timer: preempt_timer_duration,
                        record_io_latencies,
                        spin_before_park,
                        thread_pool_placement: blocking_thread_pool_placement,
                        detect_stalls,
                        recv_buffers,
                        on_unobserved_panic,
                    },
                )?;
                le.init();
                le.run(async move { Ok(fut_gen().await) })
            })
            .map_err(Into::into)
            .map(ExecutorJoinHandle)
    }
}

impl Default for LocalExecutorBuilder {
    fn default() -> Self {
        Self::new(Placement::Unbound)
    }
}

/// A factory to configure and create a pool of [`LocalExecutor`]s.
///
/// Configuration methods apply their settings to all [`LocalExecutor`]s in the
/// pool unless otherwise specified.  Methods can be chained on the builder in
/// order to configure it.  The [`Self::on_all_shards`] method will take
/// ownership of the builder and create a [`PoolThreadHandles`] struct which can
/// be used to join the executor threads.
///
/// # Example
///
/// ```
/// use glommio::{LocalExecutorPoolBuilder, PoolPlacement};
///
/// let handles = LocalExecutorPoolBuilder::new(PoolPlacement::Unbound(4))
///     .on_all_shards(|| async move {
///         let id = glommio::executor().id();
///         println!("hello from executor {id}");
///     })
///     .unwrap();
///
/// handles.join_all();
/// ```
pub struct LocalExecutorPoolBuilder {
    /// What to do with a blocking panic no task is waiting for.
    on_unobserved_panic: UnobservedPanic,
    /// Spin for duration before parking a reactor
    spin_before_park: Option<Duration>,
    /// A name for the thread-to-be (if any), for identification in panic
    /// messages. Each executor in the pool will use this name followed by
    /// a hyphen and numeric id (e.g. `myname-1`).
    name: String,
    /// Amount of memory to reserve for storage I/O. This will be preallocated
    /// and registered with io_uring. It is still possible to use more than
    /// that, but it will come from the standard allocator and performance
    /// will suffer. Defaults to 10 MiB.
    io_memory: usize,
    /// The depth of the IO rings to create. This influences the level of IO
    /// concurrency. A higher ring depth allows a shard to submit a
    /// greater number of IO requests to the kernel at once.
    ring_depth: usize,
    /// How often to yield to other task queues
    preempt_timer_duration: Duration,
    /// Indicates a policy by which [`LocalExecutor`]s are bound to CPUs.
    placement: PoolPlacement,
    /// Whether to record the latencies of individual IO requests
    record_io_latencies: bool,
    /// The placement policy of the blocking thread pools. Each executor has
    /// its own pool. Defaults to 1 thread per pool, bound using the same
    /// placement strategy as its host executor
    blocking_thread_pool_placement: PoolPlacement,
    /// Factory function to generate the stall detection handler.
    /// [`DefaultStallDetectionHandler installs`] a signal handler for
    /// [`nix::libc::SIGUSR1`], so is disabled by default.
    handler_gen: Option<Box<dyn Fn() -> Box<dyn stall::StallDetectionHandler + 'static>>>,
    /// Receive buffers each executor lends to the kernel for multishot receive
    recv_buffers: RecvBuffers,
}

impl fmt::Debug for LocalExecutorPoolBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalExecutorPoolBuilder")
            .field("spin_before_park", &self.spin_before_park)
            .field("name", &self.name)
            .field("io_memory", &self.io_memory)
            .field("ring_depth", &self.ring_depth)
            .field("preempt_timer_duration", &self.preempt_timer_duration)
            .field("record_io_latencies", &self.record_io_latencies)
            .field(
                "blocking_thread_pool_placement",
                &self.blocking_thread_pool_placement,
            )
            .field("recv_buffers", &self.recv_buffers)
            .finish_non_exhaustive()
    }
}

impl LocalExecutorPoolBuilder {
    /// Generates the base configuration for spawning a pool of
    /// [`LocalExecutor`]s, from which configuration methods can be chained.
    /// The method's only argument is the [`PoolPlacement`] policy by which
    /// [`LocalExecutor`]s are bound to the machine's hardware topology. i.e.
    /// how many and which CPUs to use.
    pub fn new(placement: PoolPlacement) -> Self {
        Self {
            on_unobserved_panic: UnobservedPanic::default(),
            spin_before_park: None,
            name: String::from(DEFAULT_EXECUTOR_NAME),
            io_memory: DEFAULT_IO_MEMORY,
            ring_depth: DEFAULT_RING_SUBMISSION_DEPTH,
            preempt_timer_duration: DEFAULT_PREEMPT_TIMER,
            placement: placement.clone(),
            record_io_latencies: false,
            blocking_thread_pool_placement: placement.shrink_to(1),
            handler_gen: None,
            recv_buffers: RecvBuffers::default(),
        }
    }

    /// Chooses what happens to a blocking closure's panic that no task is
    /// waiting for. See [`LocalExecutorBuilder::unobserved_panic`].
    #[must_use = "The builder must be built to be useful"]
    pub fn unobserved_panic(mut self, policy: UnobservedPanic) -> Self {
        self.on_unobserved_panic = policy;
        self
    }

    /// Please see documentation under
    /// [`LocalExecutorBuilder::spin_before_park`] for details.  The setting
    /// is applied to all executors in the pool.
    #[must_use = "The builder must be built to be useful"]
    pub fn spin_before_park(mut self, spin: Duration) -> Self {
        self.spin_before_park = Some(spin);
        self
    }

    /// Please see documentation under [`LocalExecutorBuilder::name`] for
    /// details. The setting is applied to all executors in the pool. Note
    /// that when a thread is spawned, the `name` is combined with a hyphen
    /// and numeric id (e.g. `myname-1`) such that each thread has a unique
    /// name.
    #[must_use = "The builder must be built to be useful"]
    pub fn name(mut self, name: &str) -> Self {
        self.name = String::from(name);
        self
    }

    /// Please see documentation under [`LocalExecutorBuilder::io_memory`] for
    /// details.  The setting is applied to all executors in the pool.
    #[must_use = "The builder must be built to be useful"]
    pub fn io_memory(mut self, io_memory: usize) -> Self {
        self.io_memory = io_memory;
        self
    }

    /// Please see documentation under [`LocalExecutorBuilder::ring_depth`] for
    /// details.  The setting is applied to all executors in the pool.
    #[must_use = "The builder must be built to be useful"]
    pub fn ring_depth(mut self, ring_depth: usize) -> Self {
        assert!(ring_depth >= 2, "ring depth should be at least 2");
        self.ring_depth = ring_depth;
        self
    }

    /// The receive buffers each executor in the pool lends to the kernel, as
    /// [`LocalExecutorBuilder::recv_buffers`]. The memory is per executor.
    #[must_use = "The builder must be built to be useful"]
    pub fn recv_buffers(mut self, buffers: impl Into<RecvBuffers>) -> Self {
        self.recv_buffers = buffers.into();
        self
    }

    /// Please see documentation under [`LocalExecutorBuilder::preempt_timer`]
    /// for details.  The setting is applied to all executors in the pool.
    #[must_use = "The builder must be built to be useful"]
    pub fn preempt_timer(mut self, dur: Duration) -> Self {
        self.preempt_timer_duration = dur;
        self
    }

    /// Whether to record the latencies of individual IO requests as part of the
    /// IO stats. Recording latency can be expensive. Disabled by default.
    #[must_use = "The builder must be built to be useful"]
    pub fn record_io_latencies(mut self, enabled: bool) -> Self {
        self.record_io_latencies = enabled;
        self
    }

    /// The placement policy of the blocking thread pool.
    /// Defaults to one thread using the same placement strategy as the host
    /// executor.
    #[must_use = "The builder must be built to be useful"]
    pub fn blocking_thread_pool_placement(mut self, placement: PoolPlacement) -> Self {
        self.blocking_thread_pool_placement = placement;
        self
    }

    /// Whether to detect stalls in unyielding tasks.
    /// This method takes a closure of `handler_gen`, which will be called on
    /// each new thread to generate the stall detection handler to be used in
    /// that executor. [`stall::DefaultStallDetectionHandler`] installs a signal
    /// handler for [`nix::libc::SIGUSR1`], so is disabled by default.
    /// # Examples
    ///
    /// ```
    /// use glommio::{
    ///     timer::Timer,
    ///     DefaultStallDetectionHandler,
    ///     LocalExecutorPoolBuilder,
    ///     PoolPlacement,
    /// };
    ///
    /// let local_ex = LocalExecutorPoolBuilder::new(PoolPlacement::Unbound(4))
    ///     .detect_stalls(Some(Box::new(|| Box::new(DefaultStallDetectionHandler {}))))
    ///     .on_all_shards(move || async {
    ///         Timer::new(std::time::Duration::from_millis(100)).await;
    ///         println!("Hello world!");
    ///     })
    ///     .expect("failed to spawn local executors")
    ///     .join_all();
    /// ```
    #[must_use = "The builder must be built to be useful"]
    pub fn detect_stalls(
        mut self,
        handler_gen: Option<Box<dyn Fn() -> Box<dyn stall::StallDetectionHandler + 'static>>>,
    ) -> Self {
        self.handler_gen = handler_gen;
        self
    }

    /// Spawn a pool of [`LocalExecutor`]s in a new thread according to the
    /// [`PoolPlacement`] policy, which is `Unbound` by default.
    ///
    /// This method is the pool equivalent of [`LocalExecutorBuilder::spawn`].
    ///
    /// The method takes a closure `fut_gen` which will be called on each new
    /// thread to obtain the [`Future`] to be executed there.
    ///
    /// # Panics
    ///
    /// The newly spawned thread panics if creating the executor fails. If you
    /// need more fine-grained error handling consider initializing those
    /// entities manually.
    #[must_use = "This spawns executors on multiple threads; threads may fail to spawn or you may \
                  need to call `PoolThreadHandles::join_all()` to keep the main thread alive"]
    pub fn on_all_shards<G, F, T>(self, fut_gen: G) -> Result<PoolThreadHandles<T>>
    where
        G: FnOnce() -> F + Clone + Send + 'static,
        F: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let mut handles = PoolThreadHandles::new();
        let nr_shards = self.placement.executor_count();
        let mut cpu_set_gen = placement::CpuSetGenerator::pool(self.placement.clone())?;
        let latch = Latch::new(nr_shards);

        for _ in 0..nr_shards {
            match self.spawn_thread(&mut cpu_set_gen, &latch, fut_gen.clone()) {
                Ok(handle) => handles.push(handle),
                Err(err) => {
                    let _ = latch.cancel();
                    handles.join_all();
                    return Err(err);
                }
            }
        }

        Ok(handles)
    }

    /// Spawns a thread.
    ///
    /// The thread only creates its [`LocalExecutor`] if all other threads that
    /// are supposed to be created by the pool builder were successfully
    /// spawned.
    ///
    /// If the latch was cancelled, the `Err` produced by the thread isn't
    /// visible to the user; the pool builder directly returns an `Err` from
    /// the `std::thread::Builder`.
    ///
    /// Likewise, if `std::thread::Builder` was unable to spawn the thread and
    /// returned an `Err`, other threads are notified so that they do not
    /// proceed with constructing their `LocalExecutor`s.
    fn spawn_thread<G, F, T>(
        &self,
        cpu_set_gen: &mut placement::CpuSetGenerator,
        latch: &Latch,
        fut_gen: G,
    ) -> Result<JoinHandle<Result<T>>>
    where
        G: FnOnce() -> F + Clone + Send + 'static,
        F: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let cpu_binding = cpu_set_gen.next().cpu_binding();
        let notifier = sys::new_sleep_notifier()?;
        let name = format!("{}-{}", self.name, notifier.id());
        let handle = Builder::new().name(name).spawn({
            let io_memory = self.io_memory;
            let ring_depth = self.ring_depth;
            let preempt_timer_duration = self.preempt_timer_duration;
            let spin_before_park = self.spin_before_park;
            let record_io_latencies = self.record_io_latencies;
            let blocking_thread_pool_placement = self.blocking_thread_pool_placement.clone();
            let detect_stalls = self.handler_gen.as_ref().map(|x| (*x.deref())());
            let recv_buffers = self.recv_buffers;
            let on_unobserved_panic = self.on_unobserved_panic.clone();
            let latch = Latch::clone(latch);

            move || {
                if latch.arrive_and_wait() == LatchState::Ready {
                    let mut le = LocalExecutor::new(
                        notifier,
                        cpu_binding,
                        LocalExecutorConfig {
                            io_memory,
                            ring_depth,
                            preempt_timer: preempt_timer_duration,
                            record_io_latencies,
                            spin_before_park,
                            thread_pool_placement: blocking_thread_pool_placement,
                            detect_stalls,
                            recv_buffers,
                            on_unobserved_panic,
                        },
                    )?;
                    le.init();
                    le.run(async move { Ok(fut_gen().await) })
                } else {
                    Err(io::Error::other("spawn failed").into())
                }
            }
        });

        match handle {
            Ok(h) => Ok(h),
            Err(e) => {
                latch.cancel().expect("unreachable: latch was ready");

                Err(e.into())
            }
        }
    }
}

/// Holds a collection of [`JoinHandle`]s.
///
/// This struct is returned by [`LocalExecutorPoolBuilder::on_all_shards`].
#[derive(Debug)]
pub struct PoolThreadHandles<T> {
    handles: Vec<JoinHandle<Result<T>>>,
}

impl<T> PoolThreadHandles<T> {
    fn new() -> Self {
        Self {
            handles: Vec::new(),
        }
    }

    fn push(&mut self, handle: JoinHandle<Result<T>>) {
        self.handles.push(handle)
    }

    /// Obtain a reference to the `JoinHandle`s.
    pub fn handles(&self) -> &Vec<JoinHandle<Result<T>>> {
        &self.handles
    }

    /// Calls [`JoinHandle::join`] on all handles.
    ///
    /// The `Ok(Err(_))` variant is unreachable since `Err` is only returned
    /// from a thread if another thread failed to spawn;
    /// [`LocalExecutorPoolBuilder::on_all_shards`] returns an immediate `Err`
    /// if any thread fails to spawn, so a `PoolThreadHandles` would never be
    /// created in that case.
    pub fn join_all(self) -> Vec<Result<T>> {
        self.handles
            .into_iter()
            .map(|h| match h.join() {
                Ok(ok @ Ok(_)) => ok,
                Ok(err @ Err(_)) => err,
                Err(e) => Err(GlommioError::BuilderError(BuilderErrorKind::ThreadPanic(e))),
            })
            .collect::<Vec<_>>()
    }
}

/// Borrows the installed context once for scheduling on the owning executor.
pub(super) fn with_executor_context<T>(f: impl FnOnce(Option<&ExecutorContext>) -> T) -> T {
    #[cfg(any(not(nightly), not(feature = "native-tls")))]
    {
        LOCAL_EX.with_optional(f)
    }

    #[cfg(all(nightly, feature = "native-tls"))]
    unsafe {
        f(LOCAL_EX.as_ref())
    }
}

pub struct LocalExecutorConfig {
    pub io_memory: usize,
    pub ring_depth: usize,
    pub preempt_timer: Duration,
    pub record_io_latencies: bool,
    pub spin_before_park: Option<Duration>,
    pub thread_pool_placement: PoolPlacement,
    pub detect_stalls: Option<Box<dyn stall::StallDetectionHandler + 'static>>,
    pub(crate) recv_buffers: RecvBuffers,
    /// What to do with a blocking panic no task is waiting for.
    pub on_unobserved_panic: UnobservedPanic,
}

/// Single-threaded executor.
///
/// The executor can only be run on the thread that created it.
///
/// # Examples
///
/// ```
/// use glommio::LocalExecutor;
///
/// let local_ex = LocalExecutor::default();
///
/// local_ex.run(async {
///     println!("Hello world!");
/// });
/// ```
///
/// In many cases, use of [`LocalExecutorBuilder`] will provide more
/// configuration options and more ergonomic methods. See
/// [`LocalExecutorBuilder::spawn`] for examples.
///
/// [`LocalExecutorBuilder`]: struct.LocalExecutorBuilder.html
///
/// [`LocalExecutorBuilder::spawn`]:
/// struct.LocalExecutorBuilder.html#method.spawn
#[derive(Debug)]
pub struct LocalExecutor {
    context: ExecutorContext,
    /// Shared so a queue run can hold the detector without borrowing this
    /// cell, which a task replaces through [`LocalExecutor::detect_stalls`].
    stall_detector: RefCell<Option<Rc<StallDetector>>>,
}

/// What a pass over the task queues left behind.
///
/// The caller parks on this, and parking is only correct on `Idle`. Work
/// behind an active queue is the executor's own: no I/O completion is coming
/// for it and no peer will write the eventfd, so a sleep entered by mistake
/// ends only if some unrelated event happens to arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskQueueRun {
    /// A queue is still runnable, whether or not this pass got to it.
    Runnable,
    /// Nothing is left to run.
    Idle,
}

impl LocalExecutor {
    /// Gives lifecycle tests access to the executor's task registry.
    #[cfg(test)]
    pub(crate) fn task_registry(&self) -> &Rc<TaskRegistry> {
        &self.context.tasks
    }

    fn init(&mut self) {
        let io_requirements = IoRequirements::new(Latency::NotImportant, 0);
        self.context.queues.borrow_mut().available_executors.insert(
            0,
            TaskQueue::new(
                Default::default(),
                "default",
                Shares::Static(1000),
                io_requirements,
                self.context.id,
                &self.context.tasks,
            ),
        );
    }

    /// Creates a new `LocalExecutor`.
    ///
    /// Linux's default memory policy is "local allocation" which allocates
    /// memory on the NUMA node containing the CPU where the allocation takes
    /// place. Hence, we bind to a CPU in the provided CPU set before
    /// allocating any memory for the `LocalExecutor`, thereby allowing any
    /// access to these data structures to occur on a local NUMA node
    /// (nevertheless, for some `Placement` variants a CPU set could span
    /// multiple NUMA nodes).
    ///
    /// For additional information see
    /// <https://www.kernel.org/doc/html/latest/admin-guide/mm/numa_memory_policy.html>.
    fn new(
        notifier: Arc<sys::SleepNotifier>,
        cpu_binding: Option<impl IntoIterator<Item = usize>>,
        mut config: LocalExecutorConfig,
    ) -> Result<LocalExecutor> {
        let blocking_thread =
            BlockingThreadPool::new(config.thread_pool_placement, notifier.clone())?;

        match cpu_binding {
            Some(cpu_set) => bind_to_cpu_set(cpu_set)?,
            None => config.spin_before_park = None,
        }
        let queues = Rc::new(RefCell::new(ExecutorQueues::new(
            config.preempt_timer,
            config.spin_before_park,
        )));
        let id = notifier.id();
        let reactor = Rc::new(reactor::Reactor::new(
            notifier,
            config.io_memory,
            config.ring_depth,
            config.record_io_latencies,
            blocking_thread,
            config.recv_buffers,
        )?);
        let tasks = Rc::new(TaskRegistry::new(WeakExecutorContext::new(
            id,
            &queues,
            &reactor,
            config.on_unobserved_panic.clone(),
        )));
        trace!(id = id, "Creating executor");
        Ok(LocalExecutor {
            context: ExecutorContext {
                queues,
                tasks,
                id,
                reactor,
                on_unobserved_panic: config.on_unobserved_panic,
            },
            stall_detector: RefCell::new(
                config
                    .detect_stalls
                    .map(|x| StallDetector::new(id, x).map(Rc::new))
                    .transpose()?,
            ),
        })
    }

    /// Enable or disable task stall detection at runtime
    ///
    /// # Examples
    /// ```
    /// use glommio::{DefaultStallDetectionHandler, LocalExecutor};
    ///
    /// let local_ex =
    ///     LocalExecutor::default().detect_stalls(Some(Box::new(DefaultStallDetectionHandler {})));
    /// ```
    pub fn detect_stalls(
        &self,
        handler: Option<Box<dyn stall::StallDetectionHandler + 'static>>,
    ) -> Result<()> {
        self.stall_detector.replace(
            handler
                .map(|x| StallDetector::new(self.context.id, x).map(Rc::new))
                .transpose()?,
        );
        Ok(())
    }

    /// Returns a unique identifier for this Executor.
    ///
    /// # Examples
    /// ```
    /// use glommio::LocalExecutor;
    ///
    /// let local_ex = LocalExecutor::default();
    /// println!("My ID: {}", local_ex.id());
    /// ```
    pub fn id(&self) -> usize {
        self.context.id
    }

    #[cfg(test)]
    fn create_task_queue<S>(&self, shares: Shares, latency: Latency, name: S) -> TaskQueueHandle
    where
        S: Into<String>,
    {
        self.context.create_task_queue(shares, latency, name)
    }

    /// Removes a task queue.
    ///
    /// The task queue cannot be removed if there are still pending tasks.
    ///
    /// The default queue, [`TaskQueueHandle::default()`], cannot be removed
    /// at all: [`spawn`] and [`run`] place work in it. Removing it returns a
    /// [`QueueErrorKind::StillActive`] error.
    ///
    /// [`spawn`]: LocalExecutor::spawn
    /// [`run`]: LocalExecutor::run
    /// [`QueueErrorKind::StillActive`]: crate::error::QueueErrorKind::StillActive
    pub fn remove_task_queue(&self, handle: TaskQueueHandle) -> Result<()> {
        self.context.remove_task_queue(handle)
    }

    /// Spawns a task directly onto this executor instance.
    ///
    /// Unlike [`spawn_local`], this uses the executor you already have a
    /// reference to rather than a thread-local, so it **never panics** and can
    /// be called from any context — including before [`run`] has been called,
    /// which is useful for setting work up in advance.
    ///
    /// This is only reachable from the thread that owns the executor, since
    /// [`LocalExecutor`] is not [`Send`].
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::LocalExecutor;
    ///
    /// let executor = LocalExecutor::default();
    ///
    /// // spawning before run() is fine here; spawn_local would panic
    /// let task = executor.spawn(async { 1 + 2 });
    ///
    /// let result = executor.run(task);
    /// assert_eq!(result, 3);
    /// ```
    ///
    /// [`spawn_local`]: crate::spawn_local
    /// [`run`]: LocalExecutor::run
    pub fn spawn<T>(&self, future: impl Future<Output = T>) -> Task<T> {
        Task(self.context.spawn_internal(future))
    }

    fn preempt_timer_duration(&self) -> Duration {
        self.context.queues.borrow().preempt_timer_duration
    }

    fn spin_before_park(&self) -> Option<Duration> {
        self.context.queues.borrow().spin_before_park
    }

    #[inline(always)]
    pub(crate) fn need_preempt(&self) -> bool {
        self.context.reactor.need_preempt()
    }

    /// Services task queues until preempted or until none is runnable.
    ///
    /// This used to answer whether anything had run, which is a different
    /// question with the same shape: being interrupted before servicing a queue
    /// is not the same as having nothing to run, and the caller parks on the
    /// answer.
    ///
    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    fn run_task_queues(&self) -> TaskQueueRun {
        self.context.end_preempted_window();
        loop {
            self.context.reactor.sys.install_eventfd();
            if self.need_preempt() {
                return if self.context.queues.borrow().active_executors.is_empty() {
                    TaskQueueRun::Idle
                } else {
                    TaskQueueRun::Runnable
                };
            }
            if !self.run_one_task_queue() {
                return TaskQueueRun::Idle;
            }
        }
    }

    /// Runs a single task queue, accounting for its runtime and vruntime.
    ///
    /// Computes the smallest vruntime out of all the active task queues. This
    /// value is used to set the vruntime of deactivated task queues when they
    /// are woken up.
    ///
    /// The guard that ends the run is armed before anything else runs: a
    /// queue's shares manager and the stall detector are user code, and a
    /// panic in either must not leave the run marked as under way. The run is
    /// ended before the stall detector's guard is dropped, since dropping that
    /// guard reports a stall through user code that can take a while, and the
    /// queue must not be charged for it. A run that unwinds drops the stall
    /// guard first.
    ///
    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    fn run_one_task_queue(&self) -> bool {
        let mut tq = self.context.queues.borrow_mut();
        let candidate = tq.active_executors.pop();
        tq.stats.scheduler_runs += 1;

        if candidate.is_none() {
            return false;
        }

        let queue = candidate.unwrap();
        tq.active_executing = Some(queue.clone());
        tq.in_slice = true;
        let least_vruntime = tq.default_vruntime;
        drop(tq);

        let time = Instant::now();
        let mut run = scopeguard::guard((queue, 0, None), |(queue, tasks_executed, preempted)| {
            let preempted = preempted.map(|task| (task, least_vruntime));
            self.finish_task_queue_run(queue, time.elapsed(), tasks_executed, preempted)
        });
        {
            let mut queue_ref = run.0.borrow_mut();
            queue_ref.prepare_to_run(time);
            self.context
                .reactor
                .inform_io_requirements(queue_ref.io_requirements);
        }

        let detector = self.stall_detector.borrow().clone();
        let _stall_guard = detector.as_ref().map(|x| {
            let queue = run.0.borrow_mut();
            x.enter_task_queue(
                queue.stats.index,
                queue.name.clone(),
                time,
                self.preempt_timer_duration(),
            )
        });
        let mut last = None;
        loop {
            if self.need_preempt() {
                run.2 = Some(last);
                break;
            }
            let mut queue_ref = run.0.borrow_mut();
            if queue_ref.yielded() {
                break;
            }

            if let Some(r) = queue_ref.get_task() {
                drop(queue_ref);
                last = Some(r.raw_task.as_ptr().addr());
                r.run();
                run.1 += 1;
            } else {
                break;
            }
        }
        drop(run);
        true
    }

    /// Accounts a queue's run and returns it to the scheduler.
    ///
    /// Runs on unwind as well: a panicking task leaves its queue active, and a
    /// queue left active outside the heap is never pushed again, so the
    /// executor would park forever on its next `run`.
    ///
    /// `preempted` is set if `need_preempt` ended the run: the address of
    /// the task that was running, or `None` if it ended the run before any
    /// task was polled, and the least vruntime as the run began.
    /// The task polled last is remembered alongside any task a window opened
    /// earlier in the run already remembers, from a task's inline first poll.
    /// A queue whose only runnable is held stays runnable, so that it keeps
    /// the vruntime it was charged.
    ///
    /// Only a run that `need_preempt` ended can have a window open when it
    /// returns normally: a window opens during a run only when the timer fires
    /// in an inline poll, by a task or by cleanup as the run is set up, nothing
    /// polls the rings while tasks run, and so the run loop ends the run on
    /// `need_preempt` at its next check, which can come before it polls any
    /// task. A run that unwinds can end before that check, though, so whether
    /// the window holds the queue's only runnable is asked however the run
    /// ended.
    ///
    /// When charging the run overflows, every queue's vruntime is reset to
    /// zero and the heap rebuilt.
    ///
    /// Modelled by the test-only `executor::window_model`: mirror changes there and rerun it.
    fn finish_task_queue_run(
        &self,
        queue: Rc<RefCell<TaskQueue>>,
        runtime: Duration,
        tasks_executed_this_loop: u64,
        preempted: Option<(Option<usize>, u64)>,
    ) {
        let (mut need_repush, vruntime) = {
            let mut state = queue.borrow_mut();
            let last_vruntime = state.account_vruntime(runtime);
            (state.is_active(), last_vruntime)
        };

        let mut tq = self.context.queues.borrow_mut();
        tq.active_executing = None;
        tq.in_slice = false;
        tq.stats.executor_runtime += runtime;
        tq.stats.tasks_executed += tasks_executed_this_loop;
        let vruntime = match vruntime {
            Some(x) => x,
            None => {
                tq.reset_vruntimes();
                0
            }
        };
        let displaced = if let Some((last, least)) = preempted {
            tq.preempted_last(&queue, last, least)
        } else {
            debug_assert!(
                tq.preempted.is_none() || std::thread::panicking(),
                "a window opened in a slice that need_preempt did not end"
            );
            None
        };
        if !need_repush && tq.preempted.is_some() {
            let index = queue.borrow().index();
            if tq.preempted.as_ref().is_some_and(|p| p.holds_for(index)) {
                queue.borrow_mut().active = true;
                need_repush = true;
            }
        }

        if need_repush {
            tq.push_runnable(queue);
        }

        tq.default_vruntime = tq
            .active_executors
            .peek()
            .map(|x| x.borrow().vruntime)
            .unwrap_or(vruntime);
        drop(tq);
        if let Some(displaced) = displaced {
            self.context.requeue(displaced);
        }
    }

    /// Runs the executor until the given future completes.
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::{LocalExecutor, Task};
    ///
    /// let local_ex = LocalExecutor::default();
    ///
    /// let res = local_ex.run(async {
    ///     let task = glommio::spawn_local(async { 1 + 2 });
    ///     task.await * 2
    /// });
    ///
    /// assert_eq!(res, 6);
    /// ```
    /// # Notes
    ///
    /// The main future can't be canceled, and its join handle is `None` only
    /// upon cancellation or panic, so in case of panic this just propagates.
    pub fn run<T>(&self, future: impl Future<Output = T>) -> T {
        let run = |this: &Self| {
            let _window = scopeguard::guard((), |()| this.context.end_preempted_window());
            let cx = &mut Context::from_waker(Waker::noop());

            let spin_before_park = self.spin_before_park().unwrap_or_default();

            let future = this
                .context
                .spawn_into(future, TaskQueueHandle::default())
                .unwrap()
                .detach();
            pin!(future);

            let mut pre_time = Instant::now();
            loop {
                if let Poll::Ready(t) = future.as_mut().poll(cx) {
                    let cur_time = Instant::now();
                    this.context.queues.borrow_mut().stats.total_runtime += cur_time - pre_time;
                    break t.unwrap();
                }

                this.context
                    .reactor
                    .react(|| Some(this.preempt_timer_duration()), false)
                    .expect("Failed to poll io! This is actually pretty bad!");

                let queues = this.run_task_queues();

                let cur_time = Instant::now();
                this.context.queues.borrow_mut().stats.total_runtime += cur_time - pre_time;
                pre_time = cur_time;
                if queues == TaskQueueRun::Idle {
                    if let Poll::Ready(t) = future.as_mut().poll(cx) {
                        break t.unwrap();
                    } else {
                        while !this.context.reactor.spin_poll_io().unwrap() {
                            if pre_time.elapsed() > spin_before_park {
                                this.context.end_preempted_window();
                                #[cfg(test)]
                                if let Some(hook) =
                                    BEFORE_PARK.with(|hook| hook.borrow_mut().take())
                                {
                                    hook();
                                }
                                debug_assert!(
                                    this.context.queues.borrow().active_executors.is_empty(),
                                    "parking with {} runnable task queues: nothing will wake us",
                                    this.context.queues.borrow().active_executors.len()
                                );
                                this.context
                                    .reactor
                                    .react(|| None, true)
                                    .expect("Failed to park! This is actually pretty bad!");
                                break;
                            }
                        }
                        pre_time = Instant::now();
                    }
                }
            }
        };

        assert!(
            executor_id().is_none(),
            "There is already an LocalExecutor running on this thread"
        );
        self.context.enter(|| run(self))
    }
}

impl Drop for LocalExecutor {
    fn drop(&mut self) {
        let shutdown = || {
            sys::get_sleep_notifier_for(self.context.id)
                .expect("executor's sleep notifier disappeared before shutdown")
                .close_foreign_wakes();
            self.context.tasks.request_shutdown();
        };

        self.context.with_cleanup(None, shutdown);
    }
}

/// Spawns a single-threaded executor with default settings on the current
/// thread.
///
/// This will create a executor using default parameters of
/// `LocalExecutorBuilder`, if you want to further customize it, use this API
/// instead.
///
/// # Panics
///
/// Panics if creating the executor fails; use `LocalExecutorBuilder::make` to
/// recover from such errors.
///
/// # Examples
///
/// ```
/// use glommio::LocalExecutor;
///
/// let local_ex = LocalExecutor::default();
/// ```
impl Default for LocalExecutor {
    fn default() -> Self {
        LocalExecutorBuilder::new(Placement::Unbound)
            .make()
            .unwrap()
    }
}

/// A spawned future that can be detached
///
/// Tasks are also futures themselves and yield the output of the spawned
/// future.
///
/// When a task is dropped, its gets canceled and won't be polled again. To
/// cancel a task a bit more gracefully and wait until it stops running, use the
/// [`cancel()`][`Task::cancel()`] method.
///
/// Tasks that panic get immediately canceled. Awaiting a canceled task also
/// causes a panic.
///
/// # Examples
///
/// ```
/// # use glommio::{LocalExecutor, Task};
/// #
/// # let ex = LocalExecutor::default();
/// #
/// # ex.run(async {
/// let task = glommio::spawn_local(async {
///     println!("Hello from a task!");
///     1 + 2
/// });
///
/// assert_eq!(task.await, 3);
/// # });
/// ```
/// Note that there is no guarantee of ordering when reasoning about when a
/// task runs, as that is an implementation detail.
///
/// In particular, acquiring a borrow and holding across a task spawning may
/// sometimes work but panic depending on scheduling decisions, so it is still
/// illegal.
///
///
/// ```no_run
/// # use glommio::{LocalExecutor, Task};
/// # use std::rc::Rc;
/// # use std::cell::RefCell;
/// #
/// # let ex = LocalExecutor::default();
/// #
/// # ex.run(async {
/// let example = Rc::new(RefCell::new(0));
/// let exclone = example.clone();
///
/// let mut ex_mut = example.borrow_mut();
/// *ex_mut = 1;
///
/// let task = glommio::spawn_local(async move {
///     let ex = exclone.borrow();
///     println!("Current value: {ex}");
/// });
///
/// // This is fine if `task` executes after the current task, but will panic if
/// // preempts the current task and executes first. This is therefore invalid.
/// *ex_mut = 2;
/// drop(ex_mut);
///
/// task.await;
/// # });
/// ```
#[must_use = "tasks get canceled when dropped, use `.detach()` to run them in the background"]
#[derive(Debug)]
pub struct Task<T>(multitask::Task<T>);

impl<T> Task<T> {
    /// Detaches the task to let it keep running in the background.
    ///
    /// # Examples
    ///
    /// ```
    /// use futures_lite::future;
    /// use glommio::{timer::Timer, LocalExecutor};
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     glommio::spawn_local(async {
    ///         loop {
    ///             println!("I'm a background task looping forever.");
    ///             glommio::executor().yield_task_queue_now().await;
    ///         }
    ///     })
    ///     .detach();
    ///     Timer::new(std::time::Duration::from_micros(100)).await;
    /// })
    /// ```
    pub fn detach(self) -> task::JoinHandle<T> {
        self.0.detach()
    }

    /// Cancels the task and waits for it to stop running.
    ///
    /// Returns the task's output if it was completed just before it got
    /// canceled, or [`None`] if it didn't complete.
    ///
    /// While it's possible to simply drop the [`Task`] to cancel it, this is a
    /// cleaner way of canceling because it also waits for the task to stop
    /// running.
    ///
    /// # Examples
    ///
    /// ```
    /// use futures_lite::future;
    /// use glommio::LocalExecutor;
    ///
    /// let ex = LocalExecutor::default();
    ///
    /// ex.run(async {
    ///     let task = glommio::spawn_local(async {
    ///         loop {
    ///             println!("Even though I'm in an infinite loop, you can still cancel me!");
    ///             future::yield_now().await;
    ///         }
    ///     });
    ///
    ///     task.cancel().await;
    /// });
    /// ```
    pub async fn cancel(self) -> Option<T> {
        self.0.cancel().await
    }
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

/// A spawned future that cannot be detached, and has a predictable lifetime.
///
/// Because their lifetimes are bounded, you don't need to make sure that data
/// you pass to the `ScopedTask` is `'static`, which can be cheaper (no need to
/// reference count). If you, however, would like to `.detach` this task and
/// have it run in the background, consider using [`Task`] instead.
///
/// Tasks are also futures themselves and yield the output of the spawned
/// future.
///
/// When a task is dropped, it gets canceled and its future is dropped before
/// `drop` returns, so nothing the future borrows is used after the scope ends.
/// To cancel a task and get its output if it had already completed, use the
/// [`cancel()`][`ScopedTask::cancel()`] method.
///
/// Tasks that panic get immediately canceled. Awaiting a canceled task also
/// causes a panic.
///
/// # Safety
///
/// `ScopedTask` is safe to use so long as it is guaranteed to be either awaited
/// or dropped. Rust does not guarantee that destructors will be called, and if
/// they are not, `ScopedTask`s can be kept alive after the scope is terminated.
///
/// Typically, the only situations in which `drop` is not executed are:
///
/// * If you manually choose not to, with [`std::mem::forget`] or
///   [`ManuallyDrop`].
/// * If cyclic reference counts prevents the task from being destroyed.
///
/// If you believe any of the above situations are present (the first one is,
/// of course, considerably easier to spot), avoid using the `ScopedTask`.
///
/// # Examples
///
/// ```
/// use glommio::LocalExecutor;
///
/// let ex = LocalExecutor::default();
///
/// ex.run(async {
///     let a = 2;
///     let task = unsafe {
///         glommio::spawn_scoped_local(async {
///             println!("Hello from a task!");
///             1 + a // this is a reference, and it works just fine
///         })
///     };
///
///     assert_eq!(task.await, 3);
/// });
/// ```
/// The usual borrow checker rules apply. A [`ScopedTask`] can acquire a mutable
/// reference to a variable just fine:
///
/// ```
/// # use glommio::{LocalExecutor};
/// #
/// # let ex = LocalExecutor::default();
/// # ex.run(async {
/// let mut a = 2;
/// let task = unsafe {
///     glommio::spawn_scoped_local(async {
///         a = 3;
///     })
/// };
/// task.await;
/// assert_eq!(a, 3);
/// # });
/// ```
///
/// But until the task completes, the reference is mutably held, so we can no
/// longer immutably reference it:
///
/// ```compile_fail
/// # use glommio::LocalExecutor;
/// #
/// # let ex = LocalExecutor::default();
/// # ex.run(async {
/// let mut a = 2;
/// let task = unsafe {
///     glommio::scoped_local(async {
///         a = 3;
///     })
/// };
/// assert_eq!(a, 3); // task hasn't completed yet!
/// task.await;
/// # });
/// ```
///
/// You can still use [`Cell`] and [`RefCell`] normally to work around this.
/// Just keep in mind that there is no guarantee of ordering for execution of
/// tasks, and if the task has not yet finished the value may or may not have
/// changed (as with any interior mutability)
///
/// ```
/// # use glommio::{LocalExecutor};
/// # use std::cell::Cell;
/// #
/// # let ex = LocalExecutor::default();
/// # ex.run(async {
/// let a = Cell::new(2);
/// let task = unsafe {
///     glommio::spawn_scoped_local(async {
///         a.set(3);
///     })
/// };
///
/// assert!(a.get() == 3 || a.get() == 2); // impossible to know if it will be 2 or 3
/// task.await;
/// assert_eq!(a.get(), 3); // The task finished now.
/// //
/// # });
/// ```
///
/// The following code, however, will access invalid memory as drop is never
/// executed
///
/// ```no_run
/// # use glommio::{LocalExecutor};
/// # use std::cell::Cell;
/// #
/// # let ex = LocalExecutor::default();
/// # ex.run(async {
/// {
///     let a = &mut "mayhem";
///     let task = unsafe {
///         glommio::spawn_scoped_local(async {
///             *a = "doom";
///         })
///     };
///     std::mem::forget(task);
/// }
/// # });
/// ```
///
/// [`Task`]: crate::Task
/// [`Cell`]: std::cell::Cell
/// [`RefCell`]: std::cell::RefCell
/// [`std::mem::forget`]: std::mem::forget
/// [`ManuallyDrop`]: std::mem::ManuallyDrop
#[must_use = "scoped tasks get canceled when dropped, use a standard Task and `.detach()` to run \
              them in the background"]
#[derive(Debug)]
pub struct ScopedTask<'a, T>(multitask::Task<T>, PhantomData<&'a T>);

impl<T> ScopedTask<'_, T> {
    /// Cancels the task and waits for it to stop running.
    ///
    /// Returns the task's output if it was completed just before it got
    /// canceled, or [`None`] if it didn't complete.
    ///
    /// While it's possible to simply drop the [`ScopedTask`] to cancel it, this
    /// is a cleaner way of canceling because it also waits for the task to
    /// stop running.
    ///
    /// # Examples
    ///
    /// ```
    /// use futures_lite::future;
    /// use glommio::LocalExecutor;
    ///
    /// let ex = LocalExecutor::default();
    ///
    /// ex.run(async {
    ///     let task = unsafe {
    ///         glommio::spawn_scoped_local(async {
    ///             loop {
    ///                 println!("Even though I'm in an infinite loop, you can still cancel me!");
    ///                 future::yield_now().await;
    ///             }
    ///         })
    ///     };
    ///
    ///     task.cancel().await;
    /// });
    /// ```
    pub async fn cancel(mut self) -> Option<T> {
        let task = self.0.take();
        task.cancel_in_place();
        task.cancel().await
    }
}

impl<T> Drop for ScopedTask<'_, T> {
    /// The future may borrow from the scope that is ending, so unlike a
    /// [`Task`]'s it cannot be left for the executor to drop later.
    fn drop(&mut self) {
        self.0.cancel_in_place();
    }
}

impl<T> Future for ScopedTask<'_, T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

/// Conditionally yields the current task queue. The scheduler may then
/// process other task queues according to their latency requirements.
/// If a call to this function results in the current queue to yield,
/// then the calling task is moved to the back of the yielded task
/// queue.
///
/// Under which condition this function yield is an implementation detail
/// subject to change, but it will always be somehow related to the latency
/// guarantees that the task queues want to uphold in their
/// `Latency::Matters` parameter (or `Latency::NotImportant`).
///
/// This function is the central mechanism of task cooperation in Glommio
/// and should be preferred over unconditional yielding methods like
/// [`ExecutorProxy::yield_now`] and
/// [`ExecutorProxy::yield_task_queue_now`].
#[inline(always)]
pub async fn yield_if_needed() {
    executor().yield_if_needed().await
}

/// Spawns a task onto the current single-threaded executor.
///
/// If called from a [`LocalExecutor`], the task is spawned on it.
/// Otherwise, this method panics.
///
/// Note that there is no guarantee of when the spawned task is scheduled.
/// The current task can continue its execution or be preempted by the
/// newly spawned task immediately. See the documentation for the
/// top-level [`Task`] for examples.
///
/// Proxy to [`ExecutorProxy::spawn_local`]
///
/// # Examples
///
/// ```
/// use glommio::{LocalExecutor, Task};
///
/// let local_ex = LocalExecutor::default();
///
/// local_ex.run(async {
///     let task = glommio::spawn_local(async { 1 + 2 });
///     assert_eq!(task.await, 3);
/// });
/// ```
pub fn spawn_local<T>(future: impl Future<Output = T> + 'static) -> Task<T>
where
    T: 'static,
{
    executor().spawn_local(future)
}

/// Allocates a buffer that is suitable for using to write to Direct Memory
/// Access File (DMA). Please note that this implementation uses embedded buddy
/// allocator to speed up allocation of the memory chunks, but the same
/// allocator is used to server memory needed to write/read data from `uring` so
/// probably that is not good idea to keep allocated memory for a long time.
/// If you want to keep allocated buffer for a long time please use
/// ['crate::allocate_dma_buffer_global'] instead.
/// Be careful when you use this buffer with DMA file, size and position of the
/// buffer should be properly aligned to the block size of the device where the
/// file is located
///
/// * `size` size of the requested buffer in bytes
///
/// [`DmaFile`]: crate::io::DmaFile
pub fn allocate_dma_buffer(size: usize) -> DmaBuffer {
    executor().reactor().alloc_dma_buffer(size)
}

/// Allocates a buffer that is suitable for using to write to Direct Memory
/// Access File (DMA). If you do not plan to keep allocated buffer for a long
/// time please use ['crate::allocate_dma_buffer'] instead.
/// Be careful when you use this buffer with DMA file, size and position of the
/// buffer should be properly aligned to the block size of the device where the
/// file is located
///
/// * `size` size of the requested buffer in bytes
///
/// [`DmaFile`]: crate::io::DmaFile
pub fn allocate_dma_buffer_global(size: usize) -> DmaBuffer {
    DmaBuffer::new(size).unwrap()
}

/// Spawns a task onto the current single-threaded executor, in a particular
/// task queue
///
/// If called from a [`LocalExecutor`], the task is spawned on it.
/// Otherwise, this method panics.
///
/// Note that there is no guarantee of when the spawned task is scheduled.
/// The current task can continue its execution or be preempted by the
/// newly spawned task immediately. See the documentation for the
/// top-level [`Task`] for examples.
///
/// Proxy to [`ExecutorProxy::spawn_local_into`]
///
/// # Examples
///
/// ```
/// # use glommio::{ LocalExecutor, Shares, Task};
///
/// # let local_ex = LocalExecutor::default();
/// # local_ex.run(async {
/// let handle = glommio::executor().create_task_queue(
///     Shares::default(),
///     glommio::Latency::NotImportant,
///     "test_queue",
/// );
/// let task = glommio::spawn_local_into(async { 1 + 2 }, handle).expect("failed to spawn task");
/// assert_eq!(task.await, 3);
/// # });
/// ```
pub fn spawn_local_into<T>(
    future: impl Future<Output = T> + 'static,
    handle: TaskQueueHandle,
) -> Result<Task<T>>
where
    T: 'static,
{
    executor().spawn_local_into(future, handle)
}

/// Spawns a task onto the current single-threaded executor.
///
/// If called from a [`LocalExecutor`], the task is spawned on it.
///
/// Otherwise, this method panics.
///
/// Proxy to [`ExecutorProxy::spawn_scoped_local`]
///
/// # Safety
///
/// `ScopedTask` depends on `drop` running or `.await` being called for
/// safety. See the struct [`ScopedTask`] for details.
///
/// # Examples
///
/// ```
/// use glommio::LocalExecutor;
///
/// let local_ex = LocalExecutor::default();
///
/// local_ex.run(async {
///     let non_static = 2;
///     let task = unsafe { glommio::spawn_scoped_local(async { 1 + non_static }) };
///     assert_eq!(task.await, 3);
/// });
/// ```
pub unsafe fn spawn_scoped_local<'a, T>(future: impl Future<Output = T> + 'a) -> ScopedTask<'a, T> {
    executor().spawn_scoped_local(future)
}

/// Spawns a task onto the current single-threaded executor, in a particular
/// task queue
///
/// If called from a [`LocalExecutor`], the task is spawned on it.
///
/// Otherwise, this method panics.
///
/// Proxy to [`ExecutorProxy::spawn_scoped_local_into`]
///
/// # Safety
///
/// `ScopedTask` depends on `drop` running or `.await` being called for
/// safety. See the struct [`ScopedTask`] for details.
///
/// # Examples
///
/// ```
/// use glommio::{LocalExecutor, Shares};
///
/// let local_ex = LocalExecutor::default();
/// local_ex.run(async {
///     let handle = glommio::executor().create_task_queue(
///         Shares::default(),
///         glommio::Latency::NotImportant,
///         "test_queue",
///     );
///     let non_static = 2;
///     let task = unsafe {
///         glommio::spawn_scoped_local_into(async { 1 + non_static }, handle)
///             .expect("failed to spawn task")
///     };
///     assert_eq!(task.await, 3);
/// })
/// ```
pub unsafe fn spawn_scoped_local_into<'a, T>(
    future: impl Future<Output = T> + 'a,
    handle: TaskQueueHandle,
) -> Result<ScopedTask<'a, T>> {
    executor().spawn_scoped_local_into(future, handle)
}

/// A proxy struct to the underlying [`LocalExecutor`]. It is accessible from
/// anywhere within a Glommio context using [`executor()`].
#[derive(Debug)]
pub struct ExecutorProxy {}

/// What to do with a blocking closure's panic that no task is waiting for.
///
/// A closure handed to [`ExecutorProxy::spawn_blocking`] runs to completion
/// even if the future awaiting it is dropped, which is what `select!` does
/// every time another branch wins. A panic then has nowhere to be resumed
/// into. The panic hook still reports it whichever of these is chosen; this
/// decides what happens afterwards.
#[derive(Clone, Default)]
#[non_exhaustive]
pub enum UnobservedPanic {
    /// Carry on. This is `tokio`'s default for a task whose handle is gone.
    #[default]
    Ignore,
    /// End the process, as `rayon` does when it has nowhere to propagate to.
    Abort,
    /// Hand the payload over, as `rayon`'s `panic_handler` does.
    ///
    /// Runs on whichever thread released the last reference: the pool worker
    /// if the future was dropped first, the executor if the closure finished
    /// first. A panic raised here aborts the process, as `rayon` does for its
    /// handler, since unwinding would otherwise end the pool worker or reach
    /// whichever task dropped the future. Wrap the body in
    /// [`std::panic::catch_unwind`] to avoid that.
    Handler(Arc<dyn Fn(Box<dyn std::any::Any + Send>) + Send + Sync>),
}

impl fmt::Debug for UnobservedPanic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ignore => f.write_str("Ignore"),
            Self::Abort => f.write_str("Abort"),
            Self::Handler(_) => f.write_str("Handler(..)"),
        }
    }
}

/// What a blocking job left behind.
///
/// One value rather than a result beside a panic flag, so "produced nothing"
/// and "produced a value" are different things the compiler can tell apart,
/// and "panicked *and* produced" cannot be written down at all. The previous
/// shape was a `MaybeUninit` that only the closure knew it had filled: a
/// closure that unwound left it untouched, and reading it then was undefined
/// rather than merely wrong.
struct BlockingSlot<R> {
    outcome: BlockingOutcome<R>,
    on_unobserved: UnobservedPanic,
}

impl<R> BlockingSlot<R> {
    /// Empties the slot, which is what marks the outcome as collected.
    ///
    /// The outcome is taken rather than moved out, since the slot implements
    /// `Drop` and a field cannot be moved out of such a type; `Pending` is left
    /// behind.
    fn take(&mut self) -> BlockingOutcome<R> {
        mem::replace(&mut self.outcome, BlockingOutcome::Pending)
    }
}

impl<R> Drop for BlockingSlot<R> {
    /// A slot still holding a panic is one nobody came back for.
    ///
    /// The awaiting task empties the slot when it collects, so reaching here
    /// with a payload means the future was dropped before the closure
    /// finished and there is nowhere left to resume into.
    fn drop(&mut self) {
        let BlockingOutcome::Panicked(payload) = self.take() else {
            return;
        };
        match &self.on_unobserved {
            UnobservedPanic::Ignore => {}
            UnobservedPanic::Abort => {
                eprintln!(
                    "glommio: a blocking closure panicked with no task awaiting it; aborting"
                );
                std::process::abort();
            }
            UnobservedPanic::Handler(handler) => {
                let _abort = scopeguard::guard_on_unwind((), |()| {
                    eprintln!("glommio: an unobserved panic handler panicked; aborting");
                    std::process::abort();
                });
                handler(payload);
            }
        }
    }
}

enum BlockingOutcome<R> {
    /// Handed to the pool, not yet run.
    Pending,
    /// Ran and returned.
    Produced(R),
    /// Ran and unwound. Carried so the caller sees its own panic rather than a
    /// substitute, as [`std::thread::JoinHandle::join`] does.
    Panicked(Box<dyn std::any::Any + Send>),
}

impl ExecutorProxy {
    /// Checks if this task has run for too long and need to be preempted. This
    /// is useful for situations where we can't call .await, for instance,
    /// if a [`RefMut`] is held. If this tests true, then the user is
    /// responsible for making any preparations necessary for calling .await
    /// and doing it themselves.
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::LocalExecutorBuilder;
    ///
    /// let ex = LocalExecutorBuilder::default()
    ///     .spawn(|| async {
    ///         loop {
    ///             if glommio::executor().need_preempt() {
    ///                 break;
    ///             }
    ///         }
    ///     })
    ///     .unwrap();
    ///
    /// ex.join().unwrap();
    /// ```
    ///
    /// Outside of a glommio context (no executor is installed on the thread)
    /// there is nothing to preempt, and this returns `false`.
    ///
    /// [`RefMut`]: https://doc.rust-lang.org/std/cell/struct.RefMut.html
    #[inline(always)]
    pub fn need_preempt(&self) -> bool {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX
            .try_with(|local_ex| local_ex.need_preempt())
            .unwrap_or(false);

        #[cfg(all(nightly, feature = "native-tls"))]
        return unsafe {
            LOCAL_EX
                .as_ref()
                .map(|ex| ex.need_preempt())
                .unwrap_or_default()
        };
    }

    /// Conditionally yields the current task queue. The scheduler may then
    /// process other task queues according to their latency requirements.
    /// If a call to this function results in the current queue to yield,
    /// then the calling task is moved to the back of the yielded task
    /// queue.
    ///
    /// Under which condition this function yield is an implementation detail
    /// subject to change, but it will always be somehow related to the latency
    /// guarantees that the task queues want to uphold in their
    /// `Latency::Matters` parameter (or `Latency::NotImportant`).
    ///
    /// This function is the central mechanism of task cooperation in Glommio
    /// and should be preferred over unconditional yielding methods like
    /// [`ExecutorProxy::yield_now`] and
    /// [`ExecutorProxy::yield_task_queue_now`].
    ///
    /// When this is called outside of a glommio context (no executor is
    /// installed on the thread), there is nothing to preempt and no yield
    /// happens.
    #[inline(always)]
    pub async fn yield_if_needed(&self) {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        {
            let need_yield = LOCAL_EX
                .try_with(|local_ex| {
                    if local_ex.need_preempt() {
                        local_ex.mark_me_for_yield();
                        true
                    } else {
                        false
                    }
                })
                .unwrap_or(false);

            if need_yield {
                futures_lite::future::yield_now().await;
            }
        }

        #[cfg(all(nightly, feature = "native-tls"))]
        unsafe {
            if self.need_preempt() {
                (*LOCAL_EX).mark_me_for_yield();
                futures_lite::future::yield_now().await;
            }
        }
    }

    /// Unconditionally yields the current task and forces the scheduler
    /// to poll another task within the current task queue.
    /// Calling this wakes the current task and returns [`Poll::Pending`] once.
    ///
    /// Unless you know you need to yield right now, using
    /// [`ExecutorProxy::yield_if_needed`] instead is the better choice.
    #[inline(always)]
    pub async fn yield_now(&self) {
        futures_lite::future::yield_now().await
    }

    /// Unconditionally yields the current task queue and forces the scheduler
    /// to poll another queue. Use [`ExecutorProxy::yield_now`] to yield within
    /// a queue.
    ///
    /// Unless you know you need to yield right now, using
    /// [`ExecutorProxy::yield_if_needed`] instead is the better choice.
    #[inline(always)]
    pub async fn yield_task_queue_now(&self) {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        {
            LOCAL_EX.try_with(|local_ex| {
                local_ex.mark_me_for_yield();
            });
            futures_lite::future::yield_now().await;
        }

        #[cfg(all(nightly, feature = "native-tls"))]
        {
            if let Some(local_ex) = unsafe { LOCAL_EX.as_ref() } {
                local_ex.mark_me_for_yield();
            }
            futures_lite::future::yield_now().await;
        }
    }

    #[inline(always)]
    pub(crate) fn reactor(&self) -> Rc<reactor::Reactor> {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX.with(|local_ex| local_ex.get_reactor());

        #[cfg(all(nightly, feature = "native-tls"))]
        return unsafe {
            LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .get_reactor()
        };
    }

    /// Returns the id of the current executor
    ///
    /// If called from a [`LocalExecutor`], returns the id of the executor.
    ///
    /// Otherwise, this method panics.
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::{LocalExecutor, Task};
    ///
    /// let local_ex = LocalExecutor::default();
    ///
    /// local_ex.run(async {
    ///     println!("my ID: {}", glommio::executor().id());
    /// });
    /// ```
    pub fn id(&self) -> usize {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX.with(|local_ex| local_ex.id());

        #[cfg(all(nightly, feature = "native-tls"))]
        return unsafe {
            LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .id()
        };
    }

    /// Creates a new task queue, with a given latency hint and the provided
    /// name
    ///
    /// Each task queue is scheduled based on the [`Shares`] and [`Latency`]
    /// system, and tasks within a queue will be scheduled in serial.
    ///
    /// Returns an opaque handle that can later be used to launch tasks into
    /// that queue with [`local_into`].
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::{Latency, LocalExecutor, Shares};
    /// use std::time::Duration;
    ///
    /// let local_ex = LocalExecutor::default();
    /// local_ex.run(async move {
    ///     let task_queue = glommio::executor().create_task_queue(
    ///         Shares::default(),
    ///         Latency::Matters(Duration::from_secs(1)),
    ///         "my_tq",
    ///     );
    ///     let task = glommio::spawn_local_into(
    ///         async {
    ///             println!("Hello world");
    ///         },
    ///         task_queue,
    ///     )
    ///     .expect("failed to spawn task");
    /// });
    /// ```
    ///
    /// [`local_into`]: crate::spawn_local_into
    /// [`Shares`]: enum.Shares.html
    /// [`Latency`]: enum.Latency.html
    pub fn create_task_queue(
        &self,
        shares: Shares,
        latency: Latency,
        name: &str,
    ) -> TaskQueueHandle {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX.with(|local_ex| local_ex.create_task_queue(shares, latency, name));

        #[cfg(all(nightly, feature = "native-tls"))]
        return unsafe {
            LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .create_task_queue(shares, latency, name)
        };
    }

    /// Returns the [`TaskQueueHandle`] that represents the TaskQueue currently
    /// running. This can be passed directly into [`crate::spawn_local_into`].
    /// This must be run from a task that was generated through
    /// [`crate::spawn_local`] or [`crate::spawn_local_into`]
    ///
    /// # Examples
    /// ```
    /// use glommio::{Latency, LocalExecutor, LocalExecutorBuilder, Shares};
    ///
    /// let ex = LocalExecutorBuilder::default()
    ///     .spawn(|| async move {
    ///         let original_tq = glommio::executor().current_task_queue();
    ///         let new_tq = glommio::executor().create_task_queue(
    ///             Shares::default(),
    ///             Latency::NotImportant,
    ///             "test",
    ///         );
    ///
    ///         let task = glommio::spawn_local_into(
    ///             async move {
    ///                 glommio::spawn_local_into(
    ///                     async move {
    ///                         assert_eq!(glommio::executor().current_task_queue(), original_tq);
    ///                     },
    ///                     original_tq,
    ///                 )
    ///                 .unwrap();
    ///             },
    ///             new_tq,
    ///         )
    ///         .unwrap();
    ///         task.await;
    ///     })
    ///     .unwrap();
    ///
    /// ex.join().unwrap();
    /// ```
    pub fn current_task_queue(&self) -> TaskQueueHandle {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX.with(|local_ex| local_ex.current_task_queue());

        #[cfg(all(nightly, feature = "native-tls"))]
        return unsafe {
            LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .current_task_queue()
        };
    }

    /// Removes a task queue, as [`LocalExecutor::remove_task_queue`] does.
    pub(crate) fn remove_task_queue(&self, handle: TaskQueueHandle) -> Result<()> {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX.with(|local_ex| local_ex.remove_task_queue(handle));

        #[cfg(all(nightly, feature = "native-tls"))]
        return unsafe {
            LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .remove_task_queue(handle)
        };
    }

    /// Returns a [`Result`] with its `Ok` value wrapping a [`TaskQueueStats`]
    /// or a [`GlommioError`] of type `[QueueErrorKind`] if there is no task
    /// queue with this handle
    ///
    /// Reading resets the queue's counters: each call returns what accumulated
    /// since the previous call to this or to [`all_task_queue_stats`].
    ///
    /// [`all_task_queue_stats`]: ExecutorProxy::all_task_queue_stats
    ///
    /// # Examples
    /// ```
    /// use glommio::{Latency, LocalExecutorBuilder, Shares};
    ///
    /// let ex = LocalExecutorBuilder::default()
    ///     .spawn(|| async move {
    ///         let new_tq = glommio::executor().create_task_queue(
    ///             Shares::default(),
    ///             Latency::NotImportant,
    ///             "test",
    ///         );
    ///         println!(
    ///             "Stats for test: {:?}",
    ///             glommio::executor().task_queue_stats(new_tq).unwrap()
    ///         );
    ///     })
    ///     .unwrap();
    ///
    /// ex.join().unwrap();
    /// ```
    ///
    /// [`ExecutorStats`]: struct.ExecutorStats.html
    /// [`GlommioError`]: crate::error::GlommioError
    /// [`QueueErrorKind`]: crate::error::QueueErrorKind
    /// [`Result`]: https://doc.rust-lang.org/std/result/enum.Result.html
    pub fn task_queue_stats(&self, handle: TaskQueueHandle) -> Result<TaskQueueStats> {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX.with(|local_ex| match local_ex.get_queue(&handle) {
            Some(x) => Ok(x.borrow_mut().stats.take()),
            None => Err(GlommioError::queue_not_found(handle.index)),
        });

        #[cfg(all(nightly, feature = "native-tls"))]
        return match unsafe {
            LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .get_queue(&handle)
        } {
            Some(x) => Ok(x.borrow_mut().stats.take()),
            None => Err(GlommioError::queue_not_found(handle.index)),
        };
    }

    /// Returns a collection of [`TaskQueueStats`] with information about all
    /// task queues in the system
    ///
    /// The collection can be anything that implements [`Extend`] and it is
    /// initially passed by the user, so they can control how allocations are
    /// done.
    ///
    /// Reading resets every queue's counters, as [`task_queue_stats`] does for
    /// one.
    ///
    /// [`task_queue_stats`]: ExecutorProxy::task_queue_stats
    ///
    /// # Examples
    /// ```
    /// use glommio::{executor, Latency, LocalExecutorBuilder, Shares};
    ///
    /// let ex = LocalExecutorBuilder::default()
    ///     .spawn(|| async move {
    ///         let new_tq = glommio::executor().create_task_queue(
    ///             Shares::default(),
    ///             Latency::NotImportant,
    ///             "test",
    ///         );
    ///         let v = Vec::new();
    ///         println!(
    ///             "Stats for all queues: {:?}",
    ///             glommio::executor().all_task_queue_stats(v)
    ///         );
    ///     })
    ///     .unwrap();
    ///
    /// ex.join().unwrap();
    /// ```
    ///
    /// [`ExecutorStats`]: struct.ExecutorStats.html
    /// [`Result`]: https://doc.rust-lang.org/std/result/enum.Result.html
    /// [`Extend`]: https://doc.rust-lang.org/std/iter/trait.Extend.html
    pub fn all_task_queue_stats<V>(&self, mut output: V) -> V
    where
        V: Extend<TaskQueueStats>,
    {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        LOCAL_EX.with(|local_ex| {
            output.extend(
                local_ex
                    .queues
                    .borrow()
                    .available_executors
                    .values()
                    .map(|x| x.borrow_mut().stats.take()),
            );
        });

        #[cfg(all(nightly, feature = "native-tls"))]
        output.extend(unsafe {
            LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .queues
                .borrow()
                .available_executors
                .values()
                .map(|x| x.borrow_mut().stats.take())
        });

        output
    }

    /// Returns a [`ExecutorStats`] struct with information about this Executor
    ///
    /// Reading resets the counters: each call returns what accumulated since
    /// the previous call. The task queue counters are separate and are not
    /// reset by this.
    ///
    /// # Examples:
    ///
    /// ```
    /// use glommio::{executor, LocalExecutorBuilder};
    ///
    /// let ex = LocalExecutorBuilder::default()
    ///     .spawn(|| async move {
    ///         println!(
    ///             "Stats for executor: {:?}",
    ///             glommio::executor().executor_stats()
    ///         );
    ///     })
    ///     .unwrap();
    ///
    /// ex.join().unwrap();
    /// ```
    ///
    /// [`ExecutorStats`]: struct.ExecutorStats.html
    pub fn executor_stats(&self) -> ExecutorStats {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX.with(|local_ex| std::mem::take(&mut local_ex.queues.borrow_mut().stats));

        #[cfg(all(nightly, feature = "native-tls"))]
        return std::mem::take(unsafe {
            &mut LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .queues
                .borrow_mut()
                .stats
        });
    }

    /// Returns an [`IoStats`] struct with information about IO performed by
    /// this executor's reactor
    ///
    /// Reading resets the counters and latency distributions of every ring:
    /// each call returns what accumulated since the previous call. The per
    /// task queue IO stats are separate and are not reset by this.
    ///
    /// # Examples:
    ///
    /// ```
    /// use glommio::LocalExecutorBuilder;
    ///
    /// let ex = LocalExecutorBuilder::default()
    ///     .spawn(|| async move {
    ///         println!("Stats for executor: {:?}", glommio::executor().io_stats());
    ///     })
    ///     .unwrap();
    ///
    /// ex.join().unwrap();
    /// ```
    ///
    /// [`IoStats`]: crate::IoStats
    pub fn io_stats(&self) -> IoStats {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX.with(|local_ex| local_ex.get_reactor().io_stats());

        #[cfg(all(nightly, feature = "native-tls"))]
        return unsafe {
            LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .get_reactor()
                .io_stats()
        };
    }

    /// Returns an [`IoStats`] struct with information about IO performed from
    /// the provided TaskQueue by this executor's reactor
    ///
    /// Reading resets this queue's IO counters and latency distributions: each
    /// call returns what accumulated since the previous call for this queue.
    /// The executor-wide [`io_stats`] are separate and are not reset by this.
    ///
    /// [`io_stats`]: ExecutorProxy::io_stats
    ///
    /// # Examples:
    ///
    /// ```
    /// use glommio::{Latency, LocalExecutorBuilder, Shares};
    ///
    /// let ex = LocalExecutorBuilder::default()
    ///     .spawn(|| async move {
    ///         let new_tq = glommio::executor().create_task_queue(
    ///             Shares::default(),
    ///             Latency::NotImportant,
    ///             "test",
    ///         );
    ///         println!(
    ///             "Stats for executor: {:?}",
    ///             glommio::executor().task_queue_io_stats(new_tq)
    ///         );
    ///     })
    ///     .unwrap();
    ///
    /// ex.join().unwrap();
    /// ```
    ///
    /// [`IoStats`]: crate::IoStats
    pub fn task_queue_io_stats(&self, handle: TaskQueueHandle) -> Result<IoStats> {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX.with(|local_ex| {
            match local_ex.get_reactor().task_queue_io_stats(&handle) {
                Some(x) => Ok(x),
                None => Err(GlommioError::queue_not_found(handle.index)),
            }
        });

        #[cfg(all(nightly, feature = "native-tls"))]
        return match unsafe {
            LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .get_reactor()
                .task_queue_io_stats(&handle)
        } {
            Some(x) => Ok(x),
            None => Err(GlommioError::queue_not_found(handle.index)),
        };
    }

    /// Spawns a task onto the current single-threaded executor.
    ///
    /// If called from a [`LocalExecutor`], the task is spawned on it.
    /// Otherwise, this method panics.
    ///
    /// Note that there is no guarantee of when the spawned task is scheduled.
    /// The current task can continue its execution or be preempted by the
    /// newly spawned task immediately. See the documentation for the
    /// top-level [`Task`] for examples.
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::{LocalExecutor, Task};
    ///
    /// let local_ex = LocalExecutor::default();
    ///
    /// local_ex.run(async {
    ///     let task = glommio::executor().spawn_local(async { 1 + 2 });
    ///     assert_eq!(task.await, 3);
    /// });
    /// ```
    pub fn spawn_local<T>(&self, future: impl Future<Output = T> + 'static) -> Task<T>
    where
        T: 'static,
    {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX.with(|local_ex| Task::<T>(local_ex.spawn_internal(future)));

        #[cfg(all(nightly, feature = "native-tls"))]
        return Task::<T>(unsafe {
            LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .spawn_internal(future)
        });
    }

    /// Spawns a task onto the current single-threaded executor, in a particular
    /// task queue
    ///
    /// If called from a [`LocalExecutor`], the task is spawned on it.
    /// Otherwise, this method panics.
    ///
    /// Note that there is no guarantee of when the spawned task is scheduled.
    /// The current task can continue its execution or be preempted by the
    /// newly spawned task immediately. See the documentation for the
    /// top-level [`Task`] for examples.
    ///
    /// # Examples
    ///
    /// ```
    /// # use glommio::{LocalExecutor, Shares, Task};
    ///
    /// # let local_ex = LocalExecutor::default();
    /// # local_ex.run(async {
    /// let handle = glommio::executor().create_task_queue(
    ///     Shares::default(),
    ///     glommio::Latency::NotImportant,
    ///     "test_queue",
    /// );
    /// let task = glommio::executor()
    ///     .spawn_local_into(async { 1 + 2 }, handle)
    ///     .expect("failed to spawn task");
    /// assert_eq!(task.await, 3);
    /// # });
    /// ```
    pub fn spawn_local_into<T>(
        &self,
        future: impl Future<Output = T> + 'static,
        handle: TaskQueueHandle,
    ) -> Result<Task<T>>
    where
        T: 'static,
    {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX.with(|local_ex| local_ex.spawn_into(future, handle).map(Task::<T>));

        #[cfg(all(nightly, feature = "native-tls"))]
        return unsafe {
            LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .spawn_into(future, handle)
        }
        .map(Task::<T>);
    }

    /// Spawns a task onto the current single-threaded executor.
    ///
    /// If called from a [`LocalExecutor`], the task is spawned on it.
    ///
    /// Otherwise, this method panics.
    ///
    /// # Safety
    ///
    /// `ScopedTask` depends on `drop` running or `.await` being called for
    /// safety. See the struct [`ScopedTask`] for details.
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::LocalExecutor;
    ///
    /// let local_ex = LocalExecutor::default();
    ///
    /// local_ex.run(async {
    ///     let non_static = 2;
    ///     let task = unsafe { glommio::executor().spawn_scoped_local(async { 1 + non_static }) };
    ///     assert_eq!(task.await, 3);
    /// });
    /// ```
    pub unsafe fn spawn_scoped_local<'a, T>(
        &self,
        future: impl Future<Output = T> + 'a,
    ) -> ScopedTask<'a, T> {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX
            .with(|local_ex| ScopedTask::<'a, T>(local_ex.spawn_internal(future), PhantomData));

        #[cfg(all(nightly, feature = "native-tls"))]
        return ScopedTask::<'a, T>(
            LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .spawn_internal(future),
            PhantomData,
        );
    }

    /// Spawns a task onto the current single-threaded executor, in a particular
    /// task queue
    ///
    /// If called from a [`LocalExecutor`], the task is spawned on it.
    ///
    /// Otherwise, this method panics.
    ///
    /// # Safety
    ///
    /// `ScopedTask` depends on `drop` running or `.await` being called for
    /// safety. See the struct [`ScopedTask`] for details.
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::{LocalExecutor, Shares};
    ///
    /// let local_ex = LocalExecutor::default();
    /// local_ex.run(async {
    ///     let handle = glommio::executor().create_task_queue(
    ///         Shares::default(),
    ///         glommio::Latency::NotImportant,
    ///         "test_queue",
    ///     );
    ///     let non_static = 2;
    ///     let task = unsafe {
    ///         glommio::executor()
    ///             .spawn_scoped_local_into(async { 1 + non_static }, handle)
    ///             .expect("failed to spawn task")
    ///     };
    ///     assert_eq!(task.await, 3);
    /// })
    /// ```
    pub unsafe fn spawn_scoped_local_into<'a, T>(
        &self,
        future: impl Future<Output = T> + 'a,
        handle: TaskQueueHandle,
    ) -> Result<ScopedTask<'a, T>> {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX.with(|local_ex| {
            local_ex
                .spawn_into(future, handle)
                .map(|x| ScopedTask::<'a, T>(x, PhantomData))
        });

        #[cfg(all(nightly, feature = "native-tls"))]
        return LOCAL_EX
            .as_ref()
            .expect("this thread doesn't have a LocalExecutor running")
            .spawn_into(future, handle)
            .map(|x| ScopedTask::<'a, T>(x, PhantomData));
    }

    /// The policy the running executor was built with.
    fn current_unobserved_panic() -> UnobservedPanic {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX.with(|local_ex| local_ex.on_unobserved_panic.clone());

        #[cfg(all(nightly, feature = "native-tls"))]
        unsafe {
            LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .on_unobserved_panic
                .clone()
        }
    }

    /// Spawns a blocking task into a background thread where blocking is
    /// acceptable.
    ///
    /// Glommio depends on cooperation from tasks in order to drive IO and meet
    /// latency requirements. Unyielding tasks are detrimental to the
    /// performance of the overall system, not just to the performance of
    /// the one stalling task.
    ///
    /// `spawn_blocking` is there as a last resort when a blocking task needs to
    /// be executed and cannot be made cooperative. Examples are:
    /// * Expensive syscalls that cannot use `io_uring`, such as `mmap`
    ///   (especially with `MAP_POPULATE`)
    /// * Calls to synchronous third-party code (compression, encoding, etc.)
    ///
    /// # Note
    ///
    /// *This method is not meant to be a way to achieve compute parallelism.*
    /// Distributing work across executors is the better way to achieve that.
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::{LocalExecutor, Task};
    /// use std::time::Duration;
    ///
    /// let local_ex = LocalExecutor::default();
    ///
    /// local_ex.run(async {
    ///     let task = glommio::executor()
    ///         .spawn_blocking(|| {
    ///             std::thread::sleep(Duration::from_millis(100));
    ///         })
    ///         .await;
    /// });
    /// ```
    ///
    /// # Panics
    ///
    /// If `func` panics, the panic is resumed on the awaiting task, as
    /// [`std::thread::JoinHandle::join`] does. The pool thread survives and
    /// keeps serving later work.
    ///
    /// `func` is not required to be [`std::panic::UnwindSafe`].
    pub fn spawn_blocking<F, R>(&self, func: F) -> impl Future<Output = R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let on_unobserved = Self::current_unobserved_panic();
        let outcome = Arc::new(Mutex::new(BlockingSlot {
            outcome: BlockingOutcome::Pending,
            on_unobserved,
        }));
        let f_inner = enclose::enclose!((outcome) move || {
            let produced = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(func)) {
                Ok(value) => BlockingOutcome::Produced(value),
                Err(payload) => BlockingOutcome::Panicked(payload),
            };
            outcome.lock().unwrap().outcome = produced;
        });

        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        let waiter =
            LOCAL_EX.with(move |local_ex| local_ex.reactor.run_blocking(Box::new(f_inner)));

        #[cfg(all(nightly, feature = "native-tls"))]
        let waiter = unsafe {
            LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .reactor
                .run_blocking(Box::new(f_inner))
        };

        async move {
            let source = waiter.await;
            assert!(source.collect_rw().await.is_ok());
            // The pool dropped its handle before answering `collect_rw`, so this
            // is the last one. Not an `unwrap`: the error arm hands back the
            // `Arc`, which is only `Debug` if `R` is.
            let Ok(cell) = Arc::try_unwrap(outcome) else {
                unreachable!("the blocking pool still holds the outcome")
            };
            // The lock is only ever held to store the outcome, and storing it
            // replaces a `Pending` that has nothing to drop, so nothing can
            // unwind while holding it and the mutex cannot be poisoned.
            match cell.into_inner().unwrap().take() {
                BlockingOutcome::Produced(value) => value,
                BlockingOutcome::Panicked(payload) => std::panic::resume_unwind(payload),
                BlockingOutcome::Pending => {
                    unreachable!("the blocking pool reported success without an outcome")
                }
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Run once, just before the executor parks, by a test that needs to act
    /// at that moment.
    static BEFORE_PARK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}

#[cfg(test)]
mod test {
    use rusty_fork::rusty_fork_test;
    use std::{
        cell::Cell,
        collections::HashMap,
        fs,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
        task::Waker,
    };

    use futures::{
        future::{join, join_all, poll_fn},
        join,
    };

    use crate::{
        enclose,
        error::{ExecutorErrorKind, QueueErrorKind},
        io::{DmaFile, OpenOptions, OwnedDmaFile},
        timer::{self, sleep, Timer},
        GlommioError, SharesManager,
    };

    use super::*;

    fn eventfd_count() -> usize {
        fs::read_dir("/proc/self/fd")
            .expect("failed to enumerate this process's file descriptors")
            .filter_map(|entry| {
                let entry = entry.ok()?;
                fs::read_link(entry.path()).ok()
            })
            .filter(|target| target == std::path::Path::new("anon_inode:[eventfd]"))
            .count()
    }

    /// Waits for the eventfd count to come back to `expected`, up to a limit.
    ///
    /// A descriptor is closed when the last owner drops it, and the last owner
    /// is not always the thread `join` returned from: a peer that has not yet
    /// noticed a disconnect still holds a notifier for a moment. Asserting the
    /// instant a round ends reads that moment as a leak. A leak does not
    /// converge, so waiting costs a real one nothing and a transient one a few
    /// milliseconds.
    fn await_eventfd_count(expected: usize, what: &str) {
        const LIMIT: Duration = Duration::from_secs(5);

        let deadline = Instant::now() + LIMIT;
        loop {
            let count = eventfd_count();
            if count == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{what}: {count} eventfds against {expected} expected, still there after {LIMIT:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn run_shared_channel_round() {
        let (sender, receiver) = crate::channels::shared_channel::new_bounded(1);

        let sender = LocalExecutorBuilder::default()
            .io_memory(0)
            .spawn(move || async move {
                let sender = sender.connect().await;
                let file = OpenOptions::new()
                    .create_new(true)
                    .read(true)
                    .write(true)
                    .tmpfile(true)
                    .dma_open(std::env::temp_dir())
                    .await
                    .unwrap();
                let file: OwnedDmaFile = file.into();
                sender.send(file).await.unwrap();
            })
            .unwrap();

        let receiver = LocalExecutorBuilder::default()
            .io_memory(0)
            .spawn(move || async move {
                let receiver = receiver.connect().await;
                let file: DmaFile = receiver.recv().await.unwrap().into();
                assert!(file.read_at(0, 1).await.unwrap().is_empty());
                file.close().await.unwrap();
            })
            .unwrap();

        sender.join().unwrap();
        receiver.join().unwrap();
    }

    rusty_fork_test! {
        /// The `eventfd_count` check must work regardless of other tests
        /// running in the same process (which is what happens when running
        /// with cargo test instead of cargo nextest), so this test forks.
        ///
        /// The disconnected notifier is a process-wide singleton: it is
        /// initialized before measuring so the baseline contains every
        /// eventfd that is expected to persist. Enough rounds run to make
        /// the leak from #448 unambiguous, then additional executor
        /// shutdowns must not accumulate descriptors either.
        #[test]
        fn executor_shutdown_does_not_leak_eventfds() {
            let _ = crate::sys::get_sleep_notifier_for(usize::MAX);
            let initial_eventfds = eventfd_count();

            for _ in 0..10 {
                run_shared_channel_round();
            }
            await_eventfd_count(initial_eventfds, "after 10 rounds");

            for _ in 0..90 {
                run_shared_channel_round();
            }
            await_eventfd_count(initial_eventfds, "after 100 rounds");
        }
    }

    /// A queue that runs for hours at one share scales its runtime past
    /// `u64`'s range before the shift brings it back, and a run longer than
    /// the vruntime can hold at all pins it at the top.
    #[test]
    fn a_long_run_at_one_share_accounts_its_vruntime_without_overflow() {
        let ex = LocalExecutor::default();
        let queue = TaskQueue::new(
            Default::default(),
            "long",
            Shares::Static(1),
            IoRequirements::default(),
            0,
            ex.task_registry(),
        );
        let two_hours = Duration::from_secs(2 * 3600);
        assert_eq!(
            queue.borrow_mut().account_vruntime(two_hours),
            Some(1024 * two_hours.as_nanos() as u64)
        );

        let queue = TaskQueue::new(
            Default::default(),
            "longer",
            Shares::Static(1),
            IoRequirements::default(),
            0,
            ex.task_registry(),
        );
        assert_eq!(
            queue.borrow_mut().account_vruntime(Duration::MAX),
            Some(u64::MAX)
        );
    }

    /// A queue activated with the least vruntime at the top of its range is
    /// placed at the top, in the window after a preempted slice and outside
    /// it, rather than wrapping to zero.
    #[test]
    fn activation_at_the_top_of_the_vruntime_range_saturates() {
        let ex = LocalExecutor::default();
        let queue = TaskQueue::new(
            Default::default(),
            "top",
            Shares::Static(1),
            IoRequirements::default(),
            0,
            ex.task_registry(),
        );
        let mut queues = ExecutorQueues::new(Duration::from_millis(20), None);
        queues.default_vruntime = u64::MAX;
        assert_eq!(queues.activation_vruntime(&queue.borrow()), u64::MAX);
        queues.preempted = Some(Preempted::new(u64::MAX, usize::MAX));
        assert_eq!(queues.activation_vruntime(&queue.borrow()), u64::MAX);
        queues.preempted = Some(Preempted::new(5, usize::MAX));
        assert_eq!(queues.activation_vruntime(&queue.borrow()), 6);
    }

    /// The least vruntime is lowered as a window ends, so a queue activated
    /// after it is placed behind the least runnable queue, not behind the
    /// cached value.
    ///
    /// The window placed `woken` by its charge, below the cached least
    /// vruntime that `busy` left as its slice ended. Activated once the window
    /// has ended, `late` goes ahead of `busy`, as it would have had it been
    /// activated just before the window ended.
    #[test]
    fn a_queue_activated_after_a_window_is_placed_by_the_least_runnable_queue() {
        let ex = LocalExecutor::default();
        let queue = |name: &str| {
            let handle =
                ex.context
                    .create_task_queue(Shares::default(), Latency::NotImportant, name);
            ex.context.get_queue(&handle).unwrap()
        };
        let busy = queue("busy");
        let woken = queue("woken");
        let late = queue("late");
        {
            let mut queues = ex.context.queues.borrow_mut();
            queues.default_vruntime = 200;
            busy.borrow_mut().vruntime = 200;
            busy.borrow_mut().active = true;
            queues.push_runnable(busy.clone());
            queues.preempted = Some(Preempted::new(100, usize::MAX));
            queues.maybe_activate(woken.clone());
        }
        assert_eq!(woken.borrow().vruntime, 101);
        ex.context.end_preempted_window();
        ex.context.queues.borrow_mut().maybe_activate(late.clone());
        assert!(
            late.borrow().vruntime < busy.borrow().vruntime,
            "the late queue was placed at {} behind the busy one at {}",
            late.borrow().vruntime,
            busy.borrow().vruntime
        );
    }

    /// The queue whose slice a window opened for is placed behind a queue the
    /// window woke when the window reactivates it.
    ///
    /// From a model trace with three queues of equal shares: `preempted`'s
    /// slice ended with its task awaiting I/O, so the queue went idle at the
    /// 3 it was charged, while `least` holds the least vruntime, 0. The window
    /// wakes `woken`, idle at 4, and places it at 1. Then it holds the
    /// preempted task, whose wake reactivates `preempted`: placed by the
    /// window like any woken queue it would land at 1 as well, tie with
    /// `woken` and could run first.
    #[test]
    fn a_preempted_queue_reactivated_in_its_window_goes_behind_the_queues_it_woke() {
        let ex = LocalExecutor::default();
        let queue = |name: &str| {
            let handle =
                ex.context
                    .create_task_queue(Shares::default(), Latency::NotImportant, name);
            ex.context.get_queue(&handle).unwrap()
        };
        let least = queue("least");
        let woken = queue("woken");
        let preempted = queue("preempted");
        let (runnable, address, _) = unscheduled_in(&ex, preempted.clone());
        {
            let mut queues = ex.context.queues.borrow_mut();
            queues.default_vruntime = 0;
            least.borrow_mut().vruntime = 0;
            least.borrow_mut().active = true;
            queues.push_runnable(least.clone());
            woken.borrow_mut().vruntime = 4;
            preempted.borrow_mut().vruntime = 3;
            let mut window = Preempted::new(0, preempted.borrow().index());
            window.last = Some(Held {
                task: address,
                runnable: None,
            });
            queues.preempted = Some(window);
            queues.maybe_activate(woken.clone());
            assert!(matches!(queues.hold(&preempted, runnable), Ok(None)));
        }
        assert_eq!(woken.borrow().vruntime, 1);
        assert!(
            woken.borrow().vruntime < preempted.borrow().vruntime,
            "the woken queue at {} does not go ahead of the preempted one at {}",
            woken.borrow().vruntime,
            preempted.borrow().vruntime
        );
        let order: Vec<String> =
            std::iter::from_fn(|| ex.context.queues.borrow_mut().active_executors.pop())
                .map(|queue| queue.borrow().name.clone())
                .collect();
        assert_eq!(order, ["least", "woken", "preempted"]);
        ex.context.end_preempted_window();
    }

    /// A queue whose slice a window opened for, reactivated in that window, is
    /// placed no further back than just behind the usual place, however much
    /// its slice was charged.
    ///
    /// A queue with one share is charged a thousand times as much for a slice
    /// as one with a thousand. Placed by that charge after sleeping through
    /// the window, it would wait for every other queue to catch up, which an
    /// idle queue is never made to do.
    #[test]
    fn a_preempted_queue_charged_far_ahead_is_placed_just_behind_the_usual_place() {
        let ex = LocalExecutor::default();
        let handle =
            ex.context
                .create_task_queue(Shares::Static(1), Latency::NotImportant, "preempted");
        let preempted = ex.context.get_queue(&handle).unwrap();
        {
            let mut queues = ex.context.queues.borrow_mut();
            queues.default_vruntime = 100;
            preempted.borrow_mut().vruntime = 1_000_000;
            queues.preempted = Some(Preempted::new(50, preempted.borrow().index()));
            queues.maybe_activate(preempted.clone());
        }
        assert_eq!(preempted.borrow().vruntime, 102);
        ex.context.end_preempted_window();
    }

    /// A window opened by a task that cleanup polls inline during a slice
    /// belongs to the queue whose slice it is, and holds the task in the queue
    /// cleanup ran it in.
    ///
    /// A task of `other`, another executor, owns through `slot` the handle of
    /// a task parked in `cleanup`, a queue of this one. Dropping `other` during
    /// a slice of the default queue cancels the parked task from `other`'s
    /// context, so this executor drops it in cleanup, in `cleanup`, and its
    /// destructor spawns a task that spins until the preempt timer fires and
    /// wakes itself. A try in which the timer had fired before that task began
    /// proves nothing and is made again.
    #[test]
    fn a_window_opened_by_cleanup_during_a_slice_belongs_to_the_slice() {
        struct SpawnOnDrop(Rc<Cell<bool>>);

        impl Drop for SpawnOnDrop {
            fn drop(&mut self) {
                let crossed = self.0.clone();
                crate::spawn_local(async move {
                    let before = crate::executor().need_preempt();
                    while !crate::executor().need_preempt() {}
                    crossed.set(!before);
                    crate::executor().yield_now().await;
                })
                .detach();
            }
        }

        for _ in 0..10 {
            let slot: Rc<RefCell<Option<crate::Task<()>>>> = Rc::new(RefCell::new(None));
            let other = LocalExecutor::default();
            other.run(enclose! { (slot) async move {
                crate::spawn_local(async move {
                    let _slot = slot;
                    Timer::new(Duration::from_secs(3600)).await;
                })
                .detach();
            }});
            let ex = LocalExecutorBuilder::default()
                .preempt_timer(Duration::from_millis(5))
                .make()
                .unwrap();
            let crossed = Rc::new(Cell::new(false));
            let (slice, cleanup, window, restored) = ex.run(enclose! { (crossed) async move {
                let cleanup = crate::executor().create_task_queue(
                    Shares::default(),
                    Latency::NotImportant,
                    "cleanup",
                );
                let started = Rc::new(Cell::new(false));
                let parked = crate::spawn_local_into(
                    enclose! { (started) async move {
                        let _spawns = SpawnOnDrop(crossed);
                        started.set(true);
                        std::future::pending::<()>().await;
                    }},
                    cleanup,
                )
                .unwrap();
                while !started.get() {
                    crate::executor().yield_now().await;
                }
                *slot.borrow_mut() = Some(parked);
                drop(slot);
                let slice = with_running_queues(|queues| {
                    queues
                        .active_executing
                        .as_ref()
                        .map(|queue| queue.borrow().index())
                });
                drop(other);
                let window = with_running_queues(|queues| {
                    queues.preempted.as_ref().map(|preempted| {
                        let held: Vec<usize> = preempted
                            .inline
                            .iter()
                            .filter_map(|held| held.runnable.as_ref())
                            .map(|runnable| runnable.task_queue_index())
                            .collect();
                        (preempted.queue, held)
                    })
                });
                let restored = with_running_queues(|queues| queues.slice_owner.is_none());
                (slice, cleanup.index, window, restored)
            }});
            if !crossed.get() {
                continue;
            }
            assert_eq!(window, Some((slice.unwrap(), vec![cleanup])));
            assert!(
                restored,
                "the slice's queue was left recorded after cleanup"
            );
            return;
        }
        panic!("the preempt timer had always fired before the task cleanup spawned began");
    }

    /// A slice that `need_preempt` ended before it polled any task opens no
    /// window of its own and charges its queue nothing past its runtime.
    #[test]
    fn a_slice_preempted_before_it_polled_a_task_opens_no_window() {
        let ex = LocalExecutor::default();
        let queue = ex.context.get_queue(&TaskQueueHandle::default()).unwrap();
        {
            let mut queues = ex.context.queues.borrow_mut();
            queue.borrow_mut().vruntime = 7;
            queue.borrow_mut().active = true;
            queues.active_executing = Some(queue.clone());
            queues.in_slice = true;
        }
        ex.finish_task_queue_run(queue.clone(), Duration::ZERO, 0, Some((None, 5)));
        assert!(
            ex.context.queues.borrow().preempted.is_none(),
            "a window opened"
        );
        assert_eq!(queue.borrow().vruntime, 7);
    }

    /// A slice that `need_preempt` ended before it polled any task keeps the
    /// window cleanup opened as it was set up, names its queue the window's,
    /// and charges it the step, as a slice that polled a task does.
    #[test]
    fn a_slice_preempted_before_it_polled_a_task_keeps_the_window_set_up_opened() {
        let ex = LocalExecutor::default();
        let queue = ex.context.get_queue(&TaskQueueHandle::default()).unwrap();
        {
            let mut queues = ex.context.queues.borrow_mut();
            queue.borrow_mut().vruntime = 7;
            queue.borrow_mut().active = true;
            queues.preempted = Some(window(1));
            queues.active_executing = Some(queue.clone());
            queues.in_slice = true;
        }
        ex.finish_task_queue_run(queue.clone(), Duration::ZERO, 0, Some((None, 5)));
        {
            let queues = ex.context.queues.borrow();
            let preempted = queues.preempted.as_ref().expect("the window was dropped");
            assert_eq!(preempted.queue, queue.borrow().index());
            assert!(preempted.last.is_none(), "a last task was remembered");
            assert_eq!(preempted.inline.len(), 1);
        }
        assert_eq!(queue.borrow().vruntime, 8);
        ex.context.end_preempted_window();
    }

    /// A queue the window woke goes ahead of the queue whose slice opened the
    /// window when the slice charged that queue nothing.
    ///
    /// The slice began with the least vruntime at 100 and left the preempted
    /// queue idle at 100. `woken`, charged 90, is placed no earlier than it
    /// would have been woken during the slice, just behind 100, which is where
    /// the preempted queue's charge for its slice puts it too.
    #[test]
    fn a_woken_queue_goes_ahead_of_a_preempted_queue_its_slice_charged_nothing() {
        let ex = LocalExecutor::default();
        let queue = |name: &str| {
            let handle =
                ex.context
                    .create_task_queue(Shares::default(), Latency::NotImportant, name);
            ex.context.get_queue(&handle).unwrap()
        };
        let preempted = queue("preempted");
        let woken = queue("woken");
        {
            let mut queues = ex.context.queues.borrow_mut();
            queues.default_vruntime = 100;
            preempted.borrow_mut().vruntime = 100;
            preempted.borrow_mut().active = true;
            woken.borrow_mut().vruntime = 90;
            queues.active_executing = Some(preempted.clone());
            queues.in_slice = true;
        }
        ex.finish_task_queue_run(preempted.clone(), Duration::ZERO, 1, Some((Some(1), 100)));
        {
            let mut queues = ex.context.queues.borrow_mut();
            queues.maybe_activate(woken.clone());
            queues.maybe_activate(preempted.clone());
        }
        assert!(
            woken.borrow().vruntime < preempted.borrow().vruntime,
            "the woken queue at {} does not go ahead of the preempted one at {}",
            woken.borrow().vruntime,
            preempted.borrow().vruntime
        );
        ex.context.end_preempted_window();
    }

    /// Ends a preempted slice of a queue charged 7, which began with the least
    /// vruntime at 5, on a task that yielded and is held, with `others` more
    /// tasks queued ahead of it, then wakes another queue charged the same in
    /// the window, and returns the order the scheduler takes the two in.
    fn order_after_a_preempted_slice_that_keeps_its_queue_runnable(others: usize) -> Vec<String> {
        let ex = LocalExecutor::default();
        let queue = |name: &str| {
            let handle =
                ex.context
                    .create_task_queue(Shares::default(), Latency::NotImportant, name);
            ex.context.get_queue(&handle).unwrap()
        };
        let preempted = queue("preempted");
        let woken = queue("woken");
        for _ in 0..others {
            let (runnable, _, _) = unscheduled_in(&ex, preempted.clone());
            preempted.borrow_mut().runnables.push_back(runnable);
        }
        let (last, address, _) = unscheduled_in(&ex, preempted.clone());
        preempted.borrow_mut().runnables.push_back(last);
        {
            let mut queues = ex.context.queues.borrow_mut();
            queues.default_vruntime = 5;
            preempted.borrow_mut().vruntime = 7;
            preempted.borrow_mut().active = true;
            woken.borrow_mut().vruntime = 7;
            queues.active_executing = Some(preempted.clone());
            queues.in_slice = true;
        }
        ex.finish_task_queue_run(
            preempted.clone(),
            Duration::ZERO,
            1,
            Some((Some(address), 5)),
        );
        assert!(
            preempted.borrow().is_active(),
            "the preempted queue went idle"
        );
        ex.context.queues.borrow_mut().maybe_activate(woken.clone());
        assert!(
            woken.borrow().vruntime < preempted.borrow().vruntime,
            "the woken queue at {} does not go ahead of the preempted one at {}",
            woken.borrow().vruntime,
            preempted.borrow().vruntime
        );
        let order = std::iter::from_fn(|| ex.context.queues.borrow_mut().active_executors.pop())
            .map(|queue| queue.borrow().name.clone())
            .collect();
        ex.context.end_preempted_window();
        order
    }

    /// A queue the window woke goes ahead of the queue whose slice opened the
    /// window when both were charged the same and the preempted queue stayed
    /// runnable because the window holds its only task.
    #[test]
    fn a_woken_queue_charged_the_same_goes_ahead_of_a_preempted_queue_left_holding() {
        assert_eq!(
            order_after_a_preempted_slice_that_keeps_its_queue_runnable(0),
            ["woken", "preempted"]
        );
    }

    /// A queue the window woke goes ahead of the queue whose slice opened the
    /// window when both were charged the same and the preempted queue stayed
    /// runnable with tasks of its own queued.
    #[test]
    fn a_woken_queue_charged_the_same_goes_ahead_of_a_preempted_queue_with_tasks() {
        assert_eq!(
            order_after_a_preempted_slice_that_keeps_its_queue_runnable(1),
            ["woken", "preempted"]
        );
    }

    /// A queue the window woke goes ahead of the queue whose slice opened the
    /// window when both were charged the same, whichever woke first.
    ///
    /// The slice began with the least vruntime at 100, and the preempted queue
    /// went idle at the 150 it was charged, with no other queue runnable, so
    /// 150 is the cached least vruntime. The preempted queue and `woken`, also
    /// charged 150, both wake in the window, the preempted one first.
    #[test]
    fn a_woken_queue_charged_the_same_goes_ahead_of_the_preempted_queue() {
        let ex = LocalExecutor::default();
        let queue = |name: &str| {
            let handle =
                ex.context
                    .create_task_queue(Shares::default(), Latency::NotImportant, name);
            ex.context.get_queue(&handle).unwrap()
        };
        let preempted = queue("preempted");
        let woken = queue("woken");
        {
            let mut queues = ex.context.queues.borrow_mut();
            queues.default_vruntime = 100;
            preempted.borrow_mut().vruntime = 150;
            preempted.borrow_mut().active = true;
            woken.borrow_mut().vruntime = 150;
            queues.active_executing = Some(preempted.clone());
            queues.in_slice = true;
        }
        ex.finish_task_queue_run(preempted.clone(), Duration::ZERO, 1, Some((Some(1), 100)));
        assert!(
            !preempted.borrow().is_active(),
            "the preempted queue is runnable"
        );
        assert_eq!(ex.context.queues.borrow().default_vruntime, 150);
        {
            let mut queues = ex.context.queues.borrow_mut();
            queues.maybe_activate(preempted.clone());
            queues.maybe_activate(woken.clone());
        }
        assert!(
            woken.borrow().vruntime < preempted.borrow().vruntime,
            "the woken queue at {} does not go ahead of the preempted one at {}",
            woken.borrow().vruntime,
            preempted.borrow().vruntime
        );
        let order: Vec<String> =
            std::iter::from_fn(|| ex.context.queues.borrow_mut().active_executors.pop())
                .map(|queue| queue.borrow().name.clone())
                .collect();
        assert_eq!(order, ["woken", "preempted"]);
        ex.context.end_preempted_window();
    }

    /// Idle queues of equal shares woken one after another run in the order
    /// they were woken.
    ///
    /// Each is activated just behind the least vruntime, so all five tie.
    /// The heap alone breaks such a tie in no particular order: it ran these
    /// as 0, 2, 4, 1, 3.
    ///
    /// The preempt timer is pushed out of reach: a slice it ended before its
    /// first poll would put its queue back behind the others.
    #[test]
    fn idle_queues_of_equal_shares_run_in_the_order_they_were_woken() {
        let ex = LocalExecutorBuilder::default()
            .preempt_timer(Duration::from_secs(3600))
            .make()
            .unwrap();
        let order = ex.run(async {
            let order = Rc::new(RefCell::new(Vec::new()));
            let tasks: Vec<_> = (0..5)
                .map(|i| {
                    let queue = crate::executor().create_task_queue(
                        Shares::default(),
                        Latency::NotImportant,
                        "woken",
                    );
                    crate::spawn_local_into(
                        enclose! { (order) async move { order.borrow_mut().push(i) }},
                        queue,
                    )
                    .unwrap()
                })
                .collect();
            for task in tasks {
                task.await;
            }
            Rc::try_unwrap(order).unwrap().into_inner()
        });
        assert_eq!(order, [0, 1, 2, 3, 4]);
    }

    /// A queue its slice leaves runnable goes back behind the queues of the
    /// same vruntime that were waiting, though it entered the heap before
    /// any of them, so that queues of equal vruntime take turns.
    #[test]
    fn a_queue_repushed_after_its_slice_goes_behind_queues_of_equal_vruntime() {
        let ex = LocalExecutor::default();
        let queue = |name: &str| {
            let handle =
                ex.context
                    .create_task_queue(Shares::default(), Latency::NotImportant, name);
            ex.context.get_queue(&handle).unwrap()
        };
        let repushed = queue("repushed");
        let waiting = ["a", "b", "c", "d"].map(queue);
        let (runnable, _, _) = unscheduled_in(&ex, repushed.clone());
        repushed.borrow_mut().runnables.push_back(runnable);
        {
            let mut queues = ex.context.queues.borrow_mut();
            queues.default_vruntime = 10;
            queues.maybe_activate(repushed.clone());
            let running = queues.active_executors.pop().unwrap();
            assert!(Rc::ptr_eq(&running, &repushed));
            queues.active_executing = Some(running);
            queues.in_slice = true;
            for queue in &waiting {
                queues.maybe_activate(queue.clone());
            }
        }
        ex.finish_task_queue_run(repushed.clone(), Duration::ZERO, 1, None);
        assert_eq!(repushed.borrow().vruntime, waiting[0].borrow().vruntime);
        let order: Vec<String> =
            std::iter::from_fn(|| ex.context.queues.borrow_mut().active_executors.pop())
                .map(|queue| queue.borrow().name.clone())
                .collect();
        assert_eq!(order, ["a", "b", "c", "d", "repushed"]);
    }

    /// The queues waiting when charging a slice overflows its queue's
    /// vruntime, all reset to zero, run in the order they entered the heap.
    ///
    /// `a` entered first and `b` second, at a lower vruntime, so `b` is at
    /// the top of the heap when the reset levels them.
    #[test]
    fn queues_reset_by_a_vruntime_overflow_run_in_the_order_they_entered() {
        let ex = LocalExecutor::default();
        let queue = |name: &str| {
            let handle =
                ex.context
                    .create_task_queue(Shares::default(), Latency::NotImportant, name);
            ex.context.get_queue(&handle).unwrap()
        };
        let overflowed = queue("overflowed");
        let a = queue("a");
        let b = queue("b");
        let (runnable, _, _) = unscheduled_in(&ex, overflowed.clone());
        overflowed.borrow_mut().runnables.push_back(runnable);
        {
            let mut queues = ex.context.queues.borrow_mut();
            for (queue, vruntime) in [(&a, 10), (&b, 5)] {
                queue.borrow_mut().vruntime = vruntime;
                queue.borrow_mut().active = true;
                queues.push_runnable(queue.clone());
            }
            overflowed.borrow_mut().vruntime = u64::MAX;
            overflowed.borrow_mut().active = true;
            queues.active_executing = Some(overflowed.clone());
            queues.in_slice = true;
        }
        ex.finish_task_queue_run(overflowed.clone(), Duration::from_micros(1), 1, None);
        assert_eq!(overflowed.borrow().vruntime, 0);
        let order: Vec<String> =
            std::iter::from_fn(|| ex.context.queues.borrow_mut().active_executors.pop())
                .map(|queue| queue.borrow().name.clone())
                .collect();
        assert_eq!(order, ["a", "b", "overflowed"]);
    }

    #[test]
    fn need_preempt_outside_an_executor_is_false() {
        assert!(!executor().need_preempt());
    }

    #[test]
    #[should_panic(expected = "ring depth should be at least 2")]
    fn ring_depth_below_the_longest_link_chain_is_rejected() {
        let _ = LocalExecutorBuilder::default().ring_depth(1);
    }

    #[test]
    #[should_panic(expected = "ring depth should be at least 2")]
    fn pool_ring_depth_below_the_longest_link_chain_is_rejected() {
        let _ = LocalExecutorPoolBuilder::new(PoolPlacement::Unbound(1)).ring_depth(1);
    }

    #[test]
    fn smallest_ring_depth_runs() {
        let ex = LocalExecutorBuilder::default()
            .ring_depth(2)
            .make()
            .unwrap();
        ex.run(async {
            crate::timer::sleep(Duration::from_millis(1)).await;
        });
    }

    #[test]
    fn create_and_destroy_executor() {
        let mut var = Rc::new(RefCell::new(0));
        let local_ex = LocalExecutor::default();
        let varclone = var.clone();
        local_ex.run(async move {
            let mut m = varclone.borrow_mut();
            *m += 10;
        });

        let v = Rc::get_mut(&mut var).unwrap();
        let v = v.replace(0);
        assert_eq!(v, 10);
    }

    #[test]
    /// If you have a system with 4 billion CPUs let me know and I will
    /// update this test.
    fn create_fail_to_bind() {
        if LocalExecutorBuilder::new(Placement::Fixed(usize::MAX))
            .make()
            .is_ok()
        {
            unreachable!("Should have failed");
        }
    }

    #[test]
    /// libc supports cpu ids up to 1023 and will use the intersection of values
    /// specified by the cpu mask and those present on the system
    /// <https://man7.org/linux/man-pages/man2/sched_setaffinity.2.html#NOTES>
    fn bind_to_cpu_set_range() {
        assert!(bind_to_cpu_set(vec![0, 1, 2, 3]).is_ok());
        assert!(bind_to_cpu_set(0..1024).is_ok());
        assert!(bind_to_cpu_set(0..1025).is_err());
    }

    /// A CPU id past what the mask can hold is refused while the set is
    /// built, before any syscall, and the caller still sees EINVAL.
    #[test]
    fn a_cpu_id_beyond_the_mask_reports_einval() {
        let err = bind_to_cpu_set([1024]).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
    }

    /// An empty set is refused by `sched_setaffinity` itself, and the
    /// caller sees the errno the kernel returned.
    #[test]
    fn an_empty_cpu_set_reports_the_kernel_errno() {
        let err = bind_to_cpu_set(std::iter::empty()).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn create_and_bind() {
        if let Err(x) = LocalExecutorBuilder::new(Placement::Fixed(0)).make() {
            panic!("got error {:?}", x);
        }
    }

    #[test]
    #[should_panic]
    fn spawn_without_executor() {
        let _ = LocalExecutor::default();
        std::mem::drop(crate::spawn_local(async move {}));
    }

    #[test]
    fn invalid_task_queue() {
        let local_ex = LocalExecutor::default();
        local_ex.run(async {
            let task = crate::spawn_local_into(
                async move {
                    unreachable!("Should not have executed this");
                },
                TaskQueueHandle { index: 1 },
            );

            if task.is_ok() {
                unreachable!("Should have failed");
            }
        });
    }

    #[test]
    /// `executed_last` tracks the last task to run: 0 means no one, then task
    /// 1, then task 2, and so on.
    fn ten_yielding_queues() {
        let local_ex = LocalExecutor::default();

        let executed_last = Rc::new(RefCell::new(0));
        local_ex.run(async {
            let mut joins = Vec::with_capacity(10);
            for id in 1..11 {
                let exec = executed_last.clone();
                joins.push(crate::spawn_local(async move {
                    for _ in 0..10_000 {
                        {
                            let mut last = exec.borrow_mut();
                            assert_ne!(id, *last);
                            *last = id;
                        }
                        crate::executor().yield_task_queue_now().await;
                    }
                }));
            }
            futures::future::join_all(joins).await;
        });
    }

    #[test]
    /// The first task busy-loops and makes sure that it yields when it has
    /// to; the second task, in case it is executed first, yields to the
    /// other task.
    ///
    /// The first task loops until `need_preempt` is set. It is set to 2ms,
    /// but because this is a test and can be running overcommitted or in
    /// whichever shared infrastructure, we'll allow the timer to fire in up
    /// to 1s. If it didn't fire in 1s, that's broken.
    fn task_with_latency_requirements() {
        let local_ex = LocalExecutor::default();

        local_ex.run(async {
            let not_latency = crate::executor().create_task_queue(
                Shares::default(),
                Latency::NotImportant,
                "test",
            );
            let latency = crate::executor().create_task_queue(
                Shares::default(),
                Latency::Matters(Duration::from_millis(2)),
                "testlat",
            );

            let nolat_started = Rc::new(RefCell::new(false));
            let lat_status = Rc::new(RefCell::new(false));

            let nolat = local_ex
                .context
                .spawn_into(
                    crate::enclose! { (nolat_started, lat_status)
                        async move {
                            *(nolat_started.borrow_mut()) = true;

                            let start = Instant::now();
                            loop {
                                if *(lat_status.borrow()) {
                                    break;
                                }
                                if start.elapsed().as_secs() > 1 {
                                    panic!("Never received preempt signal");
                                }
                                crate::yield_if_needed().await;
                            }
                        }
                    },
                    not_latency,
                )
                .unwrap();

            let lat = local_ex
                .context
                .spawn_into(
                    crate::enclose! { (nolat_started, lat_status)
                        async move {
                            loop {
                                if !(*(nolat_started.borrow())) {
                                    crate::executor().yield_task_queue_now().await;
                                } else {
                                    break;
                                }
                            }
                            *(lat_status.borrow_mut()) = true;
                        }
                    },
                    latency,
                )
                .unwrap();

            futures::join!(nolat, lat);
        });
    }

    #[test]
    fn current_task_queue_matches() {
        let local_ex = LocalExecutor::default();
        local_ex.run(async {
            let tq1 = crate::executor().create_task_queue(
                Shares::default(),
                Latency::NotImportant,
                "test1",
            );
            let tq2 = crate::executor().create_task_queue(
                Shares::default(),
                Latency::NotImportant,
                "test2",
            );

            let id1 = tq1.index;
            let id2 = tq2.index;
            let j0 = crate::spawn_local(async {
                assert_eq!(crate::executor().current_task_queue().index, 0);
            });
            let j1 = crate::spawn_local_into(
                async move {
                    assert_eq!(crate::executor().current_task_queue().index, id1);
                },
                tq1,
            )
            .unwrap();
            let j2 = crate::spawn_local_into(
                async move {
                    assert_eq!(crate::executor().current_task_queue().index, id2);
                },
                tq2,
            )
            .unwrap();
            futures::join!(j0, j1, j2);
        })
    }

    #[test]
    /// The first task busy-loops and makes sure that it yields when it has
    /// to; the second task, in case it is executed first, yields to the
    /// other task.
    fn task_optimized_for_throughput() {
        let local_ex = LocalExecutor::default();

        local_ex.run(async {
            let tq1 = crate::executor().create_task_queue(
                Shares::default(),
                Latency::NotImportant,
                "test",
            );
            let tq2 = crate::executor().create_task_queue(
                Shares::default(),
                Latency::NotImportant,
                "testlat",
            );

            let first_started = Rc::new(RefCell::new(0));
            let second_status = Rc::new(RefCell::new(0));

            let first = local_ex
                .context
                .spawn_into(
                    crate::enclose! { (first_started, second_status)
                        async move {
                            let start = Instant::now();
                            loop {
                                {
                                    let mut count = first_started.borrow_mut();
                                    *count += 1;

                                    if start.elapsed().as_millis() >= 99 {
                                        break;
                                    }

                                    if *count < *(second_status.borrow()) {
                                        panic!("I was preempted but should not have been");
                                    }
                                }
                                crate::yield_if_needed().await;
                            }
                        }
                    },
                    tq1,
                )
                .unwrap();

            let second = local_ex
                .context
                .spawn_into(
                    crate::enclose! { (first_started, second_status)
                        async move {
                            loop {
                                {
                                    let mut count = second_status.borrow_mut();
                                    *count += 1;
                                    if *count < *(first_started.borrow()) {
                                       break;
                                    }
                                }
                                crate::executor().yield_task_queue_now().await;
                            }
                        }
                    },
                    tq2,
                )
                .unwrap();

            futures::join!(first, second);
        });
    }

    #[test]
    fn test_detach() {
        let ex = LocalExecutor::default();

        ex.run(async {
            crate::spawn_local(async {
                loop {
                    crate::executor().yield_task_queue_now().await;
                }
            })
            .detach();

            Timer::new(Duration::from_micros(100)).await;
        });
    }

    fn getrusage() -> Duration {
        use nix::sys::{
            resource::{getrusage, UsageWho},
            time::TimeValLike,
        };
        let usage = getrusage(UsageWho::RUSAGE_THREAD).expect("getrusage failed");
        let micros = usage.user_time().num_microseconds() + usage.system_time().num_microseconds();
        Duration::from_micros(micros as u64)
    }

    /// Drives the path the hang was found on: several queues with a short
    /// latency budget, all yielding, so the pass over the queues is
    /// interrupted by preemption repeatedly. Paired with the debug_assert
    /// at the park site, a queue left runnable while the executor sleeps
    /// fails here rather than hanging.
    #[test]
    fn preemption_heavy_workload_makes_progress() {
        LocalExecutor::default().run(async {
            let mut tasks = Vec::new();
            for i in 0..4 {
                let tq = crate::executor().create_task_queue(
                    Shares::Static(1000),
                    Latency::Matters(Duration::from_micros(100)),
                    "preempt",
                );
                tasks.push(
                    crate::spawn_local_into(
                        async move {
                            for _ in 0..(200 * (i + 1)) {
                                crate::executor().yield_task_queue_now().await;
                            }
                        },
                        tq,
                    )
                    .unwrap(),
                );
            }
            for t in tasks {
                t.await;
            }
        });
    }

    #[test]
    fn test_no_spin() {
        let ex = LocalExecutor::default();
        let task_queue = ex.create_task_queue(
            Shares::default(),
            Latency::Matters(Duration::from_millis(10)),
            "my_tq",
        );
        let start = getrusage();
        ex.run(async {
            crate::spawn_local_into(
                async { timer::sleep(Duration::from_secs(1)).await },
                task_queue,
            )
            .expect("failed to spawn task")
            .await;
        });

        assert!(
            getrusage() - start < Duration::from_millis(10),
            "expected user time on LE is less than 10 millisecond"
        );
    }

    #[test]
    /// Compared against the parked executor, not an absolute figure:
    /// `getrusage` reports the CPU this thread was given, so a fixed
    /// threshold really asserts that we own a core. Sharing one halves
    /// the number, which is where CI's old ~49.8 ms came from. A
    /// spinning executor still burns orders of magnitude more than a
    /// parked one whatever share it gets.
    fn test_spin() {
        let dur = Duration::from_secs(1);
        let ex0 = LocalExecutorBuilder::default().make().unwrap();
        let parked_cpu = ex0.run(async {
            let ex0_ru_start = getrusage();
            timer::sleep(dur).await;
            let ex0_ru_finish = getrusage();

            let used = ex0_ru_finish - ex0_ru_start;
            assert!(
                used < Duration::from_millis(10),
                "expected user time on LE0 is less than 10 millisecond"
            );
            used
        });

        let ex = LocalExecutorBuilder::new(Placement::Fixed(0))
            .spin_before_park(Duration::from_millis(100))
            .make()
            .unwrap();

        ex.run(async {
            let ex_ru_start = getrusage();
            timer::sleep(dur).await;
            let ex_ru_finish = getrusage();
            let spinning_cpu = ex_ru_finish - ex_ru_start;

            let floor = 10 * parked_cpu.max(Duration::from_millis(1));
            assert!(
                spinning_cpu >= floor,
                "expected the spinning executor to use at least {floor:?} of CPU, \
                 ten times the {parked_cpu:?} the parked one used, but it used {spinning_cpu:?}",
            );
        });
    }

    #[test]
    /// 5s of `spin_before_park` ensures the entire sleep should spin.
    fn test_runtime_stats() {
        let dur = Duration::from_secs(2);
        let ex0 = LocalExecutorBuilder::default().make().unwrap();
        ex0.run(async {
            assert!(
                crate::executor().executor_stats().total_runtime() < Duration::from_nanos(10),
                "expected runtime on LE {:#?} is less than 10 ns",
                crate::executor().executor_stats().total_runtime()
            );

            let now = Instant::now();
            while now.elapsed().as_millis() < 200 {}
            crate::executor().yield_task_queue_now().await;
            assert!(
                crate::executor().executor_stats().total_runtime() >= Duration::from_millis(200),
                "expected runtime on LE0 {:#?} is greater than 200 ms",
                crate::executor().executor_stats().total_runtime()
            );

            timer::sleep(dur).await;
            assert!(
                crate::executor().executor_stats().total_runtime() < Duration::from_millis(400),
                "expected runtime on LE0 {:#?} is not greater than 400 ms",
                crate::executor().executor_stats().total_runtime()
            );
        });

        let ex = LocalExecutorBuilder::new(Placement::Fixed(0))
            .spin_before_park(Duration::from_secs(5))
            .make()
            .unwrap();
        ex.run(async {
            crate::spawn_local(async move {
                assert!(
                    crate::executor().executor_stats().total_runtime() < Duration::from_nanos(10),
                    "expected runtime on LE {:#?} is less than 10 ns",
                    crate::executor().executor_stats().total_runtime()
                );

                let now = Instant::now();
                while now.elapsed().as_millis() < 200 {}
                crate::executor().yield_task_queue_now().await;
                assert!(
                    crate::executor().executor_stats().total_runtime()
                        >= Duration::from_millis(200),
                    "expected runtime on LE {:#?} is greater than 200 ms",
                    crate::executor().executor_stats().total_runtime()
                );
                timer::sleep(dur).await;
                assert!(
                    crate::executor().executor_stats().total_runtime() < Duration::from_millis(400),
                    "expected runtime on LE {:#?} is not greater than 400 ms",
                    crate::executor().executor_stats().total_runtime()
                );
            })
            .await;
        });
    }

    /// Spin for 2ms and then yield. How many shares we have should control
    /// how many quantas we manage to execute.
    async fn work_quanta() {
        let now = Instant::now();
        while now.elapsed().as_millis() < 2 {}
        crate::executor().yield_task_queue_now().await;
    }

    /// Runs two queues with the given static shares and validates that the
    /// share ratio is respected. A latency queue is used, otherwise a queue
    /// will run for too long uninterrupted and we'd have to run this test
    /// for a very long time for things to equalize.
    macro_rules! test_static_shares {
        ( $s1:expr, $s2:expr, $work:block ) => {
            let local_ex = LocalExecutor::default();

            local_ex.run(async {
                let tq1 = crate::executor().create_task_queue(
                    Shares::Static($s1),
                    Latency::Matters(Duration::from_millis(1)),
                    "test_1",
                );
                let tq2 = crate::executor().create_task_queue(
                    Shares::Static($s2),
                    Latency::Matters(Duration::from_millis(1)),
                    "test_2",
                );

                let tq1_count = Rc::new(Cell::new(0));
                let tq2_count = Rc::new(Cell::new(0));
                let now = Instant::now();

                let t1 = crate::spawn_local_into(
                    enclose! { (tq1_count, now) async move {
                        while now.elapsed().as_secs() < 5 {
                            $work;
                            tq1_count.replace(tq1_count.get() + 1);
                        }
                    }},
                    tq1,
                )
                .unwrap();

                let t2 = crate::spawn_local_into(
                    enclose! { (tq2_count, now ) async move {
                        while now.elapsed().as_secs() < 5 {
                            $work;
                            tq2_count.replace(tq2_count.get() + 1);
                        }
                    }},
                    tq2,
                )
                .unwrap();

                join!(t1, t2);

                let expected_ratio = $s2 as f64 / (($s2 + $s1) as f64);
                let actual_ratio =
                    tq2_count.get() as f64 / ((tq1_count.get() + tq2_count.get()) as f64);

                assert!(
                    (expected_ratio - actual_ratio).abs() < 0.1,
                    "be gentle: we don't know if we're running against other \
                     threads, under which conditions, etc; expected ratio \
                     {expected_ratio}, actual ratio {actual_ratio}"
                );
            });
        };
    }

    #[test]
    fn test_shares_high_disparity_fat_task() {
        test_static_shares!(1000, 10, { work_quanta().await });
    }

    #[test]
    fn test_shares_low_disparity_fat_task() {
        test_static_shares!(1000, 1000, { work_quanta().await });
    }

    #[test]
    fn test_allocate_dma_buffer() {
        LocalExecutor::default().run(async {
            let mut buffer = crate::allocate_dma_buffer(42);
            assert_eq!(buffer.len(), 42);
            buffer.as_bytes_mut()[0] = 12;
            buffer.as_bytes_mut()[12] = 13;
            assert_eq!(buffer.as_bytes_mut().len(), 42);
            assert_eq!(buffer.as_bytes()[0], 12);
            assert_eq!(buffer.as_bytes()[12], 13);
        });
    }

    #[test]
    fn test_allocate_dma_buffer_global() {
        LocalExecutor::default().run(async {
            let mut buffer = crate::allocate_dma_buffer_global(42);
            assert_eq!(buffer.len(), 42);
            assert_eq!(buffer.len(), 42);
            buffer.as_bytes_mut()[0] = 12;
            buffer.as_bytes_mut()[12] = 13;
            assert_eq!(buffer.as_bytes_mut().len(), 42);
            assert_eq!(buffer.as_bytes()[0], 12);
            assert_eq!(buffer.as_bytes()[12], 13);
        });
    }

    struct DynamicSharesTest {
        shares: Cell<usize>,
    }

    impl DynamicSharesTest {
        fn new() -> Rc<Self> {
            Rc::new(Self {
                shares: Cell::new(0),
            })
        }
        fn tick(&self, millis: u64) {
            if millis < 1000 {
                self.shares.replace(1);
            } else {
                self.shares.replace(1000);
            }
        }
    }

    impl SharesManager for DynamicSharesTest {
        fn shares(&self) -> usize {
            self.shares.get()
        }

        fn adjustment_period(&self) -> Duration {
            Duration::from_millis(1)
        }
    }

    #[test]
    /// `tq1` is the reference task queue.
    ///
    /// Keep this very simple because every new processor, every load
    /// condition, will yield different results. All we want to validate is:
    /// for a large part of the first two seconds shares were very low, we
    /// should have received very low ratio. On the second half we should
    /// have accumulated much more. Real numbers are likely much higher than
    /// the targets, but those targets are safe.
    ///
    /// The second window opens when `tq2` first runs after its shares go up,
    /// not at the one second mark. Runtime is wall-clock time, so a
    /// preemption by the operating system that lands while `tq2` runs at one
    /// share is charged to it a thousand times over, and `tq1` runs alone
    /// until that is paid back: a 3ms preemption costs `tq2` three seconds.
    /// A window opened on the clock would measure that debt rather than the
    /// new shares.
    fn test_dynamic_shares() {
        let local_ex = LocalExecutor::default();

        local_ex.run(async {
            let bm = DynamicSharesTest::new();
            let tq1 = crate::executor().create_task_queue(
                Shares::Static(1000),
                Latency::Matters(Duration::from_millis(1)),
                "test_1",
            );
            let tq2 = crate::executor().create_task_queue(
                Shares::Dynamic(bm.clone()),
                Latency::Matters(Duration::from_millis(1)),
                "test_2",
            );

            let tq1_count = Rc::new(RefCell::new(vec![0, 0]));
            let tq2_count = Rc::new(RefCell::new(vec![0, 0]));
            let now = Instant::now();
            let window = Duration::from_secs(1);
            let raised: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));

            let t1 = crate::spawn_local_into(
                enclose! { (tq1_count, now, raised) async move {
                    loop {
                        match raised.get() {
                            Some(at) if at.elapsed() >= window => break,
                            Some(_) => (*tq1_count.borrow_mut())[1] += 1,
                            None if now.elapsed() < window => (*tq1_count.borrow_mut())[0] += 1,
                            None => assert!(
                                now.elapsed() < Duration::from_secs(60),
                                "tq2 never ran again after its shares went up"
                            ),
                        }
                        crate::executor().yield_task_queue_now().await;
                    }
                }},
                tq1,
            )
            .unwrap();

            let t2 = crate::spawn_local_into(
                enclose! { (tq2_count, now, bm, raised) async move {
                    loop {
                        let elapsed = now.elapsed();
                        bm.tick(elapsed.as_millis() as u64);
                        if elapsed >= window && raised.get().is_none() {
                            raised.set(Some(Instant::now()));
                        }
                        match raised.get() {
                            Some(at) if at.elapsed() >= window => break,
                            Some(_) => (*tq2_count.borrow_mut())[1] += 1,
                            None => (*tq2_count.borrow_mut())[0] += 1,
                        }
                        crate::executor().yield_task_queue_now().await;
                    }
                }},
                tq2,
            )
            .unwrap();

            join!(t1, t2);
            let ratios: Vec<f64> = tq1_count
                .borrow()
                .iter()
                .zip(tq2_count.borrow().iter())
                .map(|(x, y)| *y as f64 / *x as f64)
                .collect();
            assert!(ratios[1] > ratios[0], "ratios {ratios:?}");
            assert!(ratios[0] < 0.25, "ratios {ratios:?}");
            assert!(ratios[1] > 0.50, "ratios {ratios:?}");
        });
    }

    #[test]
    /// Issue 241: the presence of the second detached waiter caused the
    /// program to hang.
    fn multiple_spawn() {
        LocalExecutor::default().run(async {
            crate::spawn_local(async {}).detach().await;
            crate::spawn_local(async {}).detach().await;
        });
    }

    #[test]
    #[should_panic(expected = "Message!")]
    fn panic_is_not_list() {
        LocalExecutor::default().run(async { panic!("Message!") });
    }

    struct TestFuture {
        w: Arc<Mutex<Option<Waker>>>,
    }

    impl Future for TestFuture {
        type Output = ();
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut w = self.w.lock().unwrap();
            match w.take() {
                Some(_) => Poll::Ready(()),
                None => {
                    *w = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        }
    }

    #[test]
    fn cross_executor_wake_by_ref() {
        let w = Arc::new(Mutex::new(None));
        let t = w.clone();

        let fut = TestFuture { w };

        let ex1 = LocalExecutorBuilder::default()
            .spawn(|| async move {
                fut.await;
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(|| async move {
                loop {
                    sleep(Duration::from_secs(1)).await;
                    let w = t.lock().unwrap();
                    if let Some(ref x) = *w {
                        x.wake_by_ref();
                        return;
                    }
                }
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn cross_executor_wake_by_value() {
        let w = Arc::new(Mutex::new(None));
        let t = w.clone();

        let fut = TestFuture { w };

        let ex1 = LocalExecutorBuilder::default()
            .spawn(|| async move {
                fut.await;
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(|| async move {
                loop {
                    sleep(Duration::from_secs(1)).await;
                    let w = t.lock().unwrap();
                    if let Some(x) = w.clone() {
                        x.wake();
                        return;
                    }
                }
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    /// Wakes up the waker in a remote executor.
    #[test]
    fn cross_executor_wake_with_join_handle() {
        let w = Arc::new(Mutex::new(None));
        let t = w.clone();

        let fut = TestFuture { w };

        let ex1 = LocalExecutorBuilder::default()
            .spawn(|| async move {
                let x = crate::spawn_local(fut).detach();
                x.await;
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(|| async move {
                loop {
                    sleep(Duration::from_secs(1)).await;
                    let w = t.lock().unwrap();
                    if let Some(x) = w.clone() {
                        x.wake();
                        return;
                    }
                }
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    /// The other side won't be alive to get the notification. We should
    /// still survive.
    #[test]
    fn cross_executor_wake_early_drop() {
        let w = Arc::new(Mutex::new(None));
        let t = w.clone();

        let fut = TestFuture { w };

        let ex1 = LocalExecutorBuilder::default()
            .spawn(|| async move {
                let _drop = futures_lite::future::poll_once(fut).await;
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(|| async move {
                loop {
                    sleep(Duration::from_secs(1)).await;
                    let w = t.lock().unwrap();
                    if let Some(ref x) = *w {
                        x.wake_by_ref();
                        return;
                    }
                }
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    /// The other side won't be alive to get the notification and even worse,
    /// we hold a waker that we notify after the first executor is surely
    /// dead. We should still survive.
    #[test]
    fn cross_executor_wake_hold_waker() {
        let w = Arc::new(Mutex::new(None));
        let t = w.clone();

        let fut = TestFuture { w };

        let ex1 = LocalExecutorBuilder::default()
            .spawn(|| async move {
                let _drop = futures_lite::future::poll_once(fut).await;
            })
            .unwrap();
        ex1.join().unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(|| async move {
                let w = t.lock().unwrap().clone().unwrap();
                w.wake_by_ref();
            })
            .unwrap();

        ex2.join().unwrap();
    }

    #[test]
    fn executor_pool_builder() {
        let nr_cpus = 4;

        let count = Arc::new(AtomicUsize::new(0));
        let handles = LocalExecutorPoolBuilder::new(PoolPlacement::Unbound(nr_cpus))
            .on_all_shards({
                let count = Arc::clone(&count);
                || async move { count.fetch_add(1, Ordering::Relaxed) }
            })
            .unwrap();

        let _: std::thread::ThreadId = handles.handles[0].thread().id();

        assert_eq!(nr_cpus, handles.handles().iter().count());

        let mut fut_output = handles
            .join_all()
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>();

        fut_output.sort_unstable();

        assert_eq!(fut_output, (0..nr_cpus).collect::<Vec<_>>());

        assert_eq!(nr_cpus, count.load(Ordering::Relaxed));
    }

    #[test]
    fn executor_invalid_executor_count() {
        assert!(LocalExecutorPoolBuilder::new(PoolPlacement::Unbound(0))
            .on_all_shards(|| async move {})
            .is_err());
    }

    #[test]
    fn executor_pool_builder_placements() {
        let cpu_set = CpuSet::online().unwrap();
        assert!(!cpu_set.is_empty());

        for nn in 1..2 {
            let nr_execs = nn * cpu_set.len();
            let mut placements = vec![
                PoolPlacement::Unbound(nr_execs),
                PoolPlacement::Fenced(nr_execs, cpu_set.clone()),
                PoolPlacement::MaxSpread(nr_execs, None),
                PoolPlacement::MaxSpread(nr_execs, Some(cpu_set.clone())),
                PoolPlacement::MaxPack(nr_execs, None),
                PoolPlacement::MaxPack(nr_execs, Some(cpu_set.clone())),
            ];

            for pp in placements.drain(..) {
                let ids = Arc::new(Mutex::new(HashMap::new()));
                let cpus = Arc::new(Mutex::new(HashMap::new()));
                let cpu_hard_bind =
                    !matches!(pp, PoolPlacement::Unbound(_) | PoolPlacement::Fenced(_, _));

                let handles = LocalExecutorPoolBuilder::new(pp)
                    .on_all_shards({
                        let ids = Arc::clone(&ids);
                        let cpus = Arc::clone(&cpus);
                        || async move {
                            ids.lock()
                                .unwrap()
                                .entry(crate::executor().id())
                                .and_modify(|e| *e += 1)
                                .or_insert(1);

                            let pid = nix::unistd::Pid::from_raw(0);
                            let cpu = nix::sched::sched_getaffinity(pid).unwrap();
                            cpus.lock()
                                .unwrap()
                                .entry(cpu)
                                .and_modify(|e| *e += 1)
                                .or_insert(1);
                        }
                    })
                    .unwrap();

                assert_eq!(nr_execs, handles.handles().len());
                handles
                    .join_all()
                    .into_iter()
                    .for_each(|r| assert!(r.is_ok()));

                assert_eq!(nr_execs, ids.lock().unwrap().len());
                ids.lock().unwrap().values().for_each(|v| assert_eq!(*v, 1));

                if cpu_hard_bind {
                    assert_eq!(nr_execs, cpus.lock().unwrap().len());
                    cpus.lock()
                        .unwrap()
                        .values()
                        .for_each(|v| assert_eq!(*v, nn));
                }
            }
        }
    }

    #[test]
    /// Confirms that we can always get shards up to the # of cpus, and that
    /// some placements fail when shards are # of cpus + 1 (the
    /// `shard_limited` ones).
    fn executor_pool_builder_shards_limit() {
        let cpu_set = CpuSet::online().unwrap();
        assert!(!cpu_set.is_empty());

        {
            let mut placements = vec![
                (false, PoolPlacement::Unbound(cpu_set.len())),
                (false, PoolPlacement::Fenced(cpu_set.len(), cpu_set.clone())),
                (true, PoolPlacement::MaxSpread(cpu_set.len(), None)),
                (
                    true,
                    PoolPlacement::MaxSpread(cpu_set.len(), Some(cpu_set.clone())),
                ),
                (true, PoolPlacement::MaxPack(cpu_set.len(), None)),
                (
                    true,
                    PoolPlacement::MaxPack(cpu_set.len(), Some(cpu_set.clone())),
                ),
            ];

            for (_shard_limited, p) in placements.drain(..) {
                LocalExecutorPoolBuilder::new(p)
                    .on_all_shards(|| async move {})
                    .unwrap()
                    .join_all();
            }
        }

        {
            let mut placements = vec![
                (false, PoolPlacement::Unbound(1 + cpu_set.len())),
                (
                    false,
                    PoolPlacement::Fenced(1 + cpu_set.len(), cpu_set.clone()),
                ),
                (true, PoolPlacement::MaxSpread(1 + cpu_set.len(), None)),
                (
                    true,
                    PoolPlacement::MaxSpread(1 + cpu_set.len(), Some(cpu_set.clone())),
                ),
                (true, PoolPlacement::MaxPack(1 + cpu_set.len(), None)),
                (
                    true,
                    PoolPlacement::MaxPack(1 + cpu_set.len(), Some(cpu_set)),
                ),
            ];

            for (shard_limited, p) in placements.drain(..) {
                match LocalExecutorPoolBuilder::new(p).on_all_shards(|| async move {}) {
                    Ok(handles) => {
                        handles.join_all();
                        assert!(!shard_limited);
                    }
                    Err(_) => assert!(shard_limited),
                }
            }
        }
    }

    #[test]
    fn scoped_task() {
        LocalExecutor::default().run(async {
            let mut a = 1;
            unsafe {
                crate::spawn_scoped_local(async {
                    a = 2;
                })
                .await;
            }
            crate::executor().yield_task_queue_now().await;
            assert_eq!(a, 2);

            let mut a = 1;
            let do_later = unsafe {
                crate::spawn_scoped_local(async {
                    a = 2;
                })
            };

            crate::executor().yield_task_queue_now().await;
            do_later.await;
            assert_eq!(a, 2);
        });
    }

    #[test]
    fn executor_pool_builder_thread_panic() {
        let nr_execs = 8;
        let res = LocalExecutorPoolBuilder::new(PoolPlacement::Unbound(nr_execs))
            .on_all_shards(|| async move { panic!("join handle will be Err") })
            .unwrap()
            .join_all();

        assert_eq!(nr_execs, res.len());
        assert!(res.into_iter().all(|r| r.is_err()));
    }

    #[test]
    fn executor_pool_builder_return_values() {
        let nr_execs = 8;
        let x = Arc::new(AtomicUsize::new(0));
        let mut values = LocalExecutorPoolBuilder::new(PoolPlacement::Unbound(nr_execs))
            .on_all_shards(|| async move { x.fetch_add(1, Ordering::Relaxed) })
            .unwrap()
            .join_all()
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>();

        values.sort_unstable();
        assert_eq!(values, (0..nr_execs).collect::<Vec<_>>());
    }

    #[test]
    fn executor_pool_builder_spawn_cancel() {
        let nr_shards = 8;
        let builder = LocalExecutorPoolBuilder::new(PoolPlacement::Unbound(nr_shards));
        let nr_exectuted = Arc::new(AtomicUsize::new(0));

        let fut_gen = {
            let nr_exectuted = Arc::clone(&nr_exectuted);
            || async move {
                nr_exectuted.fetch_add(1, Ordering::Relaxed);
                unreachable!("should not execute")
            }
        };

        let mut handles = PoolThreadHandles::new();
        let mut cpu_set_gen = placement::CpuSetGenerator::pool(builder.placement.clone()).unwrap();
        let latch = Latch::new(builder.placement.executor_count());

        let ii_cxl = 2;
        for ii in 0..builder.placement.executor_count() {
            if ii == nr_shards - ii_cxl {
                std::thread::sleep(std::time::Duration::from_millis(100));
                assert!(ii_cxl <= latch.cancel().unwrap());
            }
            match builder.spawn_thread(&mut cpu_set_gen, &latch, fut_gen.clone()) {
                Ok(handle) => handles.push(handle),
                Err(_) => break,
            }
        }

        assert_eq!(0, nr_exectuted.load(Ordering::Relaxed));
        assert_eq!(nr_shards, handles.handles.len());
        handles.join_all().into_iter().for_each(|s| {
            assert!(format!("{}", s.unwrap_err()).contains("spawn failed"));
        });
    }

    #[should_panic]
    #[test]
    fn executor_inception() {
        LocalExecutor::default().run(async {
            LocalExecutor::default().run(async {});
        });
    }

    /// Fixture for the four regression tests against
    /// <https://github.com/DataDog/glommio/issues/379>, where we test
    /// against task reference count underflow. The tests include two
    /// scenarios, with join handles and with sleep; for each case we test
    /// both, wake and wake_by_ref.
    enum TaskState {
        Pending(Option<Waker>),
        Ready,
    }

    #[test]
    /// First task places the waker of self into the slot; when polled it
    /// checks for the result: if it's ready, returns `Ready`, otherwise
    /// returns `Pending`. Second task checks the slot for the first task's
    /// waker, wakes it by ref, and then it is dropped: the waker goes away
    /// when the slot is set to `TaskState::Ready`, bringing the refcount to
    /// zero.
    fn wake_by_ref_refcount_underflow_with_join_handle() {
        LocalExecutor::default().run(async {
            let slot: Rc<RefCell<TaskState>> = Rc::new(RefCell::new(TaskState::Pending(None)));
            let cloned_slot = slot.clone();
            let jh = crate::spawn_local(async move {
                poll_fn::<(), _>(|cx| {
                    let current = &mut *cloned_slot.borrow_mut();
                    match current {
                        TaskState::Pending(maybe_waker) => match maybe_waker {
                            Some(_) => unreachable!(),
                            None => {
                                *current = TaskState::Pending(Some(cx.waker().clone()));
                                Poll::Pending
                            }
                        },
                        TaskState::Ready => Poll::Ready(()),
                    }
                })
                .await;
            })
            .detach();
            let jh2 = crate::spawn_local(async move {
                let current = &mut *slot.borrow_mut();
                match current {
                    TaskState::Pending(maybe_waker) => {
                        let waker = maybe_waker.take().unwrap();
                        waker.wake_by_ref();
                        *current = TaskState::Ready;
                    }
                    TaskState::Ready => unreachable!("task cannot be ready at this time"),
                }
            })
            .detach();
            join_all(vec![jh, jh2]).await;
        });
    }

    #[test]
    fn wake_by_ref_refcount_underflow_with_sleep() {
        LocalExecutor::default().run(async {
            let slot: Rc<RefCell<TaskState>> = Rc::new(RefCell::new(TaskState::Pending(None)));
            let cloned_slot = slot.clone();
            crate::spawn_local(async move {
                poll_fn::<(), _>(|cx| {
                    let current = &mut *cloned_slot.borrow_mut();
                    match current {
                        TaskState::Pending(maybe_waker) => match maybe_waker {
                            Some(_) => unreachable!(),
                            None => {
                                *current = TaskState::Pending(Some(cx.waker().clone()));
                                Poll::Pending
                            }
                        },
                        TaskState::Ready => Poll::Ready(()),
                    }
                })
                .await;
            })
            .detach();
            crate::spawn_local(async move {
                let current = &mut *slot.borrow_mut();
                match current {
                    TaskState::Pending(maybe_waker) => {
                        let waker = maybe_waker.take().unwrap();
                        waker.wake_by_ref();
                        *current = TaskState::Ready;
                    }
                    TaskState::Ready => unreachable!(),
                }
            })
            .detach();
            timer::sleep(Duration::from_millis(1)).await;
        });
    }

    #[test]
    fn wake_refcount_underflow_with_join_handle() {
        LocalExecutor::default().run(async {
            let slot: Rc<RefCell<TaskState>> = Rc::new(RefCell::new(TaskState::Pending(None)));
            let cloned_slot = slot.clone();
            let jh = crate::spawn_local(async move {
                poll_fn::<(), _>(|cx| {
                    let current = &mut *cloned_slot.borrow_mut();
                    match current {
                        TaskState::Pending(maybe_waker) => match maybe_waker {
                            Some(_) => unreachable!(),
                            None => {
                                *current = TaskState::Pending(Some(cx.waker().clone()));
                                Poll::Pending
                            }
                        },
                        TaskState::Ready => Poll::Ready(()),
                    }
                })
                .await;
            })
            .detach();
            let jh2 = crate::spawn_local(async move {
                let current = &mut *slot.borrow_mut();
                match current {
                    TaskState::Pending(maybe_waker) => {
                        let waker = maybe_waker.take().unwrap();
                        waker.wake();
                        *current = TaskState::Ready;
                    }
                    TaskState::Ready => unreachable!(),
                }
            })
            .detach();
            join_all(vec![jh, jh2]).await;
        });
    }

    #[test]
    fn wake_refcount_underflow_with_sleep() {
        LocalExecutor::default().run(async {
            let slot: Rc<RefCell<TaskState>> = Rc::new(RefCell::new(TaskState::Pending(None)));
            let cloned_slot = slot.clone();
            crate::spawn_local(async move {
                poll_fn::<(), _>(|cx| {
                    let current = &mut *cloned_slot.borrow_mut();
                    match current {
                        TaskState::Pending(maybe_waker) => match maybe_waker {
                            Some(_) => unreachable!(),
                            None => {
                                *current = TaskState::Pending(Some(cx.waker().clone()));
                                Poll::Pending
                            }
                        },
                        TaskState::Ready => Poll::Ready(()),
                    }
                })
                .await;
            })
            .detach();
            crate::spawn_local(async move {
                let current = &mut *slot.borrow_mut();
                match current {
                    TaskState::Pending(maybe_waker) => {
                        let waker = maybe_waker.take().unwrap();
                        waker.wake();
                        *current = TaskState::Ready;
                    }
                    TaskState::Ready => unreachable!(),
                }
            })
            .detach();
            timer::sleep(Duration::from_millis(1)).await;
        });
    }

    #[test]
    fn wake_refcount_overflow() {
        LocalExecutor::default().run(async {
            const NUM_CLONES: usize = u16::MAX as usize;

            crate::spawn_local(poll_fn::<(), _>(move |cx| {
                let _wakers = Vec::from_iter((0..NUM_CLONES).map(|_| cx.waker().clone()));
                Poll::Ready(())
            }))
            .await;
        })
    }

    #[test]
    fn blocking_function() {
        LocalExecutor::default().run(async {
            let started = Instant::now();

            let blocking = executor().spawn_blocking(enclose!((started) move || {
                let now = Instant::now();
                while now.elapsed() < Duration::from_millis(100) {}
                started.elapsed()
            }));
            let coop = enclose!((started) async move {
                let now = Instant::now();
                while now.elapsed() < Duration::from_millis(100) {
                    yield_if_needed().await;
                }
                started.elapsed()
            });

            let (blocking, coop) = join(blocking, coop).await;

            assert!(blocking.as_millis() >= 100 && blocking.as_millis() < 150);
            assert!(coop.as_millis() >= 100 && coop.as_millis() < 150);
        });
    }

    #[test]
    /// We create 5 blocking jobs each taking 100ms but our thread pool only
    /// has 4 threads, so we expect one of those jobs to take twice as long
    /// as the others.
    fn blocking_function_parallelism() {
        LocalExecutorBuilder::new(Placement::Unbound)
            .blocking_thread_pool_placement(PoolPlacement::Unbound(4))
            .spawn(|| async {
                let started = Instant::now();
                let mut blocking = vec![];

                for _ in 0..5 {
                    blocking.push(executor().spawn_blocking(enclose!((started) move || {
                        let now = Instant::now();
                        while now.elapsed() < Duration::from_millis(100) {}
                        started.elapsed()
                    })));
                }

                let mut ts = join_all(blocking).await;
                assert_eq!(ts.len(), 5);

                ts.sort_unstable();
                for ts in ts.iter().take(4) {
                    assert!(ts.as_millis() >= 100 && ts.as_millis() < 150);
                }
                assert!(ts[4].as_millis() >= 200 && ts[4].as_millis() < 250);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn custom_pool_placement_binds_shards_in_order() {
        let pid = nix::unistd::Pid::from_raw(0);
        let affinity = nix::sched::sched_getaffinity(pid).unwrap();
        let cpus: Vec<usize> = (0..nix::sched::CpuSet::count())
            .filter(|&cpu| affinity.is_set(cpu).unwrap())
            .take(3)
            .collect();
        if cpus.len() < 2 {
            eprintln!(
                "Insufficient CPUs available to test custom_pool_placement_binds_shards_in_order (affinity only allows for {})",
                cpus.len(),
            );
            return;
        }

        let online = CpuSet::online().unwrap();
        let sets = cpus
            .iter()
            .map(|&cpu| online.clone().filter(|l| l.cpu == cpu))
            .collect();
        let bound: Vec<usize> = LocalExecutorPoolBuilder::new(PoolPlacement::Custom(sets))
            .on_all_shards(move || async move {
                let affinity = nix::sched::sched_getaffinity(pid).unwrap();
                (0..nix::sched::CpuSet::count())
                    .find(|&cpu| affinity.is_set(cpu).unwrap())
                    .unwrap()
            })
            .unwrap()
            .join_all()
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        assert_eq!(bound, cpus);
    }

    #[test]
    fn blocking_function_placement_independent_of_executor_placement() {
        let affinity = nix::sched::sched_getaffinity(nix::unistd::Pid::from_raw(0)).unwrap();
        let num_cpus_accessible_by_default = (0..nix::sched::CpuSet::count())
            .map(|cpu| affinity.is_set(cpu).unwrap() as usize)
            .sum::<usize>();
        if num_cpus_accessible_by_default < 2 {
            eprintln!(
                "Insufficient CPUs available to test blocking_function_placement_independent_of_executor_placement (affinity only allows for {})",
                num_cpus_accessible_by_default,
            );
            return;
        }

        let num_schedulable_cpus = LocalExecutorBuilder::new(Placement::Fixed(0))
            .blocking_thread_pool_placement(PoolPlacement::Unbound(2))
            .spawn(|| async {
                executor()
                    .spawn_blocking(move || {
                        let pid = nix::unistd::Pid::from_raw(0);
                        let affinity =
                            nix::sched::sched_getaffinity(pid).expect("Failed to get affinity");
                        (0..nix::sched::CpuSet::count())
                            .map(|cpu| {
                                affinity
                                    .is_set(cpu)
                                    .expect("Failed to check if cpu affinity is set")
                                    as usize
                            })
                            .sum::<usize>()
                    })
                    .await
            })
            .unwrap()
            .join()
            .unwrap();

        assert!(
            num_schedulable_cpus >= num_cpus_accessible_by_default,
            "num schedulable {}, num cpus accessible {}",
            num_schedulable_cpus,
            num_cpus_accessible_by_default,
        );
    }

    #[test]
    fn blocking_pool_invalid_placement() {
        let ret = LocalExecutorBuilder::new(Placement::Unbound)
            .blocking_thread_pool_placement(PoolPlacement::Unbound(0))
            .spawn(|| async {})
            .unwrap()
            .join();
        assert!(ret.is_err());
    }

    #[test]
    fn an_empty_custom_blocking_pool_is_a_shard_count_error() {
        let ret = LocalExecutorBuilder::new(Placement::Unbound)
            .blocking_thread_pool_placement(PoolPlacement::Custom(vec![]))
            .make();
        assert!(matches!(
            ret,
            Err(GlommioError::BuilderError(BuilderErrorKind::NrShards {
                minimum: 1,
                shards: 0
            }))
        ));
    }

    #[test]
    fn an_empty_custom_executor_pool_is_a_shard_count_error() {
        let ret = LocalExecutorPoolBuilder::new(PoolPlacement::Custom(vec![]))
            .on_all_shards(|| async move {});
        assert!(matches!(
            ret,
            Err(GlommioError::BuilderError(BuilderErrorKind::NrShards {
                minimum: 1,
                shards: 0
            }))
        ));
    }

    #[test]
    fn local_executor_unset() {
        LocalExecutor::default().run(async {});

        #[cfg(any(not(nightly), not(feature = "native-tls")))]

        assert!(!LOCAL_EX.is_set());

        #[cfg(all(nightly, feature = "native-tls"))]

        assert!(unsafe { LOCAL_EX.is_null() });
    }

    #[test]
    fn local_executor_unset_when_panic() {
        let res = std::panic::catch_unwind(|| {
            LocalExecutor::default().run(async {
                panic!("uh oh!");
            });
        });
        assert!(res.is_err());

        #[cfg(any(not(nightly), not(feature = "native-tls")))]

        assert!(!LOCAL_EX.is_set());

        #[cfg(all(nightly, feature = "native-tls"))]

        assert!(unsafe { LOCAL_EX.is_null() });
    }

    #[test]
    fn a_task_can_toggle_stall_detection_on_its_own_executor() {
        let ex = LocalExecutor::default();
        ex.run(async {
            ex.detect_stalls(Some(Box::new(stall::DefaultStallDetectionHandler {})))
                .unwrap();
            crate::executor().yield_task_queue_now().await;
            ex.detect_stalls(None).unwrap();
            crate::executor().yield_task_queue_now().await;
        });
    }

    #[test]
    fn an_executor_runs_again_after_a_task_panic_was_caught() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let ex = LocalExecutor::default();
            let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ex.run(async { panic!("the first run panics") })
            }));
            assert!(first.is_err());
            let second = ex.run(async { crate::spawn_local(async { 7 }).await });
            tx.send(second).unwrap();
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)),
            Ok(7),
            "the second run never finished"
        );
    }

    #[test]
    fn an_executor_with_a_preempt_timer_of_duration_max_runs_its_tasks() {
        let ex = LocalExecutorBuilder::default()
            .preempt_timer(Duration::MAX)
            .make()
            .unwrap();
        let out = ex.run(async {
            sleep(Duration::from_millis(1)).await;
            crate::spawn_local(async { 7 }).await
        });
        assert_eq!(out, 7);
    }

    #[test]
    fn a_task_queue_with_latency_of_duration_max_runs_its_tasks() {
        let ex = LocalExecutor::default();
        let out = ex.run(async {
            let tq = crate::executor().create_task_queue(
                Shares::default(),
                Latency::Matters(Duration::MAX),
                "forever",
            );
            crate::spawn_local_into(
                async {
                    sleep(Duration::from_millis(1)).await;
                    7
                },
                tq,
            )
            .unwrap()
            .await
        });
        assert_eq!(out, 7);
    }

    /// `spawn` and `run` both place work in the default queue, so removing it
    /// would leave them nowhere to go.
    #[test]
    fn the_default_queue_cannot_be_removed_before_run() {
        let ex = LocalExecutor::default();
        let removed = ex.remove_task_queue(TaskQueueHandle::default());
        assert!(
            matches!(
                removed,
                Err(GlommioError::ExecutorError(ExecutorErrorKind::QueueError {
                    index: 0,
                    kind: QueueErrorKind::StillActive,
                }))
            ),
            "removing the default queue returned {removed:?}"
        );
        let task = ex.spawn(async { 3 });
        assert_eq!(ex.run(async { task.await * 2 }), 6);
    }

    /// The main future lives in the default queue. A task in another queue
    /// removing it while the main future waits must not cancel the main future
    /// when it wakes.
    #[test]
    fn the_default_queue_cannot_be_removed_while_the_main_future_waits() {
        let ex = LocalExecutor::default();
        let out = ex.run(async {
            let tq = crate::executor().create_task_queue(
                Shares::default(),
                Latency::NotImportant,
                "remover",
            );
            let remover = crate::spawn_local_into(
                async { crate::executor().remove_task_queue(TaskQueueHandle::default()) },
                tq,
            )
            .unwrap();
            sleep(Duration::from_millis(1)).await;
            remover.await
        });
        assert!(
            matches!(
                out,
                Err(GlommioError::ExecutorError(ExecutorErrorKind::QueueError {
                    index: 0,
                    kind: QueueErrorKind::StillActive,
                }))
            ),
            "removing the default queue returned {out:?}"
        );
    }

    /// A `Latency::Matters` queue that wakes an idle queue and then yields
    /// while still runnable must not wait out the default preempt timer.
    ///
    /// `q` promises 5 ms and the executor's default is 1 s. `q` spawns a
    /// spinner into `n`, which only gives up the CPU when preempted, then
    /// yields in a loop and records the longest gap between its own polls.
    /// Every gap is a slice `n` ran, so the worst gap is how long the timer
    /// let `n` run while `q` was waiting. Activating `n` used to recompute the
    /// timer without the queue that was running, and the repush after that
    /// slice did not recompute it, so `n` ran under the 1 s default.
    ///
    /// `q` holds one share against `n`'s thousand, so its yields catch up with
    /// `n`'s vruntime in microseconds rather than taking a share of each slice
    /// for themselves, and every slice `n` gets is as long as the timer allows.
    ///
    /// Only slices after `n`'s first preemption are measured. The first runs
    /// under the timer armed before `q` was activated, in the same pass, which
    /// is a separate matter: a pass keeps the timer it started with.
    ///
    /// The bound is 250 ms: fifty times the promised 5 ms, so a loaded
    /// machine has room to be late, and a quarter of the 1 s the bug yields,
    /// so the timer cannot pass it by firing early.
    ///
    /// A stall before `q`'s first gap lets `n` finish its slices before `q`
    /// runs again, leaving nothing measured, so such a run is retried and the
    /// test fails if no run measured a gap.
    #[test]
    fn a_latency_queue_that_wakes_another_and_yields_keeps_its_preempt_timer() {
        const SLICES: usize = 4;
        const BOUND: Duration = Duration::from_millis(250);
        const LIMIT: Duration = Duration::from_secs(10);
        const TRIES: usize = 3;

        let run = || {
            let ex = LocalExecutorBuilder::default()
                .preempt_timer(Duration::from_secs(1))
                .make()
                .unwrap();
            ex.run(async {
                let q = crate::executor().create_task_queue(
                    Shares::Static(1),
                    Latency::Matters(Duration::from_millis(5)),
                    "latency",
                );
                let n = crate::executor().create_task_queue(
                    Shares::default(),
                    Latency::NotImportant,
                    "spinner",
                );
                crate::spawn_local_into(
                    async move {
                        let slices = Rc::new(Cell::new(0));
                        let spinner = crate::spawn_local_into(
                        enclose! { (slices) async move {
                            let start = Instant::now();
                            while slices.get() < SLICES {
                                assert!(start.elapsed() < LIMIT, "the spinner was never preempted");
                                if crate::executor().need_preempt() {
                                    slices.set(slices.get() + 1);
                                    crate::yield_if_needed().await;
                                }
                            }
                        }},
                        n,
                    )
                    .unwrap();
                        let mut worst = Duration::ZERO;
                        let mut measured = 0usize;
                        let mut last = Instant::now();
                        while slices.get() < SLICES {
                            let before = slices.get();
                            crate::executor().yield_task_queue_now().await;
                            let now = Instant::now();
                            if before >= 1 {
                                worst = worst.max(now - last);
                                measured += 1;
                            }
                            last = now;
                        }
                        spinner.await;
                        (worst, measured)
                    },
                    q,
                )
                .unwrap()
                .await
            })
        };
        let (worst, _) = (0..TRIES)
            .map(|_| run())
            .find(|&(_, measured)| measured > 0)
            .expect("no run measured a gap between the latency queue's polls");
        assert!(
            worst < BOUND,
            "the latency queue waited {worst:?} for the CPU, its promise is 5ms"
        );
    }

    /// The preempt timer is the tightest latency of the queues that exist,
    /// whether they have work or not, and no longer than the executor's own
    /// timer. Removing a queue gives its latency back only when no other
    /// queue asks for the same.
    #[test]
    fn the_preempt_timer_follows_the_tightest_latency_queue_that_exists() {
        const DEFAULT: Duration = Duration::from_millis(100);
        let ms = Duration::from_millis;
        let ex = LocalExecutorBuilder::default()
            .preempt_timer(DEFAULT)
            .make()
            .unwrap();
        let create = |d| ex.create_task_queue(Shares::default(), Latency::Matters(d), "latency");
        assert_eq!(ex.preempt_timer_duration(), DEFAULT);

        let five = create(ms(5));
        assert_eq!(ex.preempt_timer_duration(), ms(5));
        let loose = create(ms(1000));
        assert_eq!(ex.preempt_timer_duration(), ms(5));
        let one = create(ms(1));
        let other_one = create(ms(1));
        let plain = ex.create_task_queue(Shares::default(), Latency::NotImportant, "plain");
        assert_eq!(ex.preempt_timer_duration(), ms(1));

        ex.remove_task_queue(one).unwrap();
        assert_eq!(
            ex.preempt_timer_duration(),
            ms(1),
            "another 1ms queue is left"
        );
        ex.remove_task_queue(plain).unwrap();
        assert_eq!(ex.preempt_timer_duration(), ms(1));
        ex.remove_task_queue(other_one).unwrap();
        assert_eq!(ex.preempt_timer_duration(), ms(5));
        ex.remove_task_queue(five).unwrap();
        assert_eq!(ex.preempt_timer_duration(), DEFAULT);
        ex.remove_task_queue(loose).unwrap();
        assert_eq!(ex.preempt_timer_duration(), DEFAULT);
    }

    /// CPU time the calling thread has used.
    fn thread_cpu_time() -> Duration {
        nix::time::clock_gettime(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID)
            .expect("the thread CPU clock cannot fail")
            .into()
    }

    /// A `Latency::Matters(1ms)` queue whose task sleeps is woken within a few
    /// milliseconds of its deadline, though the deadline comes during the
    /// slice of a CPU-heavy queue and the executor runs under the default
    /// 100 ms preempt timer.
    ///
    /// Timers are noticed only in a reactor pass, and the spinner gives up the
    /// CPU only when `need_preempt` says so. A sleeping latency queue used not
    /// to count when the preempt timer was armed, so its deadline went
    /// unnoticed until the spinner's 100 ms slice ran out.
    ///
    /// Each nap is measured in this thread's CPU time, from the sleep to the
    /// wake. A loaded machine that takes the CPU away makes that smaller,
    /// never larger, so the bound can be a few times the promise without
    /// being flaky. A nap during which the spinner did not run measures
    /// nothing, so the test fails on one rather than passing for want of
    /// competition.
    #[test]
    fn a_sleeping_latency_queue_is_woken_on_time_beside_a_cpu_heavy_queue() {
        const NAP: Duration = Duration::from_millis(5);
        const SLACK: Duration = Duration::from_millis(5);
        const NAPS: usize = 8;

        let ex = LocalExecutorBuilder::default().make().unwrap();
        let (naps, unopposed) = ex.run(async {
            let latency = crate::executor().create_task_queue(
                Shares::default(),
                Latency::Matters(Duration::from_millis(1)),
                "latency",
            );
            let cpu = crate::executor().create_task_queue(
                Shares::default(),
                Latency::NotImportant,
                "spinner",
            );
            let done = Rc::new(Cell::new(false));
            let spins = Rc::new(Cell::new(0u64));
            let spinner = crate::spawn_local_into(
                enclose! { (done, spins) async move {
                    while !done.get() {
                        spins.set(spins.get() + 1);
                        if crate::executor().need_preempt() {
                            crate::yield_if_needed().await;
                        }
                    }
                }},
                cpu,
            )
            .unwrap();
            let sleeper = crate::spawn_local_into(
                enclose! { (spins) async move {
                    let mut naps = Vec::with_capacity(NAPS);
                    let mut unopposed = 0;
                    for _ in 0..NAPS {
                        let (before, spun) = (thread_cpu_time(), spins.get());
                        sleep(NAP).await;
                        naps.push(thread_cpu_time().saturating_sub(before));
                        if spins.get() == spun {
                            unopposed += 1;
                        }
                    }
                    (naps, unopposed)
                }},
                latency,
            )
            .unwrap()
            .await;
            done.set(true);
            spinner.await;
            sleeper
        });
        assert_eq!(unopposed, 0, "the spinner did not run during a nap");
        let worst = naps.iter().copied().max().unwrap();
        assert!(
            worst <= NAP + SLACK,
            "a {NAP:?} nap took up to {worst:?} of CPU time to be noticed, \
             against {:?} allowed: {naps:?}",
            NAP + SLACK
        );
    }

    /// How the busy task in the preemption order tests ends a slice.
    #[derive(Clone, Copy, Debug)]
    enum SliceEnd {
        /// Awaits a nop, which completes as it is submitted.
        Nop,
        /// Calls `yield_if_needed`, which requeues the task as it yields.
        Yield,
        /// Spawns a task, which is polled at once and wakes itself, and then
        /// yields: the slice was spent before the spawn.
        SpawnThenYield,
        /// Yields, having done its work in tasks it spawned, each polled at
        /// once and spinning for 200 us before it finishes, so that the
        /// preempt timer fires during one of them.
        YieldAfterInlineWork,
        /// Parks on a timer registered as the slice began, which is due by the
        /// time it ends. A timer already due when awaited completes at once,
        /// without a reactor pass.
        DueTimer,
    }

    /// What the woken task in the preemption order tests waits on.
    #[derive(Clone, Copy, Debug)]
    enum Wake {
        /// A 1 ms timer, which comes due during the busy task's slice.
        Timer,
        /// A nop, which completes in the first poll after the slice.
        Nop,
    }

    /// Awaits a nop through the ring.
    async fn nop() {
        crate::executor()
            .reactor()
            .nop()
            .collect_rw()
            .await
            .expect("a nop cannot fail");
    }

    /// Registers a timer that wakes the current task, so that a reactor pass
    /// fires it rather than the next poll completing it, however long the
    /// thread is away before it is polled.
    async fn registered_timer() -> Timer {
        let waker = poll_fn(|cx| Poll::Ready(cx.waker().clone())).await;
        timer_waking(&waker, Duration::from_micros(100))
    }

    /// Returns pending once without arranging a wake, so the task runs again
    /// only when something it registered with earlier wakes it.
    async fn park_once() {
        let mut parked = false;
        poll_fn(|_| {
            if parked {
                Poll::Ready(())
            } else {
                parked = true;
                Poll::Pending
            }
        })
        .await
    }

    thread_local! {
        /// The timer [`SpawnOnWake`] keeps armed, while one is wanted.
        static SPAWNING: RefCell<Option<Timer>> = const { RefCell::new(None) };
    }

    /// A waker that spawns a task, which is polled inline at once and wakes
    /// itself, so that it is still pending when that poll returns, and arms a
    /// timer to be woken again in a millisecond.
    ///
    /// The reactor wakes timers before it reaps the preempt timer's
    /// completion, so in the pass after a preempted slice the inline poll
    /// returns with `need_preempt` still set.
    struct SpawnOnWake;

    impl std::task::Wake for SpawnOnWake {
        fn wake(self: std::sync::Arc<Self>) {
            if SPAWNING.with(|spawning| spawning.borrow().is_none()) {
                return;
            }
            crate::spawn_local(async { crate::executor().yield_now().await }).detach();
            arm_spawning_timer(self);
        }
    }

    /// Arms a 1 ms timer that wakes `waker`.
    fn arm_spawning_timer(waker: std::sync::Arc<SpawnOnWake>) {
        let timer = timer_waking(&Waker::from(waker), Duration::from_millis(1));
        let previous = SPAWNING.with(|spawning| spawning.borrow_mut().replace(timer));
        drop(previous);
    }

    /// A timer that wakes `waker` after `after`, registered with the reactor
    /// however long the thread is away between creating and polling it.
    ///
    /// A timer polled once its deadline has passed completes without
    /// registering its waker, so this one is polled with no deadline, which
    /// parks the waker, and then reset, which registers the parked waker
    /// whatever the deadline.
    fn timer_waking(waker: &Waker, after: Duration) -> Timer {
        let mut timer = Timer::new(Duration::MAX);
        assert!(Pin::new(&mut timer)
            .poll(&mut Context::from_waker(waker))
            .is_pending());
        timer.reset(after);
        timer
    }

    /// Asserts that a task woken during another task's preempted slice runs
    /// before that task's next slice.
    ///
    /// `busy` spins until the preempt timer fires and then ends its slice as
    /// `end` says. `woken` waits on `wake` in a loop. Each wait completes
    /// during, or right after, one of `busy`'s slices, so `woken` should run
    /// when that slice ends. If `busy` is queued ahead of it instead, `busy`
    /// takes another whole slice first.
    ///
    /// With `queues` set to 2 the tasks run in two queues of equal shares,
    /// otherwise both run in the default queue.
    ///
    /// `busy` counts the slices it ends and `woken` records the most that
    /// ended between two of its polls, which must be one. Counting slices
    /// rather than timing the gap keeps a thread the system descheduled from
    /// reading as a second slice; the longest gap is reported alongside, and
    /// it is two preempt periods when the order is wrong. A slice counts only
    /// if `busy` spun for half a period or more: the preempt timer keeps its
    /// own period rather than restarting with each round, so it can fire while
    /// `woken` runs and end a slice of `busy` that has barely begun, and a
    /// slice through which the thread was descheduled does not count either.
    /// `woken` measures on every iteration, so the bound cannot hold
    /// vacuously, and goes on until `busy` has ended as many counted slices
    /// as there are gaps, measuring at most four times as many gaps, so a run
    /// in which `busy` never spun, and `woken` was never kept waiting, fails
    /// as well.
    fn assert_a_woken_task_waits_one_slice(end: SliceEnd, wake: Wake, queues: usize) {
        assert_a_woken_task_waits_one_slice_with(end, wake, queues, false)
    }

    /// As [`assert_a_woken_task_waits_one_slice`], and with `spawning`, a
    /// waker run by every reactor pass spawns a task that is polled inline
    /// while `need_preempt` is still set after a preempted slice.
    fn assert_a_woken_task_waits_one_slice_with(
        end: SliceEnd,
        wake: Wake,
        queues: usize,
        spawning: bool,
    ) {
        const PERIOD: Duration = Duration::from_millis(20);
        const GAPS: usize = 10;
        const LIMIT: Duration = Duration::from_secs(10);

        let ex = LocalExecutorBuilder::default()
            .preempt_timer(PERIOD)
            .make()
            .unwrap();
        let (most, worst, measured, slices) = ex.run(async move {
            let (busy_queue, woken_queue) = match queues {
                1 => (TaskQueueHandle::default(), TaskQueueHandle::default()),
                2 => (
                    crate::executor().create_task_queue(
                        Shares::default(),
                        Latency::NotImportant,
                        "busy",
                    ),
                    crate::executor().create_task_queue(
                        Shares::default(),
                        Latency::NotImportant,
                        "woken",
                    ),
                ),
                _ => unreachable!("the tests run one queue or two"),
            };
            let done = Rc::new(Cell::new(false));
            let slices = Rc::new(Cell::new(0usize));
            let busy = crate::spawn_local_into(
                enclose! { (done, slices) async move {
                    let start = Instant::now();
                    let mut spun = Instant::now();
                    let mut _due = match end {
                        SliceEnd::DueTimer => Some(registered_timer().await),
                        _ => None,
                    };
                    while !done.get() {
                        assert!(start.elapsed() < LIMIT, "the busy task was never preempted");
                        if let SliceEnd::YieldAfterInlineWork = end {
                            crate::spawn_local(async {
                                let work = Instant::now();
                                while work.elapsed() < Duration::from_micros(200) {}
                            })
                            .detach();
                        }
                        if crate::executor().need_preempt() {
                            if spun.elapsed() >= PERIOD / 2 {
                                slices.set(slices.get() + 1);
                            }
                            match end {
                                SliceEnd::Nop => nop().await,
                                SliceEnd::Yield => crate::yield_if_needed().await,
                                SliceEnd::YieldAfterInlineWork => crate::yield_if_needed().await,
                                SliceEnd::SpawnThenYield => {
                                    crate::spawn_local(async {
                                        crate::executor().yield_now().await;
                                    })
                                    .detach();
                                    crate::yield_if_needed().await;
                                }
                                SliceEnd::DueTimer => {
                                    park_once().await;
                                    _due = Some(registered_timer().await);
                                }
                            }
                            spun = Instant::now();
                        }
                    }
                }},
                busy_queue,
            )
            .unwrap();
            let woken = crate::spawn_local_into(
                enclose! { (slices) async move {
                    let wait = || async move {
                        match wake {
                            Wake::Timer => {
                                Timer::new(Duration::from_millis(1)).await;
                            }
                            Wake::Nop => nop().await,
                        }
                    };
                    wait().await;
                    let mut most = 0usize;
                    let mut worst = Duration::ZERO;
                    let mut measured = 0usize;
                    let mut last = (slices.get(), Instant::now());
                    while (measured < GAPS || slices.get() < GAPS) && measured < GAPS * 4 {
                        wait().await;
                        let now = (slices.get(), Instant::now());
                        most = most.max(now.0 - last.0);
                        worst = worst.max(now.1 - last.1);
                        measured += 1;
                        last = now;
                    }
                    done.set(true);
                    (most, worst, measured)
                }},
                woken_queue,
            )
            .unwrap();
            if spawning {
                arm_spawning_timer(std::sync::Arc::new(SpawnOnWake));
            }
            let (most, worst, measured) = woken.await;
            let armed = SPAWNING.with(|spawning| spawning.borrow_mut().take());
            drop(armed);
            busy.await;
            (most, worst, measured, slices.get())
        });
        assert!(
            measured >= GAPS,
            "the woken task measured {measured} of {GAPS} gaps"
        );
        assert!(
            slices >= GAPS,
            "the busy task ran {slices} slices for {GAPS} gaps, so it never held the woken task back"
        );
        assert!(
            most <= 1,
            "with the busy task ending its slices on {end:?} and the woken task waiting on \
             {wake:?} in {queues} queue(s), the busy task ran {most} slices in a row, and the \
             woken task waited up to {worst:?} for the CPU with a {PERIOD:?} preempt timer"
        );
    }

    /// The executor forgets the preempted task when `run` returns, and
    /// queues it if it was held.
    ///
    /// A task spawned after `run` returns can be allocated where a task that
    /// finished in the last slice was, so a record kept past `run` could take
    /// it for that task. The first `run` ends with the slice its own future
    /// spent until preempted; the second ends with one `busy` spent, which
    /// is held as it yields.
    #[test]
    fn run_forgets_the_preempted_task_when_it_returns() {
        let ex = LocalExecutorBuilder::default()
            .preempt_timer(Duration::from_millis(5))
            .make()
            .unwrap();
        ex.run(async { while !crate::executor().need_preempt() {} });
        assert!(
            ex.context.queues.borrow().preempted.is_none(),
            "the task that spent the last slice is still remembered"
        );

        let (ex, _, _) = run_ending_with_a_held_task();
        assert!(
            ex.context.queues.borrow().preempted.is_none(),
            "the held task is still remembered"
        );
        let queue = ex.context.get_queue(&TaskQueueHandle::default()).unwrap();
        assert_eq!(
            queue.borrow().runnables.len(),
            1,
            "the held task was not queued"
        );
    }

    /// A preempted task that wakes itself and then another task is queued
    /// after that task.
    ///
    /// A task woken while it runs is queued when its poll returns, after
    /// anything it woke. So a task that yielded is always the last runnable
    /// pushed before its slice is accounted, and looking at the back of its
    /// queue finds it whenever it is there: holding it back needs no scan and
    /// has no case to fall back from. This test fails if that stops being
    /// true. `busy` spins until preempted, wakes itself and then `woken`, and
    /// notes how many slices it has ended; `woken` runs before its next one.
    #[test]
    fn a_preempted_task_is_queued_after_a_task_it_woke_after_itself() {
        const WAKES: usize = 5;
        const LIMIT: Duration = Duration::from_secs(10);

        let ex = LocalExecutorBuilder::default()
            .preempt_timer(Duration::from_millis(10))
            .make()
            .unwrap();
        let lags = ex.run(async {
            let slices = Rc::new(Cell::new(0usize));
            let woke_at = Rc::new(Cell::new(0usize));
            let parked: Rc<RefCell<Option<Waker>>> = Rc::new(RefCell::new(None));
            let done = Rc::new(Cell::new(false));
            let busy = crate::spawn_local_into(
                enclose! { (slices, woke_at, parked, done) async move {
                    let start = Instant::now();
                    while !done.get() {
                        assert!(start.elapsed() < LIMIT, "the busy task was never preempted");
                        if crate::executor().need_preempt() {
                            slices.set(slices.get() + 1);
                            let mut yielded = false;
                            poll_fn(|cx| {
                                if yielded {
                                    return Poll::Ready(());
                                }
                                yielded = true;
                                cx.waker().wake_by_ref();
                                if let Some(waker) = parked.borrow_mut().take() {
                                    woke_at.set(slices.get());
                                    waker.wake();
                                }
                                Poll::Pending
                            })
                            .await;
                        }
                    }
                }},
                TaskQueueHandle::default(),
            )
            .unwrap();
            let mut lags = Vec::new();
            for _ in 0..WAKES {
                let mut waiting = false;
                poll_fn(|cx| {
                    if waiting {
                        return Poll::Ready(());
                    }
                    waiting = true;
                    *parked.borrow_mut() = Some(cx.waker().clone());
                    Poll::Pending
                })
                .await;
                lags.push(slices.get() - woke_at.get());
            }
            done.set(true);
            busy.await;
            lags
        });
        assert_eq!(
            lags, [0; WAKES],
            "slices the busy task ended before the task it woke ran"
        );
    }

    /// The busy task for the tests of a task held when `run` returns.
    ///
    /// It spins until preempted and yields, forever, counting its slices, and
    /// counts in `dropped` when its future is dropped.
    fn spawn_held_task(slices: Rc<Cell<usize>>, dropped: Rc<Cell<usize>>) {
        struct Dropped(Rc<Cell<usize>>);

        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        crate::spawn_local_into(
            async move {
                let _dropped = Dropped(dropped);
                loop {
                    if crate::executor().need_preempt() {
                        slices.set(slices.get() + 1);
                        crate::yield_if_needed().await;
                    }
                }
            },
            TaskQueueHandle::default(),
        )
        .unwrap()
        .detach();
    }

    /// An executor whose last `run` ended with `busy`, from `spawn_held_task`,
    /// held, and `busy`'s slice and drop counts.
    ///
    /// `run`'s future spawns `busy` and returns, `busy` spends the rest of the
    /// round, and its slice ends in preemption with the task the only one in
    /// its queue, so it is held. If the preempt timer fires before `busy` is
    /// first polled, the round ends without it, so a run in which `busy`
    /// ended no slice is made again on a new executor.
    fn run_ending_with_a_held_task() -> (LocalExecutor, Rc<Cell<usize>>, Rc<Cell<usize>>) {
        for _ in 0..10 {
            let slices = Rc::new(Cell::new(0usize));
            let dropped = Rc::new(Cell::new(0usize));
            let ex = LocalExecutorBuilder::default()
                .preempt_timer(Duration::from_millis(5))
                .make()
                .unwrap();
            ex.run(enclose! { (slices, dropped) async move {
                spawn_held_task(slices, dropped);
            }});
            if slices.get() == 1 {
                return (ex, slices, dropped);
            }
        }
        panic!("the busy task never ended a slice before a run returned");
    }

    /// A task held back when `run` returns is queued again, so the next `run`
    /// runs it.
    ///
    /// The first `run`'s future spawns `busy` and returns, `busy` spends the
    /// rest of the round, and its slice ends in preemption with the task the
    /// only one in its queue, so it is held. Then `run` returns.
    #[test]
    fn a_task_held_when_run_returns_runs_in_the_next_run() {
        const LIMIT: Duration = Duration::from_secs(10);

        let (ex, slices, dropped) = run_ending_with_a_held_task();
        ex.run(enclose! { (slices) async move {
            let start = Instant::now();
            while slices.get() < 3 && start.elapsed() < LIMIT {
                Timer::new(Duration::from_millis(1)).await;
            }
        }});
        assert!(
            slices.get() >= 3,
            "the held task ran {} slices in all",
            slices.get()
        );
        drop(ex);
        assert_eq!(
            dropped.get(),
            1,
            "the held task was dropped {} times",
            dropped.get()
        );
    }

    /// A task held back when `run` returns is dropped with the executor, once.
    #[test]
    fn a_task_held_when_run_returns_is_dropped_with_the_executor() {
        let (ex, _, dropped) = run_ending_with_a_held_task();
        drop(ex);
        assert_eq!(
            dropped.get(),
            1,
            "the held task was dropped {} times",
            dropped.get()
        );
    }

    /// A preempted task that is the only one runnable is not lost while it is
    /// held, whether the executor finds its wake before going idle or parks
    /// straight after the slice.
    ///
    /// `busy` spins until preempted and then waits on a nop, which completes
    /// in the first poll after the slice, or on a 20 ms timer, which the
    /// executor parks for. The executor runs on its own thread so that a lost
    /// wake fails the test rather than hanging it.
    #[test]
    fn a_lone_preempted_task_is_not_lost() {
        const SLICES: usize = 5;

        for wake in [Wake::Nop, Wake::Timer] {
            let (sender, receiver) = std::sync::mpsc::channel();
            let _thread = LocalExecutorBuilder::default()
                .preempt_timer(Duration::from_millis(5))
                .spawn(move || async move {
                    for _ in 0..SLICES {
                        while !crate::executor().need_preempt() {}
                        match wake {
                            Wake::Nop => nop().await,
                            Wake::Timer => {
                                Timer::new(Duration::from_millis(20)).await;
                            }
                        }
                    }
                    sender.send(()).unwrap();
                })
                .unwrap();
            assert!(
                receiver.recv_timeout(Duration::from_secs(10)).is_ok(),
                "a task waiting on {wake:?} after its slice was never run again"
            );
        }
    }

    thread_local! {
        /// The wakers `parked_until_woken` leaves, by slot, for
        /// [`WakeParked`] to wake in order.
        static PARKED: RefCell<Vec<Option<Waker>>> = const { RefCell::new(Vec::new()) };
    }

    /// A waker that wakes every task parked with `parked_until_woken`, in slot
    /// order, from wherever it is itself woken.
    struct WakeParked;

    impl std::task::Wake for WakeParked {
        fn wake(self: std::sync::Arc<Self>) {
            let wakers: Vec<Waker> = PARKED.with(|parked| {
                parked
                    .borrow_mut()
                    .iter_mut()
                    .filter_map(Option::take)
                    .collect()
            });
            for waker in wakers {
                waker.wake();
            }
        }
    }

    /// Leaves this task's waker in `slot` and waits until it is woken.
    async fn parked_until_woken(slot: usize) {
        let mut parked = false;
        poll_fn(|cx| {
            if parked {
                Poll::Ready(())
            } else {
                parked = true;
                PARKED.with(|wakers| wakers.borrow_mut()[slot] = Some(cx.waker().clone()));
                Poll::Pending
            }
        })
        .await
    }

    thread_local! {
        /// The timer [`BEFORE_PARK`] arms in the park order test.
        static PARK_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    }

    /// The window a preempted slice opens ends before the executor parks, so
    /// the tasks woken after a park run in the order they were woken.
    ///
    /// `other` parks, then `preempted` spins until preempted and parks too.
    /// Nothing is runnable then, and just before the executor parks a hook
    /// arms a 1 ms timer polled with [`WakeParked`], which wakes `preempted`
    /// and then `other` in the pass after the park. Were `preempted` still
    /// remembered, it would be held behind `other`. The executor runs on its
    /// own thread so that a lost wake fails the test rather than hanging it.
    #[test]
    fn tasks_woken_after_a_park_run_in_the_order_they_were_woken() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let _thread = LocalExecutorBuilder::default()
            .preempt_timer(Duration::from_millis(5))
            .spawn(move || async move {
                PARKED.with(|parked| *parked.borrow_mut() = vec![None, None]);
                BEFORE_PARK.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(|| {
                        let waker = Waker::from(std::sync::Arc::new(WakeParked));
                        let timer = timer_waking(&waker, Duration::from_millis(1));
                        PARK_TIMER.with(|armed| *armed.borrow_mut() = Some(timer));
                    }));
                });
                let order = Rc::new(RefCell::new(Vec::new()));
                let other = crate::spawn_local_into(
                    enclose! { (order) async move {
                        parked_until_woken(1).await;
                        order.borrow_mut().push("other");
                    }},
                    TaskQueueHandle::default(),
                )
                .unwrap();
                let preempted = crate::spawn_local_into(
                    enclose! { (order) async move {
                        while !crate::executor().need_preempt() {}
                        parked_until_woken(0).await;
                        order.borrow_mut().push("preempted");
                    }},
                    TaskQueueHandle::default(),
                )
                .unwrap();
                preempted.await;
                other.await;
                let order = order.borrow().clone();
                sender.send(order).unwrap();
            })
            .unwrap();
        let order = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("a task woken after the park never ran");
        assert_eq!(order, ["preempted", "other"]);
    }

    /// A window that remembers the task at `task` as the inline one, holding
    /// nothing yet.
    fn window(task: usize) -> Preempted {
        let mut window = Preempted::new(0, usize::MAX);
        window.inline.push(Held {
            task,
            runnable: None,
        });
        window
    }

    /// A runnable for a task in `queue` that nothing has scheduled, its
    /// address, and a flag its future sets when it is dropped.
    fn unscheduled_in(
        ex: &LocalExecutor,
        queue: Rc<RefCell<TaskQueue>>,
    ) -> (multitask::Runnable, usize, Rc<Cell<bool>>) {
        struct Dropped(Rc<Cell<bool>>);

        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }

        let dropped = Rc::new(Cell::new(false));
        let guard = Dropped(dropped.clone());
        let scheduler = queue.borrow().scheduler.clone();
        let (runnable, handle) =
            scheduler.spawn_unscheduled(ex.task_registry(), queue, async move {
                drop(guard);
            });
        drop(handle);
        let address = runnable.raw_task.as_ptr().addr();
        (runnable, address, dropped)
    }

    /// A runnable for a task in the default queue that nothing has scheduled,
    /// its address, and a flag its future sets when it is dropped.
    fn unscheduled(ex: &LocalExecutor) -> (multitask::Runnable, usize, Rc<Cell<bool>>) {
        let queue = ex.context.get_queue(&TaskQueueHandle::default()).unwrap();
        unscheduled_in(ex, queue)
    }

    /// The addresses of the tasks queued in `queue`, in order.
    fn queued(queue: &Rc<RefCell<TaskQueue>>) -> Vec<usize> {
        queue
            .borrow()
            .runnables
            .iter()
            .map(|runnable| runnable.raw_task.as_ptr().addr())
            .collect()
    }

    /// Holding a runnable activates its queue, so an executor with nothing
    /// else to run does not go idle while it is held, and the end of the
    /// window queues it.
    #[test]
    fn holding_a_task_activates_its_queue() {
        let ex = LocalExecutor::default();
        let queue = ex.context.get_queue(&TaskQueueHandle::default()).unwrap();
        let (runnable, address, _) = unscheduled(&ex);
        assert!(!queue.borrow().is_active());
        {
            let mut queues = ex.context.queues.borrow_mut();
            queues.preempted = Some(window(address));
            assert!(matches!(queues.hold(&queue, runnable), Ok(None)));
        }
        assert!(queue.borrow().is_active());
        assert!(queue.borrow().runnables.is_empty());
        ex.context.end_preempted_window();
        assert_eq!(queue.borrow().runnables.len(), 1);
    }

    /// A queue whose only runnable is held at the end of its slice stays
    /// runnable, so it keeps the vruntime it was charged rather than being
    /// placed afresh when the runnable is queued again. The run ends on
    /// `need_preempt`, as one in which a window opened always does.
    #[test]
    fn a_queue_whose_only_task_is_held_stays_runnable() {
        let ex = LocalExecutor::default();
        let queue = ex.context.get_queue(&TaskQueueHandle::default()).unwrap();
        let (runnable, address, _) = unscheduled(&ex);
        {
            let mut queues = ex.context.queues.borrow_mut();
            queues.preempted = Some(window(address));
            assert!(matches!(queues.hold(&queue, runnable), Ok(None)));
            queues.active_executors.clear();
            queues.active_executing = Some(queue.clone());
        }
        ex.finish_task_queue_run(
            queue.clone(),
            Duration::from_millis(1),
            1,
            Some((Some(1), 0)),
        );
        assert!(queue.borrow().is_active(), "the queue went idle");
        assert_eq!(
            ex.context.queues.borrow().active_executors.len(),
            1,
            "the queue was not returned to the scheduler"
        );
        ex.context.end_preempted_window();
        assert_eq!(queue.borrow().runnables.len(), 1);
    }

    /// A queue whose only runnable is held stays runnable when its run unwinds
    /// before the run loop sees the preempt timer.
    ///
    /// A task polled inline during whose poll the timer fired is held as the
    /// poll returns, and its spawner can then panic, so the run ends with no
    /// task recorded as the one that spent it.
    #[test]
    fn a_queue_whose_only_task_is_held_stays_runnable_when_its_run_unwinds() {
        struct FinishOnDrop<'a>(&'a LocalExecutor, Rc<RefCell<TaskQueue>>);

        impl Drop for FinishOnDrop<'_> {
            fn drop(&mut self) {
                self.0
                    .finish_task_queue_run(self.1.clone(), Duration::from_millis(1), 1, None);
            }
        }

        let ex = LocalExecutor::default();
        let queue = ex.context.get_queue(&TaskQueueHandle::default()).unwrap();
        let (runnable, address, _) = unscheduled(&ex);
        {
            let mut queues = ex.context.queues.borrow_mut();
            queues.preempted = Some(window(address));
            assert!(matches!(queues.hold(&queue, runnable), Ok(None)));
            queues.active_executors.clear();
            queues.active_executing = Some(queue.clone());
        }
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _finish = FinishOnDrop(&ex, queue.clone());
            panic!("the run unwinds");
        }));
        assert!(unwound.is_err());
        assert!(queue.borrow().is_active(), "the queue went idle");
        assert_eq!(
            ex.context.queues.borrow().active_executors.len(),
            1,
            "the queue was not returned to the scheduler"
        );
        ex.context.end_preempted_window();
        assert_eq!(queue.borrow().runnables.len(), 1);
    }

    /// A runnable held for another queue does not keep a finishing queue with
    /// nothing to run in the scheduler.
    #[test]
    fn a_task_held_for_another_queue_does_not_keep_the_finishing_one_runnable() {
        let ex = LocalExecutor::default();
        let queue = ex.context.get_queue(&TaskQueueHandle::default()).unwrap();
        let other = ex
            .context
            .create_task_queue(Shares::default(), Latency::NotImportant, "other");
        let other = ex.context.get_queue(&other).unwrap();
        let (runnable, address, _) = unscheduled_in(&ex, other.clone());
        {
            let mut queues = ex.context.queues.borrow_mut();
            queues.preempted = Some(window(address));
            assert!(matches!(queues.hold(&other, runnable), Ok(None)));
            queues.active_executing = Some(queue.clone());
        }
        queue.borrow_mut().active = true;
        ex.finish_task_queue_run(
            queue.clone(),
            Duration::from_millis(1),
            1,
            Some((Some(1), 0)),
        );
        assert!(
            !queue.borrow().is_active(),
            "the finishing queue was kept runnable for another queue's task"
        );
        ex.context.end_preempted_window();
        assert_eq!(queued(&other), [address]);
    }

    /// The end of a window queues the tasks it held in the order they were
    /// queued: the tasks polled inline first, innermost first, then the task
    /// the run loop polled last.
    #[test]
    fn held_tasks_are_queued_in_the_order_they_were_queued() {
        let ex = LocalExecutor::default();
        let queue = ex.context.get_queue(&TaskQueueHandle::default()).unwrap();
        let (inner, inner_address, _) = unscheduled(&ex);
        let (outer, outer_address, _) = unscheduled(&ex);
        let (last, last_address, _) = unscheduled(&ex);
        {
            let mut queues = ex.context.queues.borrow_mut();
            let mut window = window(inner_address);
            window.inline.push(Held {
                task: outer_address,
                runnable: None,
            });
            window.last = Some(Held {
                task: last_address,
                runnable: None,
            });
            queues.preempted = Some(window);
            assert!(matches!(queues.hold(&queue, last), Ok(None)));
            assert!(matches!(queues.hold(&queue, outer), Ok(None)));
            assert!(matches!(queues.hold(&queue, inner), Ok(None)));
        }
        ex.context.end_preempted_window();
        assert_eq!(queued(&queue), [inner_address, outer_address, last_address]);
    }

    /// A runnable held for the last task is queued, not dropped, if another
    /// takes its slot.
    ///
    /// A window ends before the next round, so a second slice end cannot
    /// reach the same window, and a task is queued at most once, so a hold
    /// cannot meet its own runnable. Dropping a runnable runs user code,
    /// though, and here the queues are borrowed, so the displaced one goes
    /// back to its queue once they are not.
    #[test]
    fn a_held_task_displaced_from_the_slot_is_queued_not_dropped() {
        let ex = LocalExecutor::default();
        let queue = ex.context.get_queue(&TaskQueueHandle::default()).unwrap();
        let (first, first_address, first_dropped) = unscheduled(&ex);
        let (second, second_address, _) = unscheduled(&ex);
        queue.borrow_mut().runnables.push_back(first);
        let displaced =
            ex.context
                .queues
                .borrow_mut()
                .preempted_last(&queue, Some(first_address), 0);
        assert!(displaced.is_none());
        queue.borrow_mut().runnables.push_back(second);
        let displaced = ex
            .context
            .queues
            .borrow_mut()
            .preempted_last(&queue, Some(second_address), 0)
            .expect("the first task was not displaced");
        ex.context.requeue(displaced);
        assert_eq!(queued(&queue), [first_address]);
        assert!(!first_dropped.get(), "the displaced task was dropped");
        ex.context.end_preempted_window();
        assert_eq!(queued(&queue), [first_address, second_address]);
    }

    rusty_fork_test! {
        /// A held runnable whose queue is gone is cancelled with no borrow
        /// held: cancelling wakes the task awaiting it, whose schedule
        /// borrows the queues, and under a borrow the process aborts. So
        /// this runs in a process of its own.
        #[test]
        fn a_held_task_whose_queue_is_gone_is_cancelled_with_no_borrow_held() {
            let ex = LocalExecutor::default();
            let context = &ex.context;
            ex.run(async {
                let handle = crate::executor().create_task_queue(
                    Shares::default(),
                    Latency::NotImportant,
                    "gone",
                );
                let queue = context.get_queue(&handle).unwrap();
                let scheduler = queue.borrow().scheduler.clone();
                let (runnable, join) =
                    scheduler.spawn_unscheduled(&context.tasks, queue.clone(), async {});
                drop(queue);
                let awaiter = crate::spawn_local(join);
                crate::executor().remove_task_queue(handle).unwrap();
                context.requeue(runnable);
                assert!(awaiter.await.is_none(), "the task was not cancelled");
            });
        }
    }

    /// What the spawner in the inline poll tests does after the spawn.
    #[derive(Clone, Copy, Debug)]
    enum AfterInline {
        /// Goes on to await the spawned task.
        Awaits,
        /// Yields first, since the preempt timer has fired, so that both
        /// tasks are queued when the slice ends.
        Yields,
        /// Spawns a second task, which wakes itself once and so is queued
        /// behind the first.
        SpawnsAnother,
    }

    /// Asserts that a task woken during a slice spent in the inline first poll
    /// of a spawned task runs before that task's next slice.
    ///
    /// `spawner` spawns `busy` with `spawn_local`, which polls it inside
    /// `spawner`'s own poll. `busy` spins until preempted, yields, notes that it
    /// runs again, spins a second slice and finishes. Before `busy`, `spawner`
    /// spawns `woken`, which waits on a 1 ms timer that comes due during
    /// `busy`'s first slice and then notes whether `busy` has run again: it
    /// must not have. After the spawn `spawner` does what `after` says.
    ///
    /// A slice counts only if `busy` spun for half a period or more, and each
    /// round is measured only if its first slice counted, so a round in which
    /// the timer fired before `busy` began proves nothing either way; most
    /// rounds must be measured.
    fn assert_a_woken_task_runs_before_an_inline_poll_s_next_slice(after: AfterInline) {
        const PERIOD: Duration = Duration::from_millis(20);
        const ROUNDS: usize = 10;

        let ex = LocalExecutorBuilder::default()
            .preempt_timer(PERIOD)
            .make()
            .unwrap();
        let seen = ex.run(async move {
            crate::spawn_local_into(
                async move {
                    let mut seen = Vec::new();
                    for _ in 0..ROUNDS {
                        let slices = Rc::new(Cell::new(0usize));
                        let resumed = Rc::new(Cell::new(false));
                        let woken = crate::spawn_local(enclose! { (resumed) async move {
                            Timer::new(Duration::from_millis(1)).await;
                            resumed.get()
                        }});
                        let busy = crate::spawn_local(enclose! { (slices, resumed) async move {
                            for slice in 0..2 {
                                resumed.set(slice > 0);
                                let spun = Instant::now();
                                while !crate::executor().need_preempt() {}
                                if spun.elapsed() >= PERIOD / 2 {
                                    slices.set(slices.get() + 1);
                                }
                                crate::yield_if_needed().await;
                            }
                        }});
                        let measured = slices.get() == 1;
                        match after {
                            AfterInline::Awaits => {}
                            AfterInline::Yields => crate::yield_if_needed().await,
                            AfterInline::SpawnsAnother => {
                                crate::spawn_local(async {
                                    crate::executor().yield_now().await;
                                })
                                .detach();
                            }
                        }
                        let ran_after = woken.await;
                        busy.await;
                        if measured {
                            seen.push(ran_after);
                        }
                    }
                    seen
                },
                TaskQueueHandle::default(),
            )
            .unwrap()
            .await
        });
        assert!(
            seen.len() >= ROUNDS / 2,
            "only {} of {ROUNDS} rounds began with a whole slice",
            seen.len()
        );
        assert!(
            seen.iter().all(|&resumed| !resumed),
            "the inline task ran again before the woken task in these measured rounds: {seen:?}"
        );
    }

    #[test]
    fn a_due_timer_runs_ahead_of_a_task_preempted_in_its_inline_first_poll() {
        assert_a_woken_task_runs_before_an_inline_poll_s_next_slice(AfterInline::Awaits);
    }

    #[test]
    fn a_due_timer_runs_ahead_of_an_inline_task_whose_spawner_also_yielded() {
        assert_a_woken_task_runs_before_an_inline_poll_s_next_slice(AfterInline::Yields);
    }

    #[test]
    fn a_due_timer_runs_ahead_of_an_inline_task_queued_before_another_spawned_task() {
        assert_a_woken_task_runs_before_an_inline_poll_s_next_slice(AfterInline::SpawnsAnother);
    }

    /// Spins until the preempt timer fires and adds a slice to `slices` if it
    /// spun for half a period or more.
    fn spin_a_slice(slices: &Cell<usize>, period: Duration) {
        let spun = Instant::now();
        while !crate::executor().need_preempt() {}
        if spun.elapsed() >= period / 2 {
            slices.set(slices.get() + 1);
        }
    }

    /// A task at `level` of a chain of `depth` tasks, each spawning the next
    /// with `spawn_local`, so that each is polled inside the poll of the one
    /// before it.
    ///
    /// The innermost spins until preempted. Every task yields then, the
    /// innermost first as the polls return, and every one notes its level in
    /// `order` and spins a whole slice when it runs again.
    fn nested_inline(
        level: usize,
        depth: usize,
        slices: Rc<Cell<usize>>,
        order: Rc<RefCell<Vec<usize>>>,
        period: Duration,
    ) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(async move {
            if level < depth {
                let child = crate::spawn_local(nested_inline(
                    level + 1,
                    depth,
                    slices.clone(),
                    order.clone(),
                    period,
                ));
                crate::yield_if_needed().await;
                order.borrow_mut().push(level);
                spin_a_slice(&slices, period);
                child.await;
            } else {
                spin_a_slice(&slices, period);
                crate::yield_if_needed().await;
                order.borrow_mut().push(level);
                spin_a_slice(&slices, period);
            }
        })
    }

    /// Asserts that a task woken during a slice spent in the innermost of a
    /// chain of `depth` inline first polls runs before any task of the chain
    /// runs again.
    ///
    /// `spawner` spawns `woken`, which waits on a 1 ms timer, and then the
    /// first task of the chain, and yields. Every task of the chain had a poll
    /// under way when the timer fired, so every one goes behind `woken`. Each
    /// notes its level when it runs again, and `woken` notes 0 when it runs,
    /// so the order they ran in starts with 0. A round is measured only if the
    /// innermost's first slice counted, so that the timer fired during the
    /// chain's polls and `woken`'s timer was due as the slice ended.
    fn assert_a_woken_task_runs_before_a_chain_of_inline_polls(depth: usize) {
        const PERIOD: Duration = Duration::from_millis(20);
        const ROUNDS: usize = 10;

        let ex = LocalExecutorBuilder::default()
            .preempt_timer(PERIOD)
            .make()
            .unwrap();
        let seen = ex.run(async move {
            crate::spawn_local_into(
                async move {
                    let mut seen = Vec::new();
                    for _ in 0..ROUNDS {
                        let slices = Rc::new(Cell::new(0usize));
                        let order = Rc::new(RefCell::new(Vec::new()));
                        let woken = crate::spawn_local(enclose! { (order) async move {
                            Timer::new(Duration::from_millis(1)).await;
                            order.borrow_mut().push(0);
                        }});
                        let chain = crate::spawn_local(nested_inline(
                            1,
                            depth,
                            slices.clone(),
                            order.clone(),
                            PERIOD,
                        ));
                        let measured = slices.get() == 1;
                        crate::yield_if_needed().await;
                        woken.await;
                        chain.await;
                        if measured {
                            seen.push(order.take());
                        }
                    }
                    seen
                },
                TaskQueueHandle::default(),
            )
            .unwrap()
            .await
        });
        assert!(
            seen.len() >= ROUNDS / 2,
            "only {} of {ROUNDS} rounds began with a whole slice",
            seen.len()
        );
        assert!(
            seen.iter()
                .all(|order| order.first() == Some(&0) && order.len() == depth + 1),
            "with a chain of {depth} inline polls, the woken task, 0, and the tasks of the \
             chain, by level, ran again in these orders: {seen:?}"
        );
    }

    #[test]
    fn a_due_timer_runs_ahead_of_two_nested_inline_tasks() {
        assert_a_woken_task_runs_before_a_chain_of_inline_polls(2);
    }

    #[test]
    fn a_due_timer_runs_ahead_of_three_nested_inline_tasks() {
        assert_a_woken_task_runs_before_a_chain_of_inline_polls(3);
    }

    /// A task spawned outside any slice, by a destructor the executor runs
    /// between runs, opens no window even if the preempt timer fires during
    /// its first poll, and is queued as it wakes itself.
    ///
    /// Dropping a parked task between runs drops its future there and then,
    /// and the future's destructor spawns `child`, which spins until the
    /// preempt timer fires and yields. Whether the timer fires during that
    /// poll depends on when the last run armed it, so a run in which it was
    /// already pending is tried again.
    #[test]
    fn a_task_spawned_by_a_destructor_between_runs_opens_no_window() {
        struct SpawnOnDrop(Rc<Cell<bool>>);

        impl Drop for SpawnOnDrop {
            fn drop(&mut self) {
                let fired = self.0.clone();
                crate::spawn_local(async move {
                    let reactor = crate::executor().reactor();
                    let pending = reactor.need_preempt();
                    let start = Instant::now();
                    while !reactor.need_preempt() && start.elapsed() < Duration::from_millis(500) {}
                    fired.set(!pending && reactor.need_preempt());
                    crate::executor().yield_now().await;
                })
                .detach();
            }
        }

        for _ in 0..10 {
            let ex = LocalExecutorBuilder::default()
                .preempt_timer(Duration::from_millis(5))
                .make()
                .unwrap();
            ex.run(async {});
            let fired = Rc::new(Cell::new(false));
            let guard = SpawnOnDrop(fired.clone());
            let parked = ex.spawn(async move {
                let _guard = guard;
                futures::future::pending::<()>().await;
            });
            drop(parked);
            if !fired.get() {
                continue;
            }
            assert!(
                ex.context.queues.borrow().preempted.is_none(),
                "a window opened outside any slice"
            );
            let queue = ex.context.get_queue(&TaskQueueHandle::default()).unwrap();
            assert_eq!(
                queue.borrow().runnables.len(),
                1,
                "the child was not queued"
            );
            return;
        }
        panic!("the preempt timer never fired during the child's first poll");
    }

    /// A task spawned after the preempt timer fired is queued in its turn, not
    /// held back as if it had spent the slice.
    ///
    /// `busy` spins until preempted, spawns `child`, which wakes itself once,
    /// arms a timer that wakes `woken` in the next reactor pass, and yields.
    /// `woken` is woken after `child` was queued, however the thread was
    /// delayed before then, and `child` runs first.
    #[test]
    fn a_task_spawned_after_the_preempt_timer_fired_keeps_its_place() {
        let ex = LocalExecutorBuilder::default()
            .preempt_timer(Duration::from_millis(20))
            .make()
            .unwrap();
        let order = ex.run(async {
            PARKED.with(|parked| *parked.borrow_mut() = vec![None]);
            let order = Rc::new(RefCell::new(Vec::new()));
            let woken = crate::spawn_local_into(
                enclose! { (order) async move {
                    parked_until_woken(0).await;
                    order.borrow_mut().push("woken");
                }},
                TaskQueueHandle::default(),
            )
            .unwrap();
            let busy = crate::spawn_local_into(
                enclose! { (order) async move {
                    while !crate::executor().need_preempt() {}
                    crate::spawn_local(enclose! { (order) async move {
                        crate::executor().yield_now().await;
                        order.borrow_mut().push("child");
                    }})
                    .detach();
                    let waker = Waker::from(std::sync::Arc::new(WakeParked));
                    let _wakes_woken = timer_waking(&waker, Duration::ZERO);
                    crate::yield_if_needed().await;
                    order.borrow_mut().push("busy");
                }},
                TaskQueueHandle::default(),
            )
            .unwrap();
            woken.await;
            busy.await;
            Rc::try_unwrap(order).unwrap().into_inner()
        });
        assert_eq!(order, ["child", "woken", "busy"]);
    }

    /// Calls `f` with the running executor's queues, however the build keeps
    /// the pointer to the running executor.
    ///
    /// With native thread locals the pointer is read directly, which is sound
    /// within a run: `ExecutorContext::enter` installs a pointer to the context
    /// it borrows for the duration of its call and restores the previous one
    /// when the call returns or unwinds, and the context is borrowed here only
    /// for the synchronous call to `f`, inside that span.
    fn with_running_queues<R>(f: impl FnOnce(&ExecutorQueues) -> R) -> R {
        #[cfg(any(not(nightly), not(feature = "native-tls")))]
        return LOCAL_EX.with(|ex| f(&ex.queues.borrow()));

        #[cfg(all(nightly, feature = "native-tls"))]
        return unsafe {
            f(&LOCAL_EX
                .as_ref()
                .expect("this thread doesn't have a LocalExecutor running")
                .queues
                .borrow())
        };
    }

    /// A stall handler that is slow to report runs once the run is accounted,
    /// so the queue is charged for its task and not for the report, and the
    /// run is no longer marked as under way while it reports.
    ///
    /// The handler reports every run longer than 1 ms past its budget, notes
    /// the queue of each report, sleeps 100 ms, and counts the reports it made
    /// with the run still marked as under way. The detector reports a run only
    /// if its helper thread's signal landed while the run was under way, which
    /// a busy machine can delay past the end of the run, so the spinning task
    /// runs again until a report for its queue arrives.
    #[test]
    fn a_slow_stall_report_is_not_charged_to_the_queue() {
        #[derive(Debug)]
        struct SlowHandler(Arc<Mutex<Vec<TaskQueueHandle>>>, Arc<AtomicUsize>);

        impl stall::StallDetectionHandler for SlowHandler {
            fn threshold(&self, _queue: TaskQueueHandle, _max: Duration) -> Option<Duration> {
                Some(Duration::from_millis(1))
            }

            fn stall(&self, detection: stall::StallDetection<'_>) {
                self.0.lock().unwrap().push(detection.queue_handle);
                if with_running_queues(|queues| queues.in_slice) {
                    self.1.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        let reports = Arc::new(Mutex::new(Vec::new()));
        let in_slice = Arc::new(AtomicUsize::new(0));
        let ex = LocalExecutorBuilder::default()
            .preempt_timer(Duration::from_millis(5))
            .detect_stalls(Some(Box::new(SlowHandler(
                reports.clone(),
                in_slice.clone(),
            ))))
            .make()
            .unwrap();
        let (queue, spun, charged) = ex.run(enclose! { (reports) async move {
            let queue = crate::executor().create_task_queue(
                Shares::default(),
                Latency::NotImportant,
                "spins",
            );
            let mut spun = Duration::ZERO;
            for _ in 0..20 {
                spun += crate::spawn_local_into(
                    async {
                        let start = Instant::now();
                        while start.elapsed() < Duration::from_millis(30) {}
                        start.elapsed()
                    },
                    queue,
                )
                .unwrap()
                .await;
                if reports.lock().unwrap().contains(&queue) {
                    break;
                }
            }
            Timer::new(Duration::from_millis(5)).await;
            let charged = crate::executor().task_queue_stats(queue).unwrap().runtime();
            (queue, spun, charged)
        }});
        assert!(
            reports.lock().unwrap().contains(&queue),
            "the stall handler never reported the spinning queue"
        );
        assert!(
            charged < spun + Duration::from_millis(50),
            "the queue spun {spun:?} and was charged {charged:?}"
        );
        assert_eq!(
            in_slice.load(Ordering::Relaxed),
            0,
            "the stall handler reported with the run still marked as under way"
        );
    }

    /// A window that cleanup opens as a slice is set up, before the slice
    /// polls any task, holds the task cleanup polled inline, names no queue
    /// until the slice ends, since the slice's queue is borrowed then, and the
    /// run that the preempt timer then ends at its first check keeps it.
    ///
    /// The shares manager of `dynamic` drops `other`, another executor, as the
    /// slice is set up. A task of `other` owns through `slot` the handle of a
    /// task parked in `cleanup`, a queue of this executor, so this executor
    /// drops the parked task in cleanup, and its destructor spawns a task that
    /// spins until the preempt timer fires and wakes itself. A try in which
    /// the timer had fired before that task began proves nothing and is made
    /// again.
    #[test]
    fn a_window_cleanup_opens_as_a_slice_is_set_up_is_kept() {
        struct SpawnOnDrop(Rc<Cell<bool>>);

        impl Drop for SpawnOnDrop {
            fn drop(&mut self) {
                let crossed = self.0.clone();
                crate::spawn_local(async move {
                    let before = crate::executor().need_preempt();
                    while !crate::executor().need_preempt() {}
                    crossed.set(!before);
                    crate::executor().yield_now().await;
                })
                .detach();
            }
        }

        type Window = Option<(usize, Vec<usize>)>;

        struct DropsOnAdjustment(
            Rc<RefCell<Option<LocalExecutor>>>,
            Rc<RefCell<Option<Window>>>,
        );

        impl SharesManager for DropsOnAdjustment {
            fn shares(&self) -> usize {
                1000
            }

            fn adjustment_period(&self) -> Duration {
                let other = self.0.borrow_mut().take();
                if let Some(other) = other {
                    drop(other);
                    let window = with_running_queues(|queues| {
                        queues.preempted.as_ref().map(|preempted| {
                            let held: Vec<usize> = preempted
                                .inline
                                .iter()
                                .filter_map(|held| held.runnable.as_ref())
                                .map(|runnable| runnable.task_queue_index())
                                .collect();
                            (preempted.queue, held)
                        })
                    });
                    *self.1.borrow_mut() = Some(window);
                }
                Duration::from_secs(1)
            }
        }

        for _ in 0..10 {
            let slot: Rc<RefCell<Option<crate::Task<()>>>> = Rc::new(RefCell::new(None));
            let other = LocalExecutor::default();
            other.run(enclose! { (slot) async move {
                crate::spawn_local(async move {
                    let _slot = slot;
                    Timer::new(Duration::from_secs(3600)).await;
                })
                .detach();
            }});
            let ex = LocalExecutorBuilder::default()
                .preempt_timer(Duration::from_millis(5))
                .make()
                .unwrap();
            let crossed = Rc::new(Cell::new(false));
            let window = Rc::new(RefCell::new(None));
            let armed = Rc::new(RefCell::new(None));
            let cleanup = ex.run(enclose! { (crossed, window, armed) async move {
                let cleanup = crate::executor().create_task_queue(
                    Shares::default(),
                    Latency::NotImportant,
                    "cleanup",
                );
                let started = Rc::new(Cell::new(false));
                let parked = crate::spawn_local_into(
                    enclose! { (started) async move {
                        let _spawns = SpawnOnDrop(crossed);
                        started.set(true);
                        std::future::pending::<()>().await;
                    }},
                    cleanup,
                )
                .unwrap();
                while !started.get() {
                    crate::executor().yield_now().await;
                }
                *slot.borrow_mut() = Some(parked);
                drop(slot);
                let dynamic = crate::executor().create_task_queue(
                    Shares::Dynamic(Rc::new(DropsOnAdjustment(armed.clone(), window))),
                    Latency::NotImportant,
                    "dynamic",
                );
                *armed.borrow_mut() = Some(other);
                crate::spawn_local_into(async {}, dynamic).unwrap().await;
                cleanup.index
            }});
            if !crossed.get() {
                continue;
            }
            assert_eq!(window.take(), Some(Some((usize::MAX, vec![cleanup]))));
            return;
        }
        panic!("the preempt timer had always fired before the task cleanup spawned began");
    }

    /// A task a window remembers, woken after its queue was removed, is sent
    /// to cleanup and run there, not held, so the executor does not park with
    /// it queued, and the window forgets it.
    ///
    /// Cleanup during a slice of the default queue spawns `child` in
    /// `removed`, as in [`a_window_opened_by_cleanup_during_a_slice_belongs_to_the_slice`],
    /// and `child` spins until the preempt timer fires and parks without
    /// waking itself, so the window remembers it and holds nothing. The
    /// default queue's task then removes `removed`, wakes `child`, whose
    /// runnable is cancelled and sent to cleanup, and awaits `child`. The
    /// executor runs on its own thread, so that a park with the cleanup queued
    /// shows as a timeout rather than a test that never ends. A try in which
    /// the timer had fired before `child` began proves nothing and is made
    /// again.
    #[test]
    fn a_task_a_window_remembers_woken_in_a_removed_queue_is_cleaned_up() {
        type Shared<T> = Rc<RefCell<Option<T>>>;

        struct SpawnOnDrop(
            Rc<Cell<bool>>,
            Shared<Waker>,
            Shared<crate::task::JoinHandle<()>>,
        );

        impl Drop for SpawnOnDrop {
            fn drop(&mut self) {
                let crossed = self.0.clone();
                let waker = self.1.clone();
                let child = crate::spawn_local(async move {
                    let before = crate::executor().need_preempt();
                    while !crate::executor().need_preempt() {}
                    crossed.set(!before);
                    poll_fn(|cx| {
                        *waker.borrow_mut() = Some(cx.waker().clone());
                        Poll::<()>::Pending
                    })
                    .await;
                });
                *self.2.borrow_mut() = Some(child.detach());
            }
        }

        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for _ in 0..10 {
                let slot: Shared<crate::Task<()>> = Rc::new(RefCell::new(None));
                let other = LocalExecutor::default();
                other.run(enclose! { (slot) async move {
                    crate::spawn_local(async move {
                        let _slot = slot;
                        Timer::new(Duration::from_secs(3600)).await;
                    })
                    .detach();
                }});
                let ex = LocalExecutorBuilder::default()
                    .preempt_timer(Duration::from_millis(5))
                    .make()
                    .unwrap();
                let crossed = Rc::new(Cell::new(false));
                let cleaned = ex.run(enclose! { (crossed) async move {
                    let removed = crate::executor().create_task_queue(
                        Shares::default(),
                        Latency::NotImportant,
                        "removed",
                    );
                    let waker: Shared<Waker> = Rc::new(RefCell::new(None));
                    let child: Shared<crate::task::JoinHandle<()>> = Rc::new(RefCell::new(None));
                    let started = Rc::new(Cell::new(false));
                    let parked = crate::spawn_local_into(
                        enclose! { (started, crossed, waker, child) async move {
                            let _spawns = SpawnOnDrop(crossed, waker, child);
                            started.set(true);
                            std::future::pending::<()>().await;
                        }},
                        removed,
                    )
                    .unwrap();
                    while !started.get() {
                        crate::executor().yield_now().await;
                    }
                    *slot.borrow_mut() = Some(parked);
                    drop(slot);
                    drop(other);
                    if !crossed.get() {
                        return None;
                    }
                    crate::executor().remove_task_queue(removed).unwrap();
                    waker.borrow_mut().take().unwrap().wake();
                    let remembered = with_running_queues(|queues| {
                        queues
                            .preempted
                            .as_ref()
                            .map_or(0, |preempted| preempted.inline.len())
                    });
                    let child = child.borrow_mut().take().unwrap();
                    Some((child.await.is_none(), remembered))
                }});
                match cleaned {
                    None => continue,
                    Some(outcome) => {
                        sender.send(outcome).unwrap();
                        return;
                    }
                }
            }
            panic!("the preempt timer had always fired before `child` began");
        });
        match receiver.recv_timeout(Duration::from_secs(10)) {
            Ok((cancelled, remembered)) => {
                assert!(
                    cancelled,
                    "the child in the removed queue was not cancelled"
                );
                assert_eq!(
                    remembered, 0,
                    "the window still remembers the cancelled child"
                );
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("the executor parked with the cleanup queued")
            }
            Err(error) => panic!("the executor's thread failed: {error}"),
        }
    }

    /// A window forgets a task it remembers but holds nothing of once another
    /// executor's cleanup destroys it, so that a task allocated at its address
    /// later is not taken for it.
    ///
    /// Dropping `spawner`, another executor, during a slice of the default
    /// queue makes this executor run a destructor in cleanup, which spawns
    /// `child`, which spins until the preempt timer fires and parks without
    /// waking itself, so the window remembers it and holds nothing. The
    /// destructor hands `child`'s only handle to a task of `owner`, another
    /// executor, and dropping `owner` cancels `child` from `owner`'s context,
    /// so this executor destroys it in cleanup. A try in which the timer had
    /// fired before `child` began proves nothing and is made again.
    #[test]
    fn a_window_forgets_a_task_another_executor_destroys() {
        type Shared<T> = Rc<RefCell<Option<T>>>;

        struct SpawnOnDrop(Rc<Cell<bool>>, Shared<crate::task::JoinHandle<()>>);

        impl Drop for SpawnOnDrop {
            fn drop(&mut self) {
                let crossed = self.0.clone();
                let child = crate::spawn_local(async move {
                    let before = crate::executor().need_preempt();
                    while !crate::executor().need_preempt() {}
                    crossed.set(!before);
                    std::future::pending::<()>().await;
                });
                *self.1.borrow_mut() = Some(child.detach());
            }
        }

        /// An executor with a task that owns `slot`, and keeps it until the
        /// executor is dropped.
        fn owning<T: 'static>(slot: Shared<T>) -> LocalExecutor {
            let ex = LocalExecutor::default();
            ex.run(async move {
                crate::spawn_local(async move {
                    let _slot = slot;
                    Timer::new(Duration::from_secs(3600)).await;
                })
                .detach();
            });
            ex
        }

        for _ in 0..10 {
            let parked_slot: Shared<crate::Task<()>> = Rc::new(RefCell::new(None));
            let child_slot: Shared<crate::task::JoinHandle<()>> = Rc::new(RefCell::new(None));
            let spawner = owning(parked_slot.clone());
            let owner = owning(child_slot.clone());
            let ex = LocalExecutorBuilder::default()
                .preempt_timer(Duration::from_millis(5))
                .make()
                .unwrap();
            let crossed = Rc::new(Cell::new(false));
            let remembered = ex.run(enclose! { (crossed) async move {
                let cleanup = crate::executor().create_task_queue(
                    Shares::default(),
                    Latency::NotImportant,
                    "cleanup",
                );
                let started = Rc::new(Cell::new(false));
                let parked = crate::spawn_local_into(
                    enclose! { (started, crossed, child_slot) async move {
                        let _spawns = SpawnOnDrop(crossed, child_slot);
                        started.set(true);
                        std::future::pending::<()>().await;
                    }},
                    cleanup,
                )
                .unwrap();
                drop(child_slot);
                while !started.get() {
                    crate::executor().yield_now().await;
                }
                *parked_slot.borrow_mut() = Some(parked);
                drop(parked_slot);
                let remembered = || {
                    with_running_queues(|queues| {
                        queues
                            .preempted
                            .as_ref()
                            .map_or(0, |preempted| preempted.inline.len())
                    })
                };
                drop(spawner);
                let before = remembered();
                drop(owner);
                (before, remembered())
            }});
            if !crossed.get() {
                continue;
            }
            assert_eq!(remembered, (1, 0));
            return;
        }
        panic!("the preempt timer had always fired before `child` began");
    }

    /// A shares manager whose adjustment period panics once `panics` is set.
    struct PanicsOnAdjustment(Rc<Cell<bool>>);

    impl SharesManager for PanicsOnAdjustment {
        fn shares(&self) -> usize {
            1000
        }

        fn adjustment_period(&self) -> Duration {
            assert!(!self.0.get(), "the shares manager panicked");
            Duration::from_secs(1)
        }
    }

    /// A panic in user code the executor runs as a slice begins, before any
    /// task is polled, still ends the slice.
    ///
    /// A queue with dynamic shares asks its manager for the adjustment period
    /// as its slice begins, and this one panics there. The panic leaves `run`,
    /// and with the slice still marked as running, a task spawned between runs
    /// by a destructor, during whose first poll the preempt timer fires, would
    /// open a window that no run ends. Whether the timer fires during that poll
    /// depends on when the last run armed it, so a try in which it was already
    /// pending is made again on a new executor.
    #[test]
    fn a_panic_as_a_slice_begins_ends_the_slice() {
        struct SpawnOnDrop(Rc<Cell<bool>>);

        impl Drop for SpawnOnDrop {
            fn drop(&mut self) {
                let fired = self.0.clone();
                crate::spawn_local(async move {
                    let reactor = crate::executor().reactor();
                    let pending = reactor.need_preempt();
                    let start = Instant::now();
                    while !reactor.need_preempt() && start.elapsed() < Duration::from_millis(500) {}
                    fired.set(!pending && reactor.need_preempt());
                    crate::executor().yield_now().await;
                })
                .detach();
            }
        }

        for _ in 0..10 {
            let ex = LocalExecutorBuilder::default()
                .preempt_timer(Duration::from_millis(5))
                .make()
                .unwrap();
            let panics = Rc::new(Cell::new(false));
            let manager = Rc::new(PanicsOnAdjustment(panics.clone()));
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ex.run(async {
                    let queue = crate::executor().create_task_queue(
                        Shares::Dynamic(manager),
                        Latency::NotImportant,
                        "panics",
                    );
                    panics.set(true);
                    crate::spawn_local_into(async {}, queue).unwrap().await;
                })
            }));
            assert!(outcome.is_err(), "the shares manager never panicked");
            {
                let queues = ex.context.queues.borrow();
                assert!(!queues.in_slice, "the slice is still marked as running");
                assert!(
                    queues.active_executing.is_none(),
                    "a queue is still marked as executing"
                );
            }
            let fired = Rc::new(Cell::new(false));
            let guard = SpawnOnDrop(fired.clone());
            let parked = ex.spawn(async move {
                let _guard = guard;
                futures::future::pending::<()>().await;
            });
            drop(parked);
            if !fired.get() {
                continue;
            }
            assert!(
                ex.context.queues.borrow().preempted.is_none(),
                "a window opened outside any slice"
            );
            return;
        }
        panic!("the preempt timer never fired during the child's first poll");
    }

    /// A task polled inline whose first poll finished is not remembered, so
    /// another task allocated where it was keeps its place.
    ///
    /// `top` spawns `finished`, which spins until the preempt timer fires and
    /// finishes, and then three tasks of one type, each of which wakes itself
    /// once and notes that it ran; the first is usually allocated where
    /// `finished` was. Then `top` arms a timer that wakes `woken` in the next
    /// reactor pass and yields, so `woken` is woken after the three were
    /// queued however the thread was delayed, and the three run first, in the
    /// order they were spawned, then `woken`, then `top`. Were `finished`
    /// remembered, the task at its address would be held behind `woken`. A
    /// try in which the timer had fired before `finished` began proves nothing
    /// and is made again.
    #[test]
    fn a_finished_inline_task_is_not_remembered() {
        async fn wakes_once(id: u32, order: Rc<RefCell<Vec<String>>>) {
            crate::executor().yield_now().await;
            order.borrow_mut().push(format!("c{id}"));
        }

        for _ in 0..10 {
            let ex = LocalExecutorBuilder::default()
                .preempt_timer(Duration::from_millis(5))
                .make()
                .unwrap();
            let outcome = ex.run(async {
                PARKED.with(|parked| *parked.borrow_mut() = vec![None]);
                let order = Rc::new(RefCell::new(Vec::new()));
                let woken = crate::spawn_local_into(
                    enclose! { (order) async move {
                        parked_until_woken(0).await;
                        order.borrow_mut().push("woken".to_string());
                    }},
                    TaskQueueHandle::default(),
                )
                .unwrap();
                let top = crate::spawn_local_into(
                    enclose! { (order) async move {
                        let pending = crate::executor().need_preempt();
                        crate::spawn_local(async {
                            while !crate::executor().need_preempt() {}
                        })
                        .detach();
                        let remembered = with_running_queues(|queues| {
                            queues
                                .preempted
                                .as_ref()
                                .is_some_and(|preempted| !preempted.inline.is_empty())
                        });
                        for id in 1..=3 {
                            crate::spawn_local(wakes_once(id, order.clone())).detach();
                        }
                        let waker = Waker::from(std::sync::Arc::new(WakeParked));
                        let _wakes_woken = timer_waking(&waker, Duration::ZERO);
                        crate::yield_if_needed().await;
                        order.borrow_mut().push("top".to_string());
                        (pending, remembered)
                    }},
                    TaskQueueHandle::default(),
                )
                .unwrap();
                woken.await;
                let (pending, remembered) = top.await;
                Timer::new(Duration::from_millis(10)).await;
                (
                    pending,
                    remembered,
                    Rc::try_unwrap(order).unwrap().into_inner(),
                )
            });
            let (pending, remembered, order) = outcome;
            if pending {
                continue;
            }
            assert!(!remembered, "the finished task was remembered");
            assert_eq!(order, ["c1", "c2", "c3", "woken", "top"]);
            return;
        }
        panic!("the preempt timer had always fired before the finished task began");
    }

    /// A task that was preempted goes behind the tasks woken during its slice,
    /// but not behind every task woken after that.
    ///
    /// `busy` spins until preempted and yields. Eight tasks each await a nop
    /// and then spin for 5 ms, so every reactor pass wakes some of them and
    /// together they want twice the CPU a slice gives, about eight wakes for
    /// every slice `busy` gets. If the preempted task went behind every wake
    /// until the wakes stopped, `busy` would never run again.
    ///
    /// The flood awaits first, so `busy` is the first task preempted, and
    /// `busy` is scheduled rather than spawned with `spawn_local`, which would
    /// run its first slice inside the task spawning it. Every task stops when
    /// the run is over, so a `busy` that is starved fails the test rather than
    /// hanging it.
    ///
    /// The bound counts wakes rather than time, so a thread the system
    /// deschedules does not read as starvation: `busy` must get a slice for
    /// every sixteen wakes, two rounds of the flood.
    #[test]
    fn a_preempted_task_still_gets_slices_under_a_flood_of_wakes() {
        const PERIOD: Duration = Duration::from_millis(20);
        const FLOOD: usize = 8;
        const WAKES_PER_SLICE: usize = 16;
        const RUN: Duration = Duration::from_secs(1);

        let ex = LocalExecutorBuilder::default()
            .preempt_timer(PERIOD)
            .make()
            .unwrap();
        let (slices, wakes) = ex.run(async {
            let start = Instant::now();
            let wakes = Rc::new(Cell::new(0usize));
            let flood: Vec<_> = (0..FLOOD)
                .map(|_| {
                    crate::spawn_local(enclose! { (wakes) async move {
                        while start.elapsed() < RUN {
                            nop().await;
                            wakes.set(wakes.get() + 1);
                            let start = Instant::now();
                            while start.elapsed() < Duration::from_millis(5) {}
                        }
                    }})
                })
                .collect();
            let busy = crate::spawn_local_into(
                async move {
                    let mut slices = 0usize;
                    while start.elapsed() < RUN {
                        if crate::executor().need_preempt() {
                            slices += 1;
                            crate::yield_if_needed().await;
                        }
                    }
                    slices
                },
                TaskQueueHandle::default(),
            )
            .unwrap();
            let slices = busy.await;
            join_all(flood).await;
            (slices, wakes.get())
        });
        assert!(
            wakes >= 4 * WAKES_PER_SLICE,
            "the flood woke {wakes} times in {RUN:?}, too few to crowd anything out"
        );
        assert!(
            slices * WAKES_PER_SLICE >= wakes,
            "the preempted task got {slices} slices in {RUN:?} under a flood of {wakes} wakes"
        );
    }

    /// How the sleeping queue in the share tests spends each turn.
    #[derive(Clone, Copy, Debug)]
    enum Turn {
        /// Spins for three quarters of a slice.
        Part,
        /// Spins until it is preempted.
        Whole,
    }

    /// Returns the runtime a queue that sleeps for 1 ms after every turn gets,
    /// divided by the runtime of a queue that only yields when preempted.
    ///
    /// Each queue must take twenty turns or more, so the ratio is never one
    /// or two turns against each other.
    ///
    /// The queues hold `sleeper` and `busy` shares. A queue is placed afresh
    /// each time it wakes, which forgets part of what its shares charged it,
    /// so the tests hold a sleeping queue to an equal turn with `busy` at most
    /// rather than to its shares.
    fn sleeper_runtime_ratio(turn: Turn, sleeper: usize, busy: usize) -> f64 {
        const TURNS: usize = 20;
        const PERIOD: Duration = Duration::from_millis(20);
        const RUN: Duration = Duration::from_millis(1500);

        let ex = LocalExecutorBuilder::default()
            .preempt_timer(PERIOD)
            .make()
            .unwrap();
        ex.run(async move {
            let sleeper = crate::executor().create_task_queue(
                Shares::Static(sleeper),
                Latency::NotImportant,
                "sleeper",
            );
            let busy = crate::executor().create_task_queue(
                Shares::Static(busy),
                Latency::NotImportant,
                "busy",
            );
            let start = Instant::now();
            let sleeper = crate::spawn_local_into(
                async move {
                    let mut ran = Duration::ZERO;
                    let mut turns = 0usize;
                    while start.elapsed() < RUN {
                        turns += 1;
                        let turn_start = Instant::now();
                        match turn {
                            Turn::Part => while turn_start.elapsed() < PERIOD * 3 / 4 {},
                            Turn::Whole => while !crate::executor().need_preempt() {},
                        }
                        ran += turn_start.elapsed();
                        Timer::new(Duration::from_millis(1)).await;
                    }
                    (ran, turns)
                },
                sleeper,
            )
            .unwrap();
            let busy = crate::spawn_local_into(
                async move {
                    let mut ran = Duration::ZERO;
                    let mut turns = 0usize;
                    while start.elapsed() < RUN {
                        turns += 1;
                        let turn_start = Instant::now();
                        while !crate::executor().need_preempt() && start.elapsed() < RUN {}
                        ran += turn_start.elapsed();
                        crate::yield_if_needed().await;
                    }
                    (ran, turns)
                },
                busy,
            )
            .unwrap();
            let ((sleeper, sleeper_turns), (busy, busy_turns)) = join(sleeper, busy).await;
            assert!(
                sleeper_turns >= TURNS && busy_turns >= TURNS,
                "the sleeping queue took {sleeper_turns} turns and the busy one {busy_turns}"
            );
            sleeper.as_secs_f64() / busy.as_secs_f64()
        })
    }

    /// A queue of equal shares that sleeps after part of each slice gets an
    /// equal turn, no more and no less.
    ///
    /// Woken while the busy queue runs, it goes ahead of that queue's next
    /// slice. Placed as usual it waited for that slice and got about 0.6 of
    /// the busy queue's runtime; placed ahead with no regard for its charge,
    /// it got three times the busy queue's runtime.
    #[test]
    fn a_queue_that_sleeps_after_part_of_a_slice_gets_an_equal_turn() {
        let ratio = sleeper_runtime_ratio(Turn::Part, 1000, 1000);
        assert!(
            (0.75..1.25).contains(&ratio),
            "the sleeping queue ran {ratio:.2} times as long as the busy one"
        );
    }

    /// A queue of equal shares that uses up its slice and then sleeps gets an
    /// equal turn.
    ///
    /// It is preempted, and so charged for its slice before it sleeps. Placed
    /// as usual when it wakes it waited a second slice of the busy queue and
    /// got half the busy queue's runtime.
    #[test]
    fn a_queue_that_sleeps_after_a_whole_slice_gets_an_equal_turn() {
        let ratio = sleeper_runtime_ratio(Turn::Whole, 1000, 1000);
        assert!(
            (0.75..1.25).contains(&ratio),
            "the sleeping queue ran {ratio:.2} times as long as the busy one"
        );
    }

    /// A queue with one share against a thousand that sleeps after part of
    /// each slice gets what it got before: less than an equal turn, but a
    /// turn behind every second slice of the busy queue.
    ///
    /// Its charge for a turn is a thousand times the busy queue's, so it is
    /// placed as usual. Placed ahead regardless of its charge it got three
    /// times the busy queue's runtime, and placed no earlier than its charge
    /// with nothing to bring it back it got a hundredth.
    #[test]
    fn a_low_share_queue_that_sleeps_keeps_its_turn_behind_two_slices() {
        let ratio = sleeper_runtime_ratio(Turn::Part, 1, 1000);
        assert!(
            (0.3..1.0).contains(&ratio),
            "the sleeping queue ran {ratio:.2} times as long as the busy one"
        );
    }

    #[test]
    fn a_due_timer_runs_ahead_of_io_that_completed_in_the_same_idle_poll() {
        assert_a_woken_task_waits_one_slice(SliceEnd::Nop, Wake::Timer, 1);
    }

    #[test]
    fn a_due_timer_in_another_queue_runs_ahead_of_io_that_completed_in_the_same_idle_poll() {
        assert_a_woken_task_waits_one_slice(SliceEnd::Nop, Wake::Timer, 2);
    }

    /// A task polled inline by a waker the reactor runs is not taken for the
    /// one that spends the next slice.
    ///
    /// After every preempted slice of `busy`, the reactor's pass runs a waker
    /// that spawns a task while `need_preempt` is still set. Remembered past
    /// the start of the next round, that task would be taken for the one that
    /// spent the next slice, and `busy`, which yielded, would not be held.
    #[test]
    fn a_task_spawned_by_a_waker_between_slices_is_not_taken_for_the_next() {
        assert_a_woken_task_waits_one_slice_with(SliceEnd::Yield, Wake::Timer, 1, true);
    }

    /// A task spawned after the preempt timer fired is not taken for the one
    /// that spent the slice, which spawned it.
    #[test]
    fn a_due_timer_runs_ahead_of_a_task_that_spawned_another_after_it_was_preempted() {
        assert_a_woken_task_waits_one_slice(SliceEnd::SpawnThenYield, Wake::Timer, 1);
    }

    /// A task that does its work in tasks it spawns, each polled at once and
    /// finished, and yields when preempted, goes behind the tasks woken during
    /// its slice, although the preempt timer fired during one of its children.
    #[test]
    fn a_due_timer_runs_ahead_of_a_task_whose_inline_child_finished_when_preempted() {
        assert_a_woken_task_waits_one_slice(SliceEnd::YieldAfterInlineWork, Wake::Timer, 1);
    }

    #[test]
    fn a_due_timer_runs_ahead_of_a_task_that_yielded_when_preempted() {
        assert_a_woken_task_waits_one_slice(SliceEnd::Yield, Wake::Timer, 1);
    }

    #[test]
    fn a_due_timer_in_another_queue_runs_ahead_of_a_queue_that_yielded_when_preempted() {
        assert_a_woken_task_waits_one_slice(SliceEnd::Yield, Wake::Timer, 2);
    }

    #[test]
    fn completed_io_runs_ahead_of_a_task_that_yielded_when_preempted() {
        assert_a_woken_task_waits_one_slice(SliceEnd::Yield, Wake::Nop, 1);
    }

    #[test]
    fn completed_io_in_another_queue_runs_ahead_of_a_queue_that_yielded_when_preempted() {
        assert_a_woken_task_waits_one_slice(SliceEnd::Yield, Wake::Nop, 2);
    }

    #[test]
    fn completed_io_runs_ahead_of_a_preempted_task_whose_timer_came_due() {
        assert_a_woken_task_waits_one_slice(SliceEnd::DueTimer, Wake::Nop, 1);
    }

    #[test]
    fn completed_io_in_another_queue_runs_ahead_of_a_preempted_queue_whose_timer_came_due() {
        assert_a_woken_task_waits_one_slice(SliceEnd::DueTimer, Wake::Nop, 2);
    }
}
