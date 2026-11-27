//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
//!

use std::{
    cell::{Cell, RefCell},
    collections::{btree_map::Entry, BTreeMap},
    fmt,
    future::Future,
    io, mem,
    os::unix::io::RawFd,
    rc::Rc,
    sync::Arc,
    task::Waker,
    time::{Duration, Instant},
};

use io_uring::CompletionStatus;
use nix::sys::socket::{MsgFlags, SockaddrLike, SockaddrStorage};
use smallvec::SmallVec;

use crate::sys::source::NativePath;
use crate::{
    io::{FileScheduler, IoScheduler, ScheduledSource},
    sys::SockAddrStorage,
    sys::{
        self, blocking::BlockingThreadPool, common_flags, read_flags, DirectIo, DmaBuffer,
        DmaSource, IoBuffer, PollableStatus, SleepNotifier, Source, SourceType, StatsCollection,
        Statx, MAX_RW_COUNT,
    },
    IoRequirements, IoStats, Latency, TaskQueueHandle,
};
use nix::poll::PollFlags;

use crate::timer::TimerId;

struct SharedChannels {
    id: u64,
    /// How much room each connected channel has, for as long as it is connected.
    checkers: BTreeMap<u64, Box<dyn Fn() -> usize>>,
    /// Wakers waiting on a channel. A channel has an entry only while some do.
    pending: BTreeMap<u64, SmallVec<[Waker; 1]>>,
    /// Halves waiting in `connect()` for their peer.
    connecting: BTreeMap<u64, Connecting>,
}

/// A shared channel half waiting in `connect()`.
struct Connecting {
    /// Whether the peer has connected yet.
    peer_connected: Rc<dyn Fn() -> bool>,
    /// What to wake once it has.
    waker: Waker,
}

impl SharedChannels {
    fn new() -> SharedChannels {
        SharedChannels {
            id: 0,
            checkers: BTreeMap::new(),
            pending: BTreeMap::new(),
            connecting: BTreeMap::new(),
        }
    }

    /// Detaches ready wakers so their callbacks can reenter the registry.
    fn take_ready_wakers(&mut self) -> SmallVec<[Waker; 8]> {
        let mut ready = SmallVec::new();
        ready.extend(
            self.connecting
                .extract_if(.., |_, connecting| (connecting.peer_connected)())
                .map(|(_, connecting)| connecting.waker),
        );

        let Self {
            checkers, pending, ..
        } = self;
        pending.retain(|id, wakers| {
            let room = std::cmp::min(checkers[id](), wakers.len());
            ready.extend(wakers.drain(0..room).rev());
            !wakers.is_empty()
        });
        ready
    }
}

/// The reactor's timers.
///
/// A thin wrapper so the reactor does not name the wheel directly.
struct Timers {
    wheel: crate::timer::reactor_adapter::ReactorTimers,
}

impl Timers {
    fn new() -> Timers {
        Timers {
            wheel: crate::timer::reactor_adapter::ReactorTimers::new(),
        }
    }

    fn insert(&mut self, when: Instant, waker: Waker, latency: bool) -> TimerId {
        self.wheel.insert(when, waker, latency)
    }

    fn remove(&mut self, id: TimerId) -> Option<Waker> {
        self.wheel.remove(id)
    }

    fn update_waker(&mut self, id: TimerId, waker: &Waker) -> Option<Option<Waker>> {
        self.wheel.update_waker(id, waker)
    }

    fn exists(&self, id: TimerId) -> bool {
        self.wheel.contains(id)
    }

    /// Appends the wakers of everything that has come due.
    fn process_timers(&mut self, wakers: &mut Vec<Waker>) {
        self.wheel.process_timers(wakers)
    }

    /// When the next timer is due, and when the next latency timer is.
    fn next_expiries(&self) -> (Option<Instant>, Option<Instant>) {
        self.wheel.next_expiries()
    }
}

/// The reactor.
///
/// Every async I/O handle and every timer is registered here. Invocations of
/// [`run()`][`crate::run()`] poll the reactor to periodically check for new
/// events
///
/// There is only one global instance of this type, accessible by
/// [`Local::get_reactor()`].
pub(crate) struct Reactor {
    /// Raw bindings to `epoll`/`kqueue`/`wepoll`.
    pub(crate) sys: sys::Reactor,

    timers: RefCell<Timers>,

