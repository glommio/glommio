//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2021 Datadog, Inc.
//!
use crate::{
    sys::SockAddrStorage,
    sys::{
        DmaBuffer, IoBuffer, OsResult, PollableStatus, ReactorQueue, SourceId, Statx, TimeSpec64,
        Wakers,
    },
    GlommioError, IoRequirements, ReactorErrorKind, RingIoStats, TaskQueueHandle,
};
use futures_lite::{future, io};
use std::{
    cell::{Ref, RefCell, RefMut},
    convert::TryFrom,
    ffi::CString,
    fmt,
    mem::MaybeUninit,
    os::unix::{
        ffi::OsStrExt,
        io::{FromRawFd, OwnedFd, RawFd},
    },
    path::Path,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
    time::Duration,
};

/// A path in the form this platform's syscalls take it.
///
/// On Unix that is a NUL-terminated byte string. Windows would want a
/// NUL-terminated `Vec<u16>`, which is why this is a type of its own rather
/// than a `CString` spelled out at every call site: the conversion from
/// `Path` happens once, here, and the rest of the runtime holds whatever the
/// kernel wants.
///
/// It is kept rather than converted at submission because io_uring is handed
/// a pointer and reads it when it runs the operation, which is after the call
/// that submitted it has returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativePath(CString);

impl NativePath {
    pub(crate) fn as_ptr(&self) -> *const libc::c_char {
        self.0.as_ptr()
    }
}

impl TryFrom<&Path> for NativePath {
    type Error = io::Error;

    /// Fails for a path holding an interior NUL, which cannot name a file.
    ///
    /// `EFAULT` is what the blocking pool answered for this before there was
    /// a conversion here at all, so the answer does not change with the path
    /// an operation takes.
    fn try_from(path: &Path) -> io::Result<Self> {
        CString::new(path.as_os_str().as_bytes())
            .map(NativePath)
            .map_err(|_| io::Error::from_raw_os_error(libc::EFAULT))
    }
}

#[derive(Debug)]
/// Clippy is unhappy with some of the fields on these enums never being read,
/// but they are certainly used, and read by debug
#[allow(dead_code)]
pub(crate) enum SourceType {
    Write(PollableStatus, IoBuffer),
    Read(PollableStatus, Option<IoBuffer>),
    PollAdd,
    SockSend(DmaBuffer),
    SockRecv(Option<DmaBuffer>),
    /// A read into a stream's own receive buffer, held here until the
    /// completion arrives so the stream can be dropped mid-read.
    SockRecvInto(Option<crate::net::OwnedRxBuf>),
    RecvMulti(RecvMultiState),
    SockRecvMsg(
        Option<DmaBuffer>,
        libc::iovec,
        libc::msghdr,
        MaybeUninit<nix::sys::socket::sockaddr_storage>,
    ),
    SockSendMsg(
        DmaBuffer,
        libc::iovec,
        libc::msghdr,
        nix::sys::socket::SockaddrStorage,
    ),
    Open(CString),
    FdataSync,
    Fallocate,
    Truncate,
    Close,
    LinkRings,
    ForeignNotifier(u64, bool),
    Statx(Box<RefCell<Statx>>),
    Timeout(TimeSpec64, u32),
    Connect(nix::sys::socket::SockaddrStorage),
    Accept(SockAddrStorage),
    Rename(NativePath, NativePath),
    CreateDir(NativePath),
    Remove(NativePath),
    BlockingFn,
    Invalid,
    CopyFileRange(RawFd, u64, usize),
    #[cfg(feature = "bench")]
    Noop,
}

impl SourceType {
    /// Whether a successful completion is a new file descriptor, which must
    /// be closed if nobody is left to take it.
    pub(crate) fn creates_fd(&self) -> bool {
        matches!(self, SourceType::Open(_) | SourceType::Accept(_))
    }
}

/// Closes a descriptor the kernel created for an operation whose owner is
/// gone. Nothing else can hold it: only `collect_fd` hands one out, and it
/// never ran.
pub(crate) fn close_orphaned_fd(fd: usize) {
    drop(unsafe { OwnedFd::from_raw_fd(fd as RawFd) });
}

/// One delivery of a multishot receive: `len` bytes spread over `bids` in
/// order, each buffer full but the last, of which the owner has already
/// taken `taken`. A plain multishot delivers one buffer per completion; a
/// bundled one as many as the socket had data for. Not `Clone`: the bids
/// are owed back to the ring exactly once.
#[derive(Debug)]
pub(crate) struct Arrival {
    pub(crate) bids: smallvec::SmallVec<[u16; 4]>,
    pub(crate) len: usize,
    pub(crate) taken: usize,
}

