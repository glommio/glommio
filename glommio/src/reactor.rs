//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
//!

use std::{
    cell::RefCell,
    collections::BTreeMap,
    ffi::CString,
    fmt,
    future::Future,
    io, mem,
    os::unix::{ffi::OsStrExt, io::RawFd},
    path::Path,
    rc::Rc,
    sync::Arc,
    task::Waker,
    time::{Duration, Instant},
};

use io_uring::CompletionStatus;
use nix::sys::socket::{MsgFlags, SockaddrLike, SockaddrStorage};
use smallvec::SmallVec;

use crate::{
    io::{FileScheduler, IoScheduler, ScheduledSource},
    sys::SockAddrStorage,
    sys::{
        self, blocking::BlockingThreadPool, common_flags, read_flags, DirectIo, DmaBuffer,
        DmaSource, IoBuffer, PollableStatus, SleepNotifier, Source, SourceType, StatsCollection,
        Statx,
    },
    IoRequirements, IoStats, TaskQueueHandle,
};
use nix::poll::PollFlags;

type SharedChannelWakerChecker = (SmallVec<[Waker; 1]>, Option<Box<dyn Fn() -> usize>>);

use crate::timer::slab::TimerId;

struct SharedChannels {
    id: u64,
    wakers_map: BTreeMap<u64, SharedChannelWakerChecker>,
    connection_wakers: Vec<Waker>,
}

impl SharedChannels {
    fn new() -> SharedChannels {
        SharedChannels {
            id: 0,
            connection_wakers: Vec::new(),
            wakers_map: BTreeMap::new(),
        }
    }