    /// Reused across polls so expiring a batch of timers allocates nothing.
    /// Lives outside `timers` so it can be filled under that borrow and
    /// drained after it is released.
    timer_wakers: RefCell<Vec<Waker>>,

    shared_channels: RefCell<SharedChannels>,

    io_scheduler: Rc<IoScheduler>,
    record_io_latencies: bool,

    /// Whether the latency ring has events waiting. Taken once at startup:
    /// `need_preempt` runs constantly and must not borrow the ring to ask.
    preempt_status: CompletionStatus,

    /// Whether any `Latency::Matters` queue exists, which is what a timer
    /// registered outside a queue's slice is classed by.
    latency_queues: Cell<bool>,
}

impl Reactor {
    pub(crate) fn new(
        notifier: Arc<SleepNotifier>,
        io_memory: usize,
        ring_depth: usize,
        record_io_latencies: bool,
        blocking_thread: BlockingThreadPool,
        recv_buffers: crate::executor::RecvBuffers,
    ) -> crate::Result<Reactor, ()> {
        let sys = sys::Reactor::new(
            notifier,
            io_memory,
            ring_depth,
            blocking_thread,
            recv_buffers,
        )?;
        let preempt_status = sys.preempt_status();
        Ok(Reactor {
            sys,
            timers: RefCell::new(Timers::new()),
            timer_wakers: RefCell::new(Vec::new()),
            shared_channels: RefCell::new(SharedChannels::new()),
            io_scheduler: Rc::new(IoScheduler::new()),
            record_io_latencies,
            preempt_status,
            latency_queues: Cell::new(false),
        })
    }

    /// Says whether any `Latency::Matters` queue exists.
    pub(crate) fn inform_latency_queues(&self, exist: bool) {
        self.latency_queues.set(exist);
    }

    /// Ends the running slice within `latency` of now, that of a queue just
    /// created, if the preempt timer armed would end it later. The next pass
    /// arms the preempt timer for the new queue; until then the slice runs
    /// under the one armed before the queue existed. A submission the kernel
    /// refuses is left for the next pass to report.
    pub(crate) fn tighten_preempt_timer(&self, latency: Duration) {
        let _ = self.sys.tighten_preempt_timer(latency);
    }

    pub(crate) fn io_stats(&self) -> IoStats {
        self.sys.io_stats()
    }

    pub(crate) fn task_queue_io_stats(&self, handle: &TaskQueueHandle) -> Option<IoStats> {
        self.sys.task_queue_io_stats(handle)
    }

    #[inline(always)]
    pub(crate) fn need_preempt(&self) -> bool {
        !self.preempt_status.is_empty()
    }

    pub(crate) fn id(&self) -> usize {
        self.sys.id()
    }

    pub(crate) fn ring_depth(&self) -> usize {
        self.sys.ring_depth()
    }

    fn new_source(
        &self,
        raw: RawFd,
        stype: SourceType,
        stats_collection: Option<&'static StatsCollection>,
    ) -> Source {
        sys::Source::new(
            self.io_scheduler.requirements(),
            raw,
            stype,
            stats_collection,
            Some(crate::executor().current_task_queue()),
        )
    }

    pub(crate) fn inform_io_requirements(&self, req: IoRequirements) {
        self.io_scheduler.inform_requirements(req);
    }

    /// Returns the I/O requirements to restore after a temporary cleanup context.
    pub(crate) fn io_requirements(&self) -> IoRequirements {
        self.io_scheduler.requirements()
    }

    pub(crate) fn register_shared_channel<F>(&self, test_function: Box<F>) -> u64
    where
        F: Fn() -> usize + 'static,
    {
        let mut channels = self.shared_channels.borrow_mut();
        let id = channels.id;
        channels.id += 1;
        let ret = channels.checkers.insert(id, test_function);
        assert!(ret.is_none());
        id
    }

    /// How many shared channel halves the reactor still holds anything for,
    /// a room check or a waiting waker.
    #[cfg(test)]
    pub(crate) fn registered_shared_channels(&self) -> usize {
        let channels = self.shared_channels.borrow();
        let mut ids: Vec<u64> = channels
            .checkers
            .keys()
            .chain(channels.pending.keys())
            .chain(channels.connecting.keys())
            .copied()
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids.len()
    }

    /// How many wakers wait on the shared channel half `id`.
    #[cfg(test)]
    pub(crate) fn shared_channel_wakers(&self, id: u64) -> usize {
        self.shared_channels
            .borrow()
            .pending
            .get(&id)
            .map_or(0, |wakers| wakers.len())
    }