impl Arrival {
    /// Copies up to `out.len()` bytes starting `from` bytes into the
    /// arrival, across buffer boundaries, and returns how many.
    pub(crate) fn copy_to(
        &self,
        buffers: &crate::sys::ProvidedBuffers,
        from: usize,
        out: &mut [u8],
    ) -> usize {
        let size = buffers.size();
        let mut done = 0;
        let mut at = from;
        while at < self.len && done < out.len() {
            let (which, offset) = (at / size, at % size);
            let in_buffer = (self.len - at).min(size - offset).min(out.len() - done);
            let bytes = &buffers.bytes(self.bids[which], offset + in_buffer)[offset..];
            out[done..done + in_buffer].copy_from_slice(bytes);
            done += in_buffer;
            at += in_buffer;
        }
        done
    }
}

/// What a multishot receive has delivered and not yet handed over.
///
/// One request, many completions: each arrival lands in a buffer the kernel
/// picked from the executor's ring and is queued here as the buffer's id and
/// the byte count. The owner takes them in order and gives each buffer back.
/// Anything still queued when the state is dropped goes back by itself.
#[derive(Debug)]
pub(crate) struct RecvMultiState {
    pub(crate) ready: std::collections::VecDeque<Arrival>,
    /// Whether the kernel still holds the request. Cleared by the completion
    /// that carries no `MORE` flag, which is also the one that retires the
    /// source and stores its result.
    pub(crate) armed: bool,
    /// Whether the request was armed as a bundle, so a completion may span
    /// several buffers.
    pub(crate) bundled: bool,
    /// The most bytes the queue may hold before the request is cancelled,
    /// so a connection nobody reads cannot drain the executor's buffers for
    /// every other connection: once its queue is full the kernel stops
    /// taking its socket, the socket fills, and the peer's window closes,
    /// which is backpressure where it belongs. The owner re-arms when it
    /// drains the queue.
    pub(crate) ceiling: usize,
    /// Whether the request was cancelled for exceeding the ceiling; one
    /// cancellation per request.
    pub(crate) throttled: bool,
    pub(crate) buffers: Rc<RefCell<crate::sys::ProvidedBuffers>>,
}

impl RecvMultiState {
    pub(crate) fn new(
        buffers: Rc<RefCell<crate::sys::ProvidedBuffers>>,
        bundled: bool,
        ceiling: usize,
    ) -> Self {
        RecvMultiState {
            ready: Default::default(),
            armed: true,
            bundled,
            ceiling,
            throttled: false,
            buffers,
        }
    }

    /// Bytes queued and not yet taken by the owner.
    pub(crate) fn queued(&self) -> usize {
        self.ready.iter().map(|a| a.len - a.taken).sum()
    }
}

impl Drop for RecvMultiState {
    fn drop(&mut self) {
        if self.ready.is_empty() {
            return;
        }
        let mut buffers = self.buffers.borrow_mut();
        for arrival in self.ready.drain(..) {
            for bid in arrival.bids {
                buffers.give_back(bid);
            }
        }
        buffers.publish();
    }
}

impl TryFrom<SourceType> for Statx {
    type Error = GlommioError<()>;