    fn process_shared_channels(&mut self) -> usize {
        let mut woke = self.connection_wakers.len();
        for waker in self.connection_wakers.drain(..) {
            wake!(waker);
        }

        for (_, (pending, check)) in self.wakers_map.iter_mut() {
            if pending.is_empty() {
                continue;
            }
            let room = std::cmp::min(check.as_ref().unwrap()(), pending.len());
            for waker in pending.drain(0..room).rev() {
                woke += 1;
                wake!(waker);
            }
        }
        woke
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

    fn insert(&mut self, when: Instant, waker: Waker) -> TimerId {
        self.wheel.insert(when, waker)
    }

    fn remove(&mut self, id: TimerId) -> Option<Waker> {
        self.wheel.remove(id)
    }

    fn exists(&self, id: TimerId) -> bool {
        self.wheel.contains(id)
    }

    /// Return the duration until the next event, appending the wakers of
    /// everything that has come due.
    fn process_timers(&mut self, wakers: &mut Vec<Waker>) -> Option<Duration> {
        self.wheel.process_timers(wakers)
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
}

impl Reactor {
    pub(crate) fn new(
        notifier: Arc<SleepNotifier>,
        io_memory: usize,
        ring_depth: usize,
        record_io_latencies: bool,
        blocking_thread: BlockingThreadPool,
    ) -> io::Result<Reactor> {
        let sys = sys::Reactor::new(notifier, io_memory, ring_depth, blocking_thread)?;
        let preempt_status = sys.preempt_status();
        Ok(Reactor {
            sys,
            timers: RefCell::new(Timers::new()),
            timer_wakers: RefCell::new(Vec::new()),
            shared_channels: RefCell::new(SharedChannels::new()),
            io_scheduler: Rc::new(IoScheduler::new()),
            record_io_latencies,
            preempt_status,
        })
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
        stats_collection: Option<StatsCollection>,
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

    pub(crate) fn register_shared_channel<F>(&self, test_function: Box<F>) -> u64
    where
        F: Fn() -> usize + 'static,
    {
        let mut channels = self.shared_channels.borrow_mut();
        let id = channels.id;
        channels.id += 1;
        let ret = channels
            .wakers_map
            .insert(id, (Default::default(), Some(test_function)));
        assert!(ret.is_none());
        id
    }

    pub(crate) fn unregister_shared_channel(&self, id: u64) {
        let mut channels = self.shared_channels.borrow_mut();
        channels.wakers_map.remove(&id);
    }

    pub(crate) fn add_shared_channel_connection_waker(&self, waker: Waker) {
        let mut channels = self.shared_channels.borrow_mut();
        channels.connection_wakers.push(waker);
    }

    pub(crate) fn add_shared_channel_waker(&self, id: u64, waker: Waker) {
        let mut channels = self.shared_channels.borrow_mut();
        let map = channels
            .wakers_map
            .entry(id)
            .or_insert_with(|| (SmallVec::new(), None));

        map.0.push(waker);
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
        let stats = StatsCollection {
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
            Some(stats),
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
        let stats = StatsCollection {
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
            Some(stats),
        );
        let waiter = self.sys.copy_file_range(&source, off_out);
        async move {
            waiter.await;
            source
        }
    }

    pub(crate) fn write_buffered(&self, raw: RawFd, buf: Vec<u8>, pos: u64) -> Source {
        let stats = StatsCollection {
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
            Some(stats),
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
        self.sys.send(&source, MsgFlags::empty());
        self.rush_dispatch(&source)?;
        Ok(source)
    }

    pub(crate) fn rushed_sendmsg(
        &self,
        fd: RawFd,
        buf: DmaBuffer,
        addr: impl nix::sys::socket::SockaddrLike,
        timeout: Option<Duration>,
    ) -> io::Result<Source> {
        let iov = libc::iovec {
            iov_base: buf.as_ptr() as *mut libc::c_void,
            iov_len: 1,
        };
        // Note that the iov and addresses we have above are stack addresses. We will
        // leave it blank and the `io_uring` callee will fill that up
        let hdr = unsafe { std::mem::zeroed::<libc::msghdr>() };

        let addr = unsafe { SockaddrStorage::from_raw(addr.as_ptr(), Some(addr.len())) }.unwrap();
        let source = self.new_source(fd, SourceType::SockSendMsg(buf, iov, hdr, addr), None);
        if let Some(timeout) = timeout {
            source.set_timeout(timeout);
        }

        self.sys.sendmsg(&source, MsgFlags::empty());
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
        timeout: Option<Duration>,
    ) -> io::Result<Source> {
        let source = self.new_source(fd, SourceType::SockRecv(None), None);
        if let Some(timeout) = timeout {
            source.set_timeout(timeout);
        }
        self.sys.recv(&source, size, MsgFlags::empty());
        self.rush_dispatch(&source)?;
        Ok(source)
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
        let stats = StatsCollection {
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
            latency: if self.record_io_latencies {
                Some(|pre_lat, io_lat, post_lat, stats| {
                    stats
                        .pre_reactor_io_scheduler_latency_us
                        .add(pre_lat.as_micros() as f64);
                    stats.io_latency_us.add(io_lat.as_micros() as f64);
                    stats
                        .post_reactor_io_scheduler_latency_us
                        .add(post_lat.as_micros() as f64)
                })
            } else {
                None
            },
        };

        let source = self.new_source(raw, SourceType::Read(pollable, None), Some(stats));

        if let Some(scheduler) = scheduler {
            if let Some(source) =
                scheduler.consume_scheduled(pos..pos + size as u64, Some(&self.sys))
            {
                source
            } else {
                self.sys.read_dma(&source, pos, size);
                scheduler.schedule(source, pos..pos + size as u64)
            }
        } else {
            self.sys.read_dma(&source, pos, size);
            ScheduledSource::new_raw(source, pos..pos + size as u64)
        }
    }

    pub(crate) fn read_buffered(
        &self,
        raw: RawFd,
        pos: u64,
        size: usize,
        scheduler: Option<&FileScheduler>,
    ) -> ScheduledSource {
        let stats = StatsCollection {
            fulfilled: Some(|result, stats, op_count| {
                if let Ok(result) = result {
                    stats.file_buffered_reads += op_count;
                    stats.file_buffered_bytes_read += *result as u64 * op_count;
                }
            }),
            reused: None,
            latency: if self.record_io_latencies {
                Some(|pre_lat, io_lat, post_lat, stats| {
                    stats
                        .pre_reactor_io_scheduler_latency_us
                        .add(pre_lat.as_micros() as f64);
                    stats.io_latency_us.add(io_lat.as_micros() as f64);
                    stats
                        .post_reactor_io_scheduler_latency_us
                        .add(post_lat.as_micros() as f64)
                })
            } else {
                None
            },
        };

        let source = self.new_source(
            raw,
            SourceType::Read(PollableStatus::NonPollable(DirectIo::Disabled), None),
            Some(stats),
        );

        if let Some(scheduler) = scheduler {
            if let Some(source) =
                scheduler.consume_scheduled(pos..pos + size as u64, Some(&self.sys))
            {
                source
            } else {
                self.sys.read_buffered(&source, pos, size);
                scheduler.schedule(source, pos..pos + size as u64)
            }
        } else {
            self.sys.read_buffered(&source, pos, size);
            ScheduledSource::new_raw(source, pos..pos + size as u64)
        }
    }

    pub(crate) fn fdatasync(&self, raw: RawFd) -> Source {
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
        let source = self.new_source(raw, SourceType::Truncate, None);
        let waiter = self.sys.truncate(&source, size);

        async move {
            waiter.await;
            source
        }
    }

    pub(crate) fn rename<P, Q>(&self, old_path: P, new_path: Q) -> impl Future<Output = Source>
    where
        P: AsRef<Path>,
        Q: AsRef<Path>,
    {
        let source = self.new_source(
            -1,
            SourceType::Rename(old_path.as_ref().to_owned(), new_path.as_ref().to_owned()),
            None,
        );
        let waiter = self.sys.rename(&source);

        async move {
            waiter.await;
            source
        }
    }

    pub(crate) fn remove_file<P: AsRef<Path>>(&self, path: P) -> impl Future<Output = Source> {
        let source = self.new_source(-1, SourceType::Remove(path.as_ref().to_owned()), None);
        let waiter = self.sys.remove_file(&source);

        async move {
            waiter.await;
            source
        }
    }

    pub(crate) fn create_dir<P: AsRef<Path>>(
        &self,
        path: P,
        mode: libc::c_int,
    ) -> impl Future<Output = Source> {
        let source = self.new_source(-1, SourceType::CreateDir(path.as_ref().to_owned()), None);
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
            Some(StatsCollection {
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
        path: &Path,
        flags: libc::c_int,
        mode: libc::mode_t,
    ) -> Source {
        let path = CString::new(path.as_os_str().as_bytes()).expect("path contained null!");

        let source = self.new_source(
            dir,
            SourceType::Open(path),
            Some(StatsCollection {
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

    #[cfg(feature = "bench")]
    pub(crate) fn nop(&self) -> Source {
        let source = self.new_source(-1, SourceType::Noop, None);
        self.sys.nop(&source);
        source
    }

    /// Registers a timer in the reactor.
    ///
    /// The returned handle stays valid for the timer's whole life, however
    /// many times it cascades between levels.
    pub(crate) fn insert_timer(&self, when: Instant, waker: Waker) -> TimerId {
        let mut timers = self.timers.borrow_mut();
        timers.insert(when, waker)
    }

    /// Deregisters a timer, handing back its waker if it had not fired.
    pub(crate) fn remove_timer(&self, id: TimerId) -> Option<Waker> {
        let mut timers = self.timers.borrow_mut();
        timers.remove(id)
    }

    /// Whether a handle still names a registered timer.
    pub(crate) fn timer_exists(&self, id: TimerId) -> bool {
        let timers = self.timers.borrow();
        timers.exists(id)
    }

    /// Processes ready timers and extends the list of wakers to wake.
    ///
    /// Returns the duration until the next timer
    fn process_timers(&self) -> (Option<Duration>, usize) {
        // Collect first, wake after. `timers` is a RefCell, so a waker that
        // arms or cancels a timer while we still hold it re-enters and panics
        // on the second borrow. The scratch buffer is kept across calls so a
        // batch of expiries costs no allocation.
        let mut scratch = std::mem::take(&mut *self.timer_wakers.borrow_mut());
        debug_assert!(
            scratch.is_empty(),
            "scratch is drained before it is returned"
        );

        let next = {
            let mut timers = self.timers.borrow_mut();
            timers.process_timers(&mut scratch)
        };

        let woke = scratch.len();
        for waker in scratch.drain(..) {
            wake!(waker);
        }
        *self.timer_wakers.borrow_mut() = scratch;

        (next, woke)
    }

    fn process_shared_channels(&self) -> usize {
        let mut channels = self.shared_channels.borrow_mut();
        let mut processed = channels.process_shared_channels();
        processed += self.sys.process_foreign_wakes();
        processed
    }

    pub(crate) fn process_shared_channels_by_id(&self, id: u64) -> usize {
        match self.shared_channels.borrow_mut().wakers_map.get_mut(&id) {
            Some(wakers) => {
                let processed = wakers.0.len();
                wakers.0.drain(..).for_each(|w| {
                    wake!(w);
                });
                processed
            }
            None => 0,
        }
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
        woke += self.process_timers().1;
        woke += self.process_shared_channels();

        Ok(woke > 0)
    }

    fn process_external_events(&self) -> (Option<Duration>, usize) {
        let (next_timer, mut woke) = self.process_timers();
        woke += self.process_shared_channels();
        (next_timer, woke)
    }

    /// Processes new events, blocking until the first event or the timeout.
    pub(crate) fn react(&self, timeout: impl Fn() -> Option<Duration>) -> io::Result<bool> {
        // Process ready timers.
        let (next_timer, woke) = self.process_external_events();

        // Block on I/O events.
        match self
            .sys
            .wait(timeout, next_timer, woke, || self.process_shared_channels())
        {
            // Don't wait for the next loop to process timers or shared channels
            Ok(true) => {
                self.process_external_events();
                Ok(true)
            }

            Ok(false) => Ok(false),

            // An actual error occurred.
            Err(err) => Err(err),
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