    pub(crate) fn unregister_shared_channel(&self, id: u64) {
        let mut channels = self.shared_channels.borrow_mut();
        let removed = (
            channels.checkers.remove(&id),
            channels.pending.remove(&id),
            channels.connecting.remove(&id),
        );
        drop(channels);
        drop(removed);
    }

    /// Wakes `waker` once `peer_connected` holds for the half `id`, in place
    /// of whatever waker `id` waited with before.
    ///
    /// Only the peer's `notify`, sent once it has connected or been dropped,
    /// wakes this executor for it, so `peer_connected` is checked on every
    /// reactor pass, including the drain before a park.
    pub(crate) fn wait_for_shared_channel_peer(
        &self,
        id: u64,
        peer_connected: &Rc<dyn Fn() -> bool>,
        waker: &Waker,
    ) {
        let mut channels = self.shared_channels.borrow_mut();
        let replaced = match channels.connecting.entry(id) {
            Entry::Vacant(entry) => {
                entry.insert(Connecting {
                    peer_connected: peer_connected.clone(),
                    waker: waker.clone(),
                });
                None
            }
            Entry::Occupied(entry) => {
                let current = &mut entry.into_mut().waker;
                (!current.will_wake(waker)).then(|| mem::replace(current, waker.clone()))
            }
        };
        drop(channels);
        drop(replaced);
    }

    /// Withdraws what [`wait_for_shared_channel_peer`] registered for `id`.
    ///
    /// [`wait_for_shared_channel_peer`]: Self::wait_for_shared_channel_peer
    pub(crate) fn stop_waiting_for_shared_channel_peer(&self, id: u64) {
        let removed = self.shared_channels.borrow_mut().connecting.remove(&id);
        drop(removed);
    }

    /// Queues `waker` for the half `id`, unless it is queued there already.
    pub(crate) fn add_shared_channel_waker(&self, id: u64, waker: &Waker) {
        let mut channels = self.shared_channels.borrow_mut();
        let wakers = channels.pending.entry(id).or_default();
        if !wakers.iter().any(|queued| queued.will_wake(waker)) {
            wakers.push(waker.clone());
        }
    }

    pub(crate) fn alloc_dma_buffer(&self, size: usize) -> DmaBuffer {
        self.sys.alloc_dma_buffer(size)
    }

    pub(crate) fn write_dma(
        &self,
        raw: RawFd,
        buf: DmaSource,
        pos: u64,
        pollable: PollableStatus,
    ) -> Source {
        const STATS: StatsCollection = StatsCollection {
            fulfilled: Some(|result, stats, op_count| {
                if let Ok(result) = result {
                    stats.file_writes += op_count;
                    stats.file_bytes_written += *result as u64 * op_count;
                }
            }),
            reused: None,
            latency: None,
        };

        let source = self.new_source(
            raw,
            SourceType::Write(pollable, IoBuffer::DmaSource(buf)),
            Some(&STATS),
        );
        self.sys.write_dma(&source, pos);
        source
    }

    pub(crate) fn copy_file_range(
        &self,
        fd_in: RawFd,
        off_in: u64,
        fd_out: RawFd,
        off_out: u64,
        len: usize,
    ) -> impl Future<Output = Source> {
        const STATS: StatsCollection = StatsCollection {
            fulfilled: Some(|result, stats, op_count| {
                if let Ok(result) = result {
                    let len = *result as u64 * op_count;

                    stats.file_reads += op_count;
                    stats.file_bytes_read += len;
                    stats.file_writes += op_count;
                    stats.file_bytes_written += len;
                }
            }),
            reused: None,
            latency: None,
        };

        let source = self.new_source(
            fd_out,
            SourceType::CopyFileRange(fd_in, off_in, len),
            Some(&STATS),
        );
        let waiter = self.sys.copy_file_range(&source, off_out);
        async move {
            waiter.await;
            source
        }
    }

    pub(crate) fn write_buffered(&self, raw: RawFd, buf: Vec<u8>, pos: u64) -> Source {
        const STATS: StatsCollection = StatsCollection {
            fulfilled: Some(|result, stats, op_count| {
                if let Ok(result) = result {
                    stats.file_buffered_writes += op_count;
                    stats.file_buffered_bytes_written += *result as u64 * op_count;
                }
            }),
            reused: None,
            latency: None,
        };

        let source = self.new_source(
            raw,
            SourceType::Write(
                PollableStatus::NonPollable(DirectIo::Disabled),
                IoBuffer::Buffered(buf),
            ),
            Some(&STATS),
        );
        self.sys.write_buffered(&source, pos);
        source
    }