    fn try_from(value: SourceType) -> Result<Self, Self::Error> {
        match value {
            SourceType::Statx(buf) => Ok(buf.into_inner()),
            src => Err(GlommioError::ReactorError(
                ReactorErrorKind::IncorrectSourceType(format!("{src:?}")),
            )),
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
pub(crate) enum EnqueuedStatus {
    Enqueued,
    Canceled,
    Dispatched,
}

#[derive(Clone)]
pub struct EnqueuedSource {
    pub(crate) id: SourceId,
    pub(crate) queue: ReactorQueue,
    pub(crate) status: EnqueuedStatus,
}

pub(crate) type StatsCollectionFn = fn(&io::Result<usize>, &mut RingIoStats, waiters: u64) -> ();
pub(crate) type LatencyCollectionFn =
    fn(std::time::Duration, std::time::Duration, std::time::Duration, &mut RingIoStats) -> ();

#[derive(Copy, Clone)]
pub(crate) struct StatsCollection {
    /// fulfilled runs when the source exits the reactor
    pub(crate) fulfilled: Option<StatsCollectionFn>,

    /// reused runs when a fulfilled source is reused by another consumer
    pub(crate) reused: Option<StatsCollectionFn>,

    /// Called when a result is first consumed. It collects the time
    /// it took for user code to consume the result of a fulfilled source.
    /// This is called once, even if the source is reused multiple time.
    pub(crate) latency: Option<LatencyCollectionFn>,
}

/// A registered source of I/O events.
pub(crate) struct InnerSource {
    /// Raw file descriptor on Unix platforms.
    pub(crate) raw: RawFd,

    /// Tasks interested in events on this source.
    pub(crate) wakers: Wakers,

    pub(crate) source_type: SourceType,

    /// Whether blocking I/O completion should wake a latency-sensitive executor.
    pub(crate) latency_sensitive: bool,

    pub(crate) timeout: Option<TimeSpec64>,

    pub(crate) enqueued: Option<EnqueuedSource>,

    pub(crate) stats_collection: Option<&'static StatsCollection>,

    pub(crate) task_queue: Option<TaskQueueHandle>,
}

impl InnerSource {
    pub(crate) fn update_source_type(&mut self, source_type: SourceType) -> SourceType {
        std::mem::replace(&mut self.source_type, source_type)
    }
}

impl fmt::Debug for InnerSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InnerSource")
            .field("raw", &self.raw)
            .field("wakers", &self.wakers)
            .field("source_type", &self.source_type)
            .field("latency_sensitive", &self.latency_sensitive)
            .finish()
    }
}

#[derive(Debug)]
pub struct Source {
    pub(crate) inner: Pin<Rc<RefCell<InnerSource>>>,
}

impl Source {
    /// Registers an I/O source in the reactor.
    pub(crate) fn new(
        ioreq: IoRequirements,
        raw: RawFd,
        source_type: SourceType,
        stats_collection: Option<&'static StatsCollection>,
        task_queue: Option<TaskQueueHandle>,
    ) -> Source {
        Source {
            inner: Rc::pin(RefCell::new(InnerSource {
                raw,
                wakers: Wakers::new(),
                source_type,
                latency_sensitive: matches!(ioreq.latency_req, crate::Latency::Matters(_)),
                enqueued: None,
                timeout: None,
                stats_collection,
                task_queue,
            })),
        }
    }

    pub(crate) fn set_timeout(&self, d: Duration) -> Option<Duration> {
        let mut inner = self.inner.borrow_mut();
        let t = &mut inner.timeout;
        let old = *t;
        match TimeSpec64::try_from(d) {
            Ok(dur) | Err(dur) => *t = Some(dur),
        }
        old.map(Duration::from)
    }

    pub(super) fn timeout_ref(&self) -> Ref<'_, Option<TimeSpec64>> {
        Ref::map(self.inner.borrow(), |x| &x.timeout)
    }