    pub(crate) fn connect(&self, raw: RawFd, addr: impl SockaddrLike) -> Source {
        let addr = unsafe { SockaddrStorage::from_raw(addr.as_ptr(), Some(addr.len())) }.unwrap();
        let source = self.new_source(raw, SourceType::Connect(addr), None);
        self.sys.connect(&source);
        source
    }

    pub(crate) fn connect_timeout(&self, raw: RawFd, addr: SockaddrStorage, d: Duration) -> Source {
        let source = self.new_source(raw, SourceType::Connect(addr), None);
        source.set_timeout(d);
        self.sys.connect(&source);
        source
    }

    pub(crate) fn accept(&self, raw: RawFd) -> Source {
        let addr = SockAddrStorage::uninit();
        let source = self.new_source(raw, SourceType::Accept(addr), None);
        self.sys.accept(&source);
        source
    }

    pub(crate) fn poll_read_ready(&self, fd: RawFd) -> Source {
        let source = self.new_source(fd, SourceType::PollAdd, None);
        self.sys.poll_ready(&source, common_flags() | read_flags());
        source
    }

    pub(crate) fn poll_write_ready(&self, fd: RawFd) -> Source {
        let source = self.new_source(fd, SourceType::PollAdd, None);
        self.sys
            .poll_ready(&source, common_flags() | PollFlags::POLLOUT);
        source
    }

    pub(crate) fn rushed_send(
        &self,
        fd: RawFd,
        buf: DmaBuffer,
        timeout: Option<Duration>,
    ) -> io::Result<Source> {
        let source = self.new_source(fd, SourceType::SockSend(buf), None);
        if let Some(timeout) = timeout {
            source.set_timeout(timeout);
        }
        self.sys.send(&source, crate::net::SEND_FLAGS);
        self.rush_dispatch(&source)?;
        Ok(source)
    }

    #[allow(no_comments_allowed)]
    pub(crate) fn rushed_sendmsg(
        &self,
        fd: RawFd,
        buf: DmaBuffer,
        addr: impl nix::sys::socket::SockaddrLike,
        timeout: Option<Duration>,
    ) -> io::Result<Source> {
        let iov = libc::iovec {
            iov_base: buf.as_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        // Note that the iov and addresses we have above are stack addresses. We will
        // leave it blank and the `io_uring` callee will fill that up
        let hdr = unsafe { std::mem::zeroed::<libc::msghdr>() };

        let addr = unsafe { SockaddrStorage::from_raw(addr.as_ptr(), Some(addr.len())) }.unwrap();
        let source = self.new_source(fd, SourceType::SockSendMsg(buf, iov, hdr, addr), None);
        if let Some(timeout) = timeout {
            source.set_timeout(timeout);
        }

        self.sys.sendmsg(&source, crate::net::SEND_FLAGS);
        self.rush_dispatch(&source)?;
        Ok(source)
    }

    pub(crate) fn rushed_recvmsg(
        &self,
        fd: RawFd,
        size: usize,
        flags: MsgFlags,
        timeout: Option<Duration>,
    ) -> io::Result<Source> {
        let hdr = unsafe { std::mem::zeroed::<libc::msghdr>() };
        let iov = libc::iovec {
            iov_base: std::ptr::null_mut(),
            iov_len: 0,
        };
        let source = self.new_source(
            fd,
            SourceType::SockRecvMsg(
                None,
                iov,
                hdr,
                std::mem::MaybeUninit::<nix::sys::socket::sockaddr_storage>::uninit(),
            ),
            None,
        );
        if let Some(timeout) = timeout {
            source.set_timeout(timeout);
        }
        self.sys.recvmsg(&source, size, flags);
        self.rush_dispatch(&source)?;
        Ok(source)
    }

    pub(crate) fn rushed_recv(
        &self,
        fd: RawFd,
        size: usize,
        flags: MsgFlags,
        timeout: Option<Duration>,
    ) -> io::Result<Source> {
        let source = self.new_source(fd, SourceType::SockRecv(None), None);
        if let Some(timeout) = timeout {
            source.set_timeout(timeout);
        }
        self.sys.recv(&source, size, flags);
        self.rush_dispatch(&source)?;
        Ok(source)
    }

    /// Arms a multishot receive on `fd`, or returns `None` when this
    /// executor has no receive buffers or the kernel refused the flag.
    /// `wanted` is how many bytes the owner can take at once, its own
    /// buffer; the request is cancelled once it has queued more than that.
    /// At least two kernel buffers, so a bundle can always land. The
    /// ceiling is checked as completions are reaped, so one visit to the
    /// ring can still overshoot by whatever the socket held, up to every
    /// free buffer; see [`sys::RecvMultiState::ceiling`].
    pub(crate) fn recv_multi(&self, fd: RawFd, wanted: usize) -> Option<Source> {
        let buffers = self.sys.recv_buffers()?;
        let bundle = self.sys.recv_multi_bundles();
        let ceiling = wanted.max(2 * buffers.borrow().size());
        let source = self.new_source(
            fd,
            SourceType::RecvMulti(sys::RecvMultiState::new(buffers, bundle, ceiling)),
            None,
        );
        self.sys.recv_multi(&source, bundle);
        Some(source)
    }

    /// The kernel answered a multishot receive with EINVAL. If it was asking
    /// for bundles, those are given up and the plain form is tried; if not,
    /// multishot itself is given up on this executor.
    pub(crate) fn refuse_recv_multi(&self, was_bundled: bool) {
        if was_bundled {
            self.sys.refuse_bundle()
        } else {
            self.sys.refuse_recv_multi()
        }
    }

    /// See [`sys::Reactor::note_recv_buffers_exhausted`].
    pub(crate) fn note_recv_buffers_exhausted(&self) {
        self.sys.note_recv_buffers_exhausted()
    }

    /// See [`sys::Reactor::reap_socket_completions`].
    pub(crate) fn reap_socket_completions(&self) -> io::Result<usize> {
        self.sys.reap_socket_completions()
    }

    /// A completion-based read into `buffer`, which stays in the `Source`
    /// until the read completes.
    pub(crate) fn recv_into(&self, fd: RawFd, buffer: crate::net::OwnedRxBuf) -> Source {
        let source = self.new_source(fd, SourceType::SockRecvInto(Some(buffer)), None);
        self.sys.recv_into(&source, MsgFlags::empty());
        source
    }

    pub(crate) fn recv(&self, fd: RawFd, size: usize, flags: MsgFlags) -> Source {
        let source = self.new_source(fd, SourceType::SockRecv(None), None);
        self.sys.recv(&source, size, flags);
        source
    }

    pub(crate) fn read_dma(
        &self,
        raw: RawFd,
        pos: u64,
        size: usize,
        pollable: PollableStatus,
        scheduler: Option<&FileScheduler>,
    ) -> ScheduledSource {
        const STATS: StatsCollection = StatsCollection {
            fulfilled: Some(|result, stats, op_count| {
                if let Ok(result) = result {
                    stats.file_reads += op_count;
                    stats.file_bytes_read += *result as u64 * op_count;
                }
            }),
            reused: Some(|result, stats, op_count| {
                if let Ok(result) = result {
                    stats.file_deduped_reads += op_count;
                    stats.file_deduped_bytes_read += *result as u64 * op_count;
                }
            }),
            latency: Some(|pre_lat, io_lat, post_lat, stats| {
                stats
                    .pre_reactor_io_scheduler_latency_us
                    .add(pre_lat.as_micros() as f64);
                stats.io_latency_us.add(io_lat.as_micros() as f64);
                stats
                    .post_reactor_io_scheduler_latency_us
                    .add(post_lat.as_micros() as f64)
            }),
        };
        const STATS_WITHOUT_LATENCY: StatsCollection = StatsCollection {
            latency: None,
            ..STATS
        };
        let stats = if self.record_io_latencies {
            &STATS
        } else {
            &STATS_WITHOUT_LATENCY
        };

        let size = size.min(MAX_RW_COUNT);
        let source = self.new_source(raw, SourceType::Read(pollable, None), Some(stats));

        if let Some(scheduler) = scheduler {
            if let Some(source) =
                scheduler.consume_scheduled(pos..pos.saturating_add(size as u64), Some(&self.sys))
            {
                source
            } else {
                self.sys.read_dma(&source, pos, size);
                scheduler.schedule(source, pos..pos.saturating_add(size as u64))
            }
        } else {
            self.sys.read_dma(&source, pos, size);
            ScheduledSource::new_raw(source, pos..pos.saturating_add(size as u64))
        }
    }

    pub(crate) fn read_buffered(
        &self,
        raw: RawFd,
        pos: u64,
        size: usize,
        scheduler: Option<&FileScheduler>,
    ) -> ScheduledSource {
        let size = size.min(MAX_RW_COUNT);
        let source = self.new_source(
            raw,
            SourceType::Read(PollableStatus::NonPollable(DirectIo::Disabled), None),
            Some(self.buffered_read_stats()),
        );

        if let Some(scheduler) = scheduler {
            if let Some(source) =
                scheduler.consume_scheduled(pos..pos.saturating_add(size as u64), Some(&self.sys))
            {
                source
            } else {
                self.sys.read_buffered(&source, pos, size);
                scheduler.schedule(source, pos..pos.saturating_add(size as u64))
            }
        } else {
            self.sys.read_buffered(&source, pos, size);
            ScheduledSource::new_raw(source, pos..pos.saturating_add(size as u64))
        }
    }

    /// Reads from wherever the descriptor's own offset is, and advances it, as
    /// `read(2)` does. For descriptors such as standard input that are read in
    /// sequence rather than by position: the ring takes an offset of -1 to mean
    /// the current one.
    pub(crate) fn read_buffered_at_cursor(&self, raw: RawFd, size: usize) -> ScheduledSource {
        let size = size.min(MAX_RW_COUNT);
        let source = self.new_source(
            raw,
            SourceType::Read(PollableStatus::NonPollable(DirectIo::Disabled), None),
            Some(self.buffered_read_stats()),
        );
        self.sys.read_buffered(&source, u64::MAX, size);
        ScheduledSource::new_raw(source, 0..size as u64)
    }

    fn buffered_read_stats(&self) -> &'static StatsCollection {
        const STATS: StatsCollection = StatsCollection {
            fulfilled: Some(|result, stats, op_count| {
                if let Ok(result) = result {
                    stats.file_buffered_reads += op_count;
                    stats.file_buffered_bytes_read += *result as u64 * op_count;
                }
            }),
            reused: None,
            latency: Some(|pre_lat, io_lat, post_lat, stats| {
                stats
                    .pre_reactor_io_scheduler_latency_us
                    .add(pre_lat.as_micros() as f64);
                stats.io_latency_us.add(io_lat.as_micros() as f64);
                stats
                    .post_reactor_io_scheduler_latency_us
                    .add(post_lat.as_micros() as f64)
            }),
        };
        const STATS_WITHOUT_LATENCY: StatsCollection = StatsCollection {
            latency: None,
            ..STATS
        };
        if self.record_io_latencies {
            &STATS
        } else {
            &STATS_WITHOUT_LATENCY
        }
    }