    pub(crate) fn source_type(&self) -> Ref<'_, SourceType> {
        Ref::map(self.inner.borrow(), |x| &x.source_type)
    }

    pub(crate) fn source_type_mut(&self) -> RefMut<'_, SourceType> {
        RefMut::map(self.inner.borrow_mut(), |x| &mut x.source_type)
    }

    pub(crate) fn extract_source_type(self) -> SourceType {
        self.inner
            .borrow_mut()
            .update_source_type(SourceType::Invalid)
    }

    pub(crate) fn extract_buffer(self) -> IoBuffer {
        let stype = self.extract_source_type();
        match stype {
            SourceType::Read(_, Some(buffer)) => buffer,
            SourceType::Write(_, buffer) => buffer,
            x => panic!("Could not extract buffer. Source: {:?}", x),
        }
    }

    pub(crate) fn buffer(&self) -> Ref<'_, IoBuffer> {
        Ref::map(self.source_type(), |stype| match stype {
            SourceType::Read(_, Some(buffer)) => buffer,
            SourceType::Write(_, buffer) => buffer,
            x => panic!("Could not extract buffer. Source: {:?}", x),
        })
    }

    /// Returns the result of this I/O source, if it has completed.
    ///
    /// If a scheduler latency collection function is present, invoke it once.
    pub(crate) fn result(&self) -> Option<io::Result<usize>> {
        let mut inner = self.inner.borrow_mut();
        let ret = inner
            .wakers
            .result
            .as_ref()
            .map(|x| OsResult::from(x).into());
        if ret.is_none() {
            return ret;
        }

        if let Some(Some(stat_fn)) = inner.stats_collection.as_ref().map(|x| x.latency) {
            if let Some(lat) = inner.wakers.timestamps() {
                drop(inner);
                let pre_lat = lat.submitted_at - lat.queued_at;
                let io_lat = lat.fulfilled_at - lat.submitted_at;
                let post_lat = lat.fulfilled_at.elapsed();

                let reactor = &crate::executor().reactor().sys;
                (stat_fn)(
                    pre_lat,
                    io_lat,
                    post_lat,
                    reactor.ring_for_source(self).io_stats_mut(),
                );
                (stat_fn)(
                    pre_lat,
                    io_lat,
                    post_lat,
                    reactor
                        .ring_for_source(self)
                        .io_stats_for_task_queue_mut(crate::executor().current_task_queue()),
                );
            }
        }

        ret
    }

    /// Adds a single waiter to the list, replacing any waiter that may already
    /// exist. Should be used for single streams that map a future 1:1 to their I/O
    /// source
    ///
    /// The list can hold several waiters when the source is also waited on
    /// through [`add_waiter_unique`](Self::add_waiter_unique), by futures
    /// that only borrow the stream. A caller here has the stream to itself,
    /// so those futures were dropped before it got it, and their wakers go.
    pub(crate) fn add_waiter_single(&self, waker: &Waker) {
        let mut inner = self.inner.borrow_mut();
        let waiters = &mut inner.wakers.waiters;
        if waiters.len() == 1 && waiters[0].will_wake(waker) {
            return;
        }
        waiters.clear();
        waiters.push(waker.clone());
    }

    /// Adds a waiter unless one in the list already wakes the same task. For
    /// a source several tasks can wait on at once through a shared borrow,
    /// where each of them must be woken, and a future polled again must not
    /// add itself twice.
    pub(crate) fn add_waiter_unique(&self, waker: &Waker) {
        let mut inner = self.inner.borrow_mut();
        let waiters = &mut inner.wakers.waiters;
        if !waiters.iter().any(|w| w.will_wake(waker)) {
            waiters.push(waker.clone());
        }
    }

    /// Adds a waiter to the list. Useful for streams that have many futures waiting
    /// on a single I/O source
    pub(crate) fn add_waiter_many(&self, waker: Waker) {
        self.inner.borrow_mut().wakers.waiters.push(waker)
    }

    pub(super) fn is_installed(&self) -> Option<bool> {
        match &self.inner.borrow().source_type {
            SourceType::ForeignNotifier(_, installed) => Some(*installed),
            _ => None,
        }
    }

    pub(super) fn raw(&self) -> RawFd {
        self.inner.borrow().raw
    }

    pub(crate) fn stats_collection(&self) -> Option<&'static StatsCollection> {
        self.inner.borrow().stats_collection
    }

    pub(crate) async fn collect_rw(&self) -> io::Result<usize> {
        future::poll_fn(|cx| self.poll_collect_rw(cx)).await
    }

    /// Like [`collect_rw`](Self::collect_rw), for an operation that creates
    /// a file descriptor: the caller owns the one returned, and a source
    /// dropped without returning one closes it.
    pub(crate) async fn collect_fd(&self) -> io::Result<RawFd> {
        future::poll_fn(|cx| self.poll_collect_fd(cx)).await
    }

    /// Poll version of [`collect_fd`](Self::collect_fd).
    pub(crate) fn poll_collect_fd(&self, cx: &mut Context<'_>) -> Poll<io::Result<RawFd>> {
        let result = futures_lite::ready!(self.poll_collect_rw(cx));
        self.inner.borrow_mut().wakers.result = None;
        Poll::Ready(result.map(|fd| fd as RawFd))
    }

    pub(crate) fn poll_collect_rw(&self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        if let Some(result) = self.result() {
            return Poll::Ready(result);
        }

        self.add_waiter_many(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for Source {
    /// Drops the source and consumes its enqueue status:
    ///
    /// * If it was never submitted (`Enqueued`), it is safe to consume the source
    ///   here -- the kernel didn't see our buffers. Consuming the source signals
    ///   to the submit method that we are no longer interested in submitting
    ///   this. This should be cheaper than removing elements from the vector all
    ///   the time.
    /// * If it was dispatched (`Dispatched`) but is being cancelled, the kernel
    ///   might be using the buffers right now, so `consume_source` is delayed
    ///   until the corresponding event is consumed from the completion queue.
    ///   Marking it `Canceled` is not necessary, but useful for correctness.
    /// * A close is never cancelled: the descriptor is no longer owned by
    ///   anything else, so this is the last chance to close it.
    /// * A completed operation that created a descriptor nobody collected
    ///   closes it.
    fn drop(&mut self) {
        let mut inner = self.inner.borrow_mut();
        let InnerSource {
            enqueued,
            source_type,
            wakers,
            ..
        } = &mut *inner;
        match enqueued.as_mut() {
            Some(_) if matches!(source_type, SourceType::Close) => {}
            Some(EnqueuedSource { id, queue, status }) => match status {
                EnqueuedStatus::Enqueued => {
                    *status = EnqueuedStatus::Canceled;
                }
                EnqueuedStatus::Dispatched => {
                    queue.borrow_mut().cancel_request(*id);
                    *status = EnqueuedStatus::Canceled;
                }
                EnqueuedStatus::Canceled => unreachable!(),
            },
            None if source_type.creates_fd() => {
                if let Some(Ok(fd)) = wakers.result.take() {
                    close_orphaned_fd(fd);
                }
            }
            None => {}
        }
    }
}