    pub(crate) fn fdatasync(&self, raw: RawFd) -> Source {
        #[cfg(test)]
        crate::sys::test_support::note_file_op("fdatasync");
        let source = self.new_source(raw, SourceType::FdataSync, None);
        self.sys.fdatasync(&source);
        source
    }

    pub(crate) fn fallocate(
        &self,
        raw: RawFd,
        position: u64,
        size: u64,
        flags: libc::c_int,
    ) -> Source {
        let source = self.new_source(raw, SourceType::Fallocate, None);
        self.sys.fallocate(&source, position, size, flags);
        source
    }

    pub(crate) fn truncate(&self, raw: RawFd, size: u64) -> impl Future<Output = Source> {
        #[cfg(test)]
        crate::sys::test_support::note_file_op("truncate");
        let source = self.new_source(raw, SourceType::Truncate, None);
        let waiter = self.sys.truncate(&source, size);

        async move {
            waiter.await;
            source
        }
    }

    pub(crate) fn rename(
        &self,
        old_path: NativePath,
        new_path: NativePath,
    ) -> impl Future<Output = Source> {
        let source = self.new_source(-1, SourceType::Rename(old_path, new_path), None);
        let waiter = self.sys.rename(&source);

        async move {
            waiter.await;
            source
        }
    }

    pub(crate) fn remove_file(&self, path: NativePath) -> impl Future<Output = Source> {
        let source = self.new_source(-1, SourceType::Remove(path), None);
        let waiter = self.sys.remove_file(&source);

        async move {
            waiter.await;
            source
        }
    }

    pub(crate) fn create_dir(
        &self,
        path: NativePath,
        mode: libc::c_int,
    ) -> impl Future<Output = Source> {
        let source = self.new_source(-1, SourceType::CreateDir(path), None);
        let waiter = self.sys.create_dir(&source, mode);

        async move {
            waiter.await;
            source
        }
    }

    pub(crate) fn run_blocking(
        &self,
        func: Box<dyn FnOnce() + Send + 'static>,
    ) -> impl Future<Output = Source> {
        let source = self.new_source(-1, SourceType::BlockingFn, None);
        let waiter = self.sys.run_blocking(&source, func);

        async move {
            waiter.await;
            source
        }
    }

    pub(crate) fn close(&self, raw: RawFd) -> Source {
        let source = self.new_source(
            raw,
            SourceType::Close,
            Some(&StatsCollection {
                fulfilled: Some(|result, stats, op_count| {
                    if result.is_ok() {
                        stats.files_closed += op_count
                    }
                }),
                reused: None,
                latency: None,
            }),
        );
        self.sys.close(&source);
        source
    }

    pub(crate) fn statx(&self, raw: RawFd) -> Source {
        let statx_buf = unsafe {
            let statx_buf = mem::MaybeUninit::<Statx>::zeroed();
            statx_buf.assume_init()
        };

        let source = self.new_source(
            raw,
            SourceType::Statx(Box::new(RefCell::new(statx_buf))),
            None,
        );
        self.sys.statx_fd(&source);
        source
    }

    pub(crate) fn open_at(
        &self,
        dir: RawFd,
        path: NativePath,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> Source {
        let source = self.new_source(
            dir,
            SourceType::Open(path),
            Some(&StatsCollection {
                fulfilled: Some(|result, stats, op_count| {
                    if result.is_ok() {
                        stats.files_opened += op_count
                    }
                }),
                reused: None,
                latency: None,
            }),
        );
        self.sys.open_at(&source, flags, mode);
        source
    }

    #[cfg(any(test, feature = "bench"))]
    pub(crate) fn nop(&self) -> Source {
        let source = self.new_source(-1, SourceType::Noop, None);
        self.sys.nop(&source);
        source
    }

    /// Registers a timer in the reactor.
    ///
    /// The returned handle stays valid for the timer's whole life, however
    /// many times it cascades between levels.
    ///
    /// A timer registered while a `Latency::Matters` queue runs is a latency
    /// timer, as I/O submitted then goes to the latency ring: its deadline
    /// preempts whatever holds the CPU when it comes due. A submission the
    /// kernel refuses while arming it is left for the next pass to report.
    ///
    /// A timer registered outside any queue's slice of this reactor's
    /// executor, such as by a waker the reactor runs in its pass, with no
    /// executor running on the thread, or during another executor's slice,
    /// would otherwise be classed by whichever queue ran last, and nothing
    /// tells whose task the waker wakes. So it is a latency timer if any
    /// latency queue exists: that costs a plain timer at most one preemption
    /// too many, where the other way a latency queue's timer would go
    /// unnoticed for a whole slice. A registration that cannot tell whether a
    /// slice runs is treated alike.
    pub(crate) fn insert_timer(&self, when: Instant, waker: Waker) -> TimerId {
        let latency = if crate::executor::slice_is_running_on(self) {
            matches!(
                self.io_scheduler.requirements().latency_req,
                Latency::Matters(_)
            )
        } else {
            self.latency_queues.get()
        };
        let id = self.timers.borrow_mut().insert(when, waker, latency);
        if latency {
            let _ = self.sys.hasten_latency_timer(when);
        }
        id
    }

    /// Deregisters a timer, handing back its waker if it had not fired.
    pub(crate) fn remove_timer(&self, id: TimerId) -> Option<Waker> {
        let mut timers = self.timers.borrow_mut();
        timers.remove(id)
    }

    /// Points a registered timer at `waker`, as
    /// [`ReactorTimers::update_waker`](crate::timer::reactor_adapter::ReactorTimers::update_waker)
    /// describes. The displaced waker is returned after the `timers` borrow
    /// has ended.
    pub(crate) fn update_timer_waker(&self, id: TimerId, waker: &Waker) -> Option<Option<Waker>> {
        let mut timers = self.timers.borrow_mut();
        timers.update_waker(id, waker)
    }

    /// Whether a handle still names a registered timer.
    pub(crate) fn timer_exists(&self, id: TimerId) -> bool {
        let timers = self.timers.borrow();
        timers.exists(id)
    }

    /// Wakes everything that has come due, and says how many that was.
    ///
    /// The wakers are collected under the `timers` borrow and woken after it
    /// ends, since one that arms or cancels a timer would otherwise re-enter
    /// and panic on the second borrow.
    fn process_timers(&self) -> usize {
        let mut scratch = std::mem::take(&mut *self.timer_wakers.borrow_mut());
        debug_assert!(
            scratch.is_empty(),
            "scratch is drained before it is returned"
        );

        self.timers.borrow_mut().process_timers(&mut scratch);

        let woke = scratch.len();
        for waker in scratch.drain(..) {
            wake!(waker);
        }
        *self.timer_wakers.borrow_mut() = scratch;

        woke
    }

    /// Releases the channel registry before local or foreign wake callbacks.
    fn process_shared_channels(&self) -> usize {
        let ready = self.shared_channels.borrow_mut().take_ready_wakers();
        let processed = ready.len();
        for waker in ready {
            wake!(waker);
        }
        processed + self.sys.process_foreign_wakes()
    }

    pub(crate) fn process_shared_channels_by_id(&self, id: u64) -> usize {
        let ready = self
            .shared_channels
            .borrow_mut()
            .pending
            .remove(&id)
            .unwrap_or_default();
        let processed = ready.len();
        for waker in ready {
            wake!(waker);
        }
        processed
    }

    pub(crate) fn rush_dispatch(&self, source: &Source) -> io::Result<()> {
        self.sys.rush_dispatch(source, &mut 0)
    }

    /// Polls for I/O, but does not change any timer registration.
    ///
    /// This doesn't ever sleep, and does not touch the preemption timer.
    pub(crate) fn spin_poll_io(&self) -> io::Result<bool> {
        let mut woke = 0;
        self.sys.poll_io(&mut woke)?;
        woke += self.process_timers();
        woke += self.process_shared_channels();

        Ok(woke > 0)
    }

    /// Wakes what is due, and says how long until the next timer is due,
    /// when the next latency timer is due and how long until then, and how
    /// many tasks were woken. The clock is read once for both, and not at all
    /// when no timer is armed.
    fn process_external_events(&self) -> (Option<Duration>, Option<(Instant, Duration)>, usize) {
        let mut woke = self.process_timers();
        woke += self.process_shared_channels();
        let (next, latency) = self.timers.borrow().next_expiries();
        let Some(next) = next else {
            return (None, None, woke);
        };
        let now = Instant::now();
        let until = |at: Instant| at.saturating_duration_since(now);
        (Some(until(next)), latency.map(|at| (at, until(at))), woke)
    }

    /// Processes new events, blocking until the first event or the timeout.
    ///
    /// A latency timer due within the `wait` itself fires there, and the
    /// wait drains its completion with everything else, so nothing would
    /// preempt the slice that follows for it. The timer it stands for is
    /// due then, so the pass is made again to wake its task before any
    /// queue runs. So is a pass in whose wait a waker registered a latency
    /// timer: the wait read the earliest deadline before running any waker,
    /// and the ring it held could not be armed for it. Each pass made again
    /// needs another such timer, so the passes end. A pass made again never
    /// parks: the one before it did not, and may have woken a task that
    /// the new pass would not count.
    pub(crate) fn react(
        &self,
        timeout: impl Fn() -> Option<Duration>,
        mut may_sleep: bool,
    ) -> io::Result<bool> {
        loop {
            self.sys.begin_pass();
            let (next_timer, latency_timer, woke) = self.process_external_events();

            match self
                .sys
                .wait(&timeout, next_timer, latency_timer, may_sleep, woke, || {
                    self.process_shared_channels()
                }) {
                Ok(true) => {
                    self.process_external_events();
                    return Ok(true);
                }

                Ok(false)
                    if self.sys.latency_timer_fired() || self.sys.take_deferred_latency_timer() =>
                {
                    may_sleep = false;
                    continue;
                }

                Ok(false) => return Ok(false),

                Err(err) => return Err(err),
            }
        }
    }

    pub(crate) fn io_scheduler(&self) -> &Rc<IoScheduler> {
        &self.io_scheduler
    }
}

impl fmt::Debug for Reactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("Reactor { .. }")
    }
}
