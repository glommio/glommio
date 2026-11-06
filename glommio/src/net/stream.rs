//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
use crate::{
    reactor::Reactor,
    sys::{self, Source, SourceType},
};
use futures_lite::ready;
use nix::sys::socket::MsgFlags;
use std::{
    cell::Cell,
    io::{self, IoSlice},
    net::Shutdown,
    os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
    rc::{Rc, Weak},
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

use crate::Result;

/// A receive buffer on loan to the kernel.
///
/// It lives in the `Source` until the completion arrives, so the read can
/// outlive the future that started it.
#[derive(Debug)]
pub struct OwnedRxBuf {
    memory: Vec<u8>,
}

impl OwnedRxBuf {
    pub(crate) fn new(memory: Vec<u8>) -> Self {
        OwnedRxBuf { memory }
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut u8 {
        self.memory.as_mut_ptr()
    }

    pub(crate) fn len(&self) -> usize {
        self.memory.len()
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.memory
    }
}

/// Root trait for socket stream receive buffer
pub trait RxBuf {
    fn read(&mut self, buf: &mut [u8]) -> usize;
    fn peek(&self, buf: &mut [u8]) -> usize;
    fn is_empty(&self) -> bool;
    fn as_bytes(&self) -> &[u8];
    fn consume(&mut self, amt: usize);
    fn buffer_size(&self) -> usize;
    fn handle_result(&mut self, result: usize);
    fn unfilled(&mut self) -> &mut [u8];

    /// Lends the whole buffer to the kernel for a completion-based read.
    ///
    /// Called only when the buffer is empty. `None`, the default, keeps the
    /// readiness path; returning `Some` requires
    /// [`restore_kernel_buffer`](Self::restore_kernel_buffer) as well.
    fn take_kernel_buffer(&mut self) -> Option<OwnedRxBuf> {
        None
    }

    /// Takes the buffer back, empty. The byte count arrives separately,
    /// through [`handle_result`](Self::handle_result).
    fn restore_kernel_buffer(&mut self, buffer: OwnedRxBuf) {
        let _ = buffer;
        unreachable!(
            "restore_kernel_buffer called on an RxBuf that never lends its buffer out: \
             take_kernel_buffer and restore_kernel_buffer must be implemented together"
        )
    }
}

#[derive(Debug, Default)]
pub struct NonBuffered;

impl RxBuf for NonBuffered {
    fn read(&mut self, _buf: &mut [u8]) -> usize {
        0
    }

    fn peek(&self, _buf: &mut [u8]) -> usize {
        0
    }

    fn is_empty(&self) -> bool {
        true
    }

    fn as_bytes(&self) -> &[u8] {
        &[]
    }

    fn consume(&mut self, _amt: usize) {}

    fn buffer_size(&self) -> usize {
        0
    }

    fn handle_result(&mut self, _result: usize) {}

    fn unfilled(&mut self) -> &mut [u8] {
        &mut []
    }
}

/// Trait for receive buffer implementations
pub trait Buffered: RxBuf {}

/// Non-shared fixed sized receive buffer allocated
/// when buffered stream is created
#[derive(Debug)]
pub struct Preallocated {
    buf: Vec<u8>,
    head: usize,
    tail: usize,
    cap: usize,
}

impl Preallocated {
    pub(crate) const DEFAULT_BUFFER_SIZE: usize = 8192;

    /// Creates a fixed sized receive buffer
    pub fn new(size: usize) -> Self {
        Self {
            buf: vec![0; size],
            tail: 0,
            head: 0,
            cap: size,
        }
    }
}

impl Default for Preallocated {
    fn default() -> Self {
        Self::new(Self::DEFAULT_BUFFER_SIZE)
    }
}

impl Preallocated {
    fn len(&self) -> usize {
        self.tail - self.head
    }
}

impl Buffered for Preallocated {}

impl RxBuf for Preallocated {
    fn read(&mut self, buf: &mut [u8]) -> usize {
        let sz = std::cmp::min(self.len(), buf.len());
        if sz > 0 {
            buf[..sz].copy_from_slice(&self.buf[self.head..self.head + sz]);
            self.head += sz;
        }
        sz
    }

    fn peek(&self, buf: &mut [u8]) -> usize {
        let sz = std::cmp::min(self.len(), buf.len());
        if sz > 0 {
            buf[..sz].copy_from_slice(&self.buf[self.head..self.head + sz]);
        }
        sz
    }

    fn is_empty(&self) -> bool {
        self.head >= self.tail
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[self.head..self.tail]
    }

    fn consume(&mut self, amt: usize) {
        self.head += std::cmp::min(self.len(), amt);
    }

    fn buffer_size(&self) -> usize {
        self.cap
    }

    fn handle_result(&mut self, result: usize) {
        self.tail += result;
    }

    fn unfilled(&mut self) -> &mut [u8] {
        if self.len() == 0 {
            self.head = 0;
            self.tail = 0;
        }
        &mut self.buf[self.tail..]
    }

    fn take_kernel_buffer(&mut self) -> Option<OwnedRxBuf> {
        debug_assert!(
            self.is_empty(),
            "lending a buffer that still holds unread bytes would lose them"
        );
        self.head = 0;
        self.tail = 0;
        Some(OwnedRxBuf::new(std::mem::take(&mut self.buf)))
    }

    fn restore_kernel_buffer(&mut self, buffer: OwnedRxBuf) {
        self.buf = buffer.memory;
        self.head = 0;
        self.tail = 0;
    }
}

#[derive(Debug)]
struct Timeout {
    id: u64,
    timeout: Cell<Option<Duration>>,
    timer: Cell<Option<Instant>>,
}

impl Timeout {
    fn new(id: u64) -> Self {
        Self {
            id,
            timeout: Cell::new(None),
            timer: Cell::new(None),
        }
    }

    fn get(&self) -> Option<Duration> {
        self.timeout.get()
    }

    fn set(&self, dur: Option<Duration>) -> Result<()> {
        if let Some(dur) = dur.as_ref() {
            if dur.as_nanos() == 0 {
                return Err(io::Error::from_raw_os_error(libc::EINVAL).into());
            }
        }
        self.timeout.set(dur);
        Ok(())
    }

    fn maybe_set_timer(&self, reactor: &Reactor, waker: &Waker) {
        if let Some(timeout) = self.timeout.get() {
            if self.timer.get().is_none() {
                let deadline = Instant::now() + timeout;
                reactor.insert_timer(self.id, deadline, waker.clone());
                self.timer.set(Some(deadline));
            }
        }
    }

    fn cancel_timer(&self, reactor: &Reactor) {
        if self.timer.take().is_some() {
            reactor.remove_timer(self.id);
        }
    }

    fn check(&self, reactor: &Reactor) -> io::Result<()> {
        if let Some(deadline) = self.timer.get() {
            if !reactor.timer_exists(&(deadline, self.id)) {
                reactor.remove_timer(self.id);
                self.timer.take();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Operation timed out",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct NonBufferedStream<S> {
    reactor: Weak<Reactor>,
    stream: S,
    source_tx: Option<Source>,
    source_rx: Option<Source>,
    write_timeout: Timeout,
    read_timeout: Timeout,
}

impl<S: AsRawFd> NonBufferedStream<S> {
    fn init(&mut self) {
        let reactor = self.reactor.upgrade().unwrap();
        let stream_fd = self.stream.as_raw_fd();
        self.source_rx = Some(reactor.poll_read_ready(stream_fd));
    }

    pub(crate) fn try_peek(&self, buf: &mut [u8]) -> Option<io::Result<usize>> {
        super::yolo_peek(self.stream.as_raw_fd(), buf)
    }

    pub(crate) async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        let source = self.reactor.upgrade().unwrap().recv(
            self.stream.as_raw_fd(),
            buf.len(),
            MsgFlags::MSG_PEEK,
        );

        let sz = source.collect_rw().await?;
        match source.extract_source_type() {
            SourceType::SockRecv(mut src) => {
                buf[0..sz].copy_from_slice(&src.take().unwrap().as_bytes()[0..sz]);
            }
            _ => unreachable!(),
        }
        Ok(sz)
    }

    /// One non-blocking `recv`; `None` when the socket has nothing.
    pub(crate) fn poll_read_speculative(&mut self, buf: &mut [u8]) -> Option<io::Result<usize>> {
        super::yolo_recv(self.stream.as_raw_fd(), buf)
    }

    /// Speculates with a plain `recv`, and registers for readiness only when
    /// that comes back empty.
    ///
    /// Nothing is registered after a read that succeeded. Doing so used to be
    /// a bet that the next read would have to wait, so arming now would save
    /// arming then. It does not pay: the saving is one `recv` returning
    /// `EAGAIN`, and the price is an SQE, a kernel enter and a completion on
    /// every short read. The syscall is the cheaper of the two.
    pub(crate) fn poll_read(
        &mut self,
        cx: &Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let reactor = self.reactor.upgrade().unwrap();
        let reactor = reactor.as_ref();

        let no_pending_poll = self
            .source_rx
            .as_ref()
            .map(|src| src.result().is_some())
            .unwrap_or(true);

        if no_pending_poll {
            if let Some(result) = super::yolo_recv(self.stream.as_raw_fd(), buf) {
                self.source_rx.take();
                self.read_timeout.cancel_timer(reactor);
                let result = poll_err!(result);
                return Poll::Ready(Ok(result));
            }
        }

        poll_err!(self.read_timeout.check(reactor));

        if no_pending_poll {
            self.source_rx = Some(reactor.poll_read_ready(self.stream.as_raw_fd()));
        }

        let source = self.source_rx.as_ref().unwrap();
        source.add_waiter_single(cx.waker());
        self.read_timeout.maybe_set_timer(reactor, cx.waker());
        Poll::Pending
    }

    /// Writes several buffers in one syscall.
    pub(crate) fn poll_write_vectored(
        &mut self,
        cx: &Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        if let Some(result) = super::yolo_sendv(self.stream.as_raw_fd(), bufs) {
            let reactor = self.reactor.upgrade().unwrap();
            self.write_timeout.cancel_timer(reactor.as_ref());
            self.source_tx.take();
            return Poll::Ready(result);
        }

        self.poll_write_ready(cx)
    }

    /// Registers interest in the socket becoming writable, shared by the
    /// scalar and vectored paths.
    fn poll_write_ready(&mut self, cx: &Context<'_>) -> Poll<io::Result<usize>> {
        let reactor = self.reactor.upgrade().unwrap();
        let reactor = reactor.as_ref();
        poll_err!(self.write_timeout.check(reactor));

        let no_pending_poll = self
            .source_tx
            .as_ref()
            .map(|src| src.result().is_some())
            .unwrap_or(true);

        if no_pending_poll {
            self.source_tx = Some(reactor.poll_write_ready(self.stream.as_raw_fd()));
        }

        let source = self.source_tx.as_ref().unwrap();
        source.add_waiter_single(cx.waker());
        self.write_timeout.maybe_set_timer(reactor, cx.waker());
        Poll::Pending
    }

    /// Writes one buffer.
    ///
    /// Starts with `yolo_send`, because it is very likely to succeed. That
    /// could be a waste if the write already timed out since the last call,
    /// but giving it one last chance costs little.
    pub(crate) fn poll_write(&mut self, cx: &Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        if let Some(result) = super::yolo_send(self.stream.as_raw_fd(), buf) {
            let reactor = self.reactor.upgrade().unwrap();
            self.write_timeout.cancel_timer(reactor.as_ref());
            self.source_tx.take();
            return Poll::Ready(result);
        }

        self.poll_write_ready(cx)
    }

    pub(crate) fn poll_close(&mut self, _: &Context<'_>) -> Poll<io::Result<()>> {
        self.source_tx.take();
        Poll::Ready(sys::shutdown(self.stream.as_raw_fd(), Shutdown::Write))
    }

    /// io_uring has support for shutdown now, but it is not in any released
    /// kernel. Even with my "let's use latest" policy it would be crazy to
    /// mandate a kernel that doesn't even exist. So in preparation for that
    /// we'll sync-emulate this but already on an async wrapper
    pub(crate) fn poll_shutdown(
        &self,
        _cx: &mut Context<'_>,
        how: Shutdown,
    ) -> Poll<io::Result<()>> {
        Poll::Ready(sys::shutdown(self.stream.as_raw_fd(), how))
    }
}

#[derive(Debug)]
pub(crate) struct GlommioStream<S, B> {
    stream: NonBufferedStream<S>,
    rx_buf: B,
    rx_done: Cell<bool>,
    /// A completion-based read with the receive buffer lent to the kernel.
    rx_source: Option<Source>,
    /// The multishot receive a buffered stream arms for its lifetime, when
    /// the executor lends buffers. While it is armed every byte the socket
    /// produces lands in one of those buffers, so nothing here reads the
    /// socket directly.
    rx_multi: Option<Source>,
}

impl<S> From<socket2::Socket> for GlommioStream<S, NonBuffered>
where
    S: AsRawFd + From<socket2::Socket> + Unpin,
{
    fn from(socket: socket2::Socket) -> Self {
        let reactor = crate::executor().reactor();
        let mut stream = NonBufferedStream {
            reactor: Rc::downgrade(&reactor),
            stream: socket.into(),
            source_tx: None,
            source_rx: None,
            write_timeout: Timeout::new(reactor.register_timer()),
            read_timeout: Timeout::new(reactor.register_timer()),
        };
        stream.init();
        GlommioStream {
            stream,
            rx_buf: NonBuffered,
            rx_done: Cell::new(false),
            rx_source: None,
            rx_multi: None,
        }
    }
}

impl<S: AsRawFd> AsRawFd for GlommioStream<S, NonBuffered> {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.stream.as_raw_fd()
    }
}

impl<S> FromRawFd for GlommioStream<S, NonBuffered>
where
    S: AsRawFd + FromRawFd + From<socket2::Socket> + Unpin,
{
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        let socket = socket2::Socket::from_raw_fd(fd);
        GlommioStream::from(socket)
    }
}

impl<S: AsRawFd> GlommioStream<S, NonBuffered> {
    pub(crate) fn buffered_with<B: Buffered>(self, rx_buf: B) -> GlommioStream<S, B> {
        let rx_multi = self
            .stream
            .reactor
            .upgrade()
            .and_then(|reactor| reactor.recv_multi(self.stream.stream.as_raw_fd()));
        GlommioStream {
            stream: self.stream,
            rx_buf,
            rx_done: self.rx_done,
            rx_source: self.rx_source,
            rx_multi,
        }
    }
}

impl<S: AsRawFd, B: RxBuf> GlommioStream<S, B> {
    /// Receives data on the socket from the remote address to which it is
    /// connected, without removing that data from the queue.
    ///
    /// On success, returns the number of bytes peeked.
    /// Successive calls return the same data. This is accomplished by passing
    /// `MSG_PEEK` as a flag to the underlying `recv` system call.
    pub(crate) async fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(source) = self.rx_source.as_ref() {
            let read = std::future::poll_fn(|cx| match source.result() {
                Some(result) => Poll::Ready(result),
                None => {
                    source.add_waiter_single(cx.waker());
                    Poll::Pending
                }
            })
            .await?;
            let len = read.min(buf.len());
            match &*source.source_type() {
                SourceType::SockRecvInto(Some(lent)) => {
                    buf[..len].copy_from_slice(&lent.as_bytes()[..len]);
                }
                _ => unreachable!("a completion read without its buffer"),
            }
            return Ok(len);
        }
        let mut pos = self.rx_buf.peek(buf);
        if let Some(multi) = self.rx_multi.as_ref() {
            if let SourceType::RecvMulti(state) = &*multi.source_type() {
                let buffers = state.buffers.borrow();
                for arrival in &state.ready {
                    if pos == buf.len() {
                        break;
                    }
                    pos += arrival.copy_to(&buffers, arrival.taken, &mut buf[pos..]);
                }
            }
        }
        if pos < buf.len() && !self.rx_done.get() {
            if let Some(result) = self.stream.try_peek(&mut buf[pos..]) {
                match result {
                    Err(e) => return Err(e),
                    Ok(len) => {
                        pos += len;
                        if len == 0 {
                            self.rx_done.set(true);
                        }
                    }
                }
            }
        }
        if pos > 0 || self.rx_done.get() {
            return Ok(pos);
        }
        self.stream.peek(buf).await
    }

    pub(crate) fn poll_read(
        &mut self,
        cx: &Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if self.rx_buf.is_empty() {
            if buf.len() >= self.rx_buf.buffer_size()
                && self.rx_source.is_none()
                && self.rx_multi.is_none()
            {
                return self.stream.poll_read(cx, buf);
            }
            if !self.rx_done.get() {
                poll_err!(ready!(self.poll_replenish_buffer(cx)));
            }
        }
        Poll::Ready(Ok(self.rx_buf.read(buf)))
    }

    fn poll_replenish_buffer(&mut self, cx: &Context<'_>) -> Poll<io::Result<usize>> {
        let result = if self.rx_multi.is_some() {
            poll_err!(ready!(self.poll_fill_multishot(cx)))
        } else {
            let result = poll_err!(ready!(self.poll_fill(cx)));
            self.rx_buf.handle_result(result);
            result
        };
        if result == 0 {
            self.rx_done.set(true);
        }
        Poll::Ready(Ok(result))
    }

    /// Fills the receive buffer by a speculative `recv`, a completion read
    /// into the lent buffer, or readiness when the buffer cannot be lent.
    fn poll_fill(&mut self, cx: &Context<'_>) -> Poll<io::Result<usize>> {
        if let Some(source) = self.rx_source.as_ref() {
            let reactor = self.stream.reactor.upgrade().unwrap();
            let Some(result) = source.result() else {
                poll_err!(self.stream.read_timeout.check(&reactor));
                source.add_waiter_single(cx.waker());
                self.stream
                    .read_timeout
                    .maybe_set_timer(&reactor, cx.waker());
                return Poll::Pending;
            };
            self.stream.read_timeout.cancel_timer(&reactor);

            let source = self.rx_source.take().unwrap();
            match source.extract_source_type() {
                SourceType::SockRecvInto(Some(buffer)) => {
                    self.rx_buf.restore_kernel_buffer(buffer);
                }
                _ => unreachable!("a completion read came back without its buffer"),
            }

            return Poll::Ready(result);
        }

        if let Some(result) = self.stream.poll_read_speculative(self.rx_buf.unfilled()) {
            return Poll::Ready(result);
        }

        match self.rx_buf.take_kernel_buffer() {
            Some(buffer) => {
                let reactor = self.stream.reactor.upgrade().unwrap();
                self.rx_source = Some(reactor.recv_into(self.stream.stream.as_raw_fd(), buffer));
                self.poll_fill(cx)
            }
            None => self.stream.poll_read(cx, self.rx_buf.unfilled()),
        }
    }

    /// Fills the receive buffer from the multishot receive: every queued
    /// arrival that fits, else whatever the kernel has already posted and the
    /// reactor has not yet looked at, else waits. A request the kernel has
    /// dropped is re-armed, after taking what the socket holds, unless it
    /// ended in end of file or an error.
    ///
    /// Unlike [`poll_fill`](Self::poll_fill), this advances the receive
    /// buffer itself, arrival by arrival: a pipeline's replies sit in the
    /// queue as a dozen completions, and handing them out one per poll costs
    /// a poll round each.
    fn poll_fill_multishot(&mut self, cx: &Context<'_>) -> Poll<io::Result<usize>> {
        let reactor = self.stream.reactor.upgrade().unwrap();
        let mut reaped = false;
        loop {
            let taken = self.take_arrivals();
            if taken > 0 {
                self.stream.read_timeout.cancel_timer(&reactor);
                return Poll::Ready(Ok(taken));
            }
            let multi = self.rx_multi.as_ref().unwrap();
            let Some(result) = multi.result() else {
                if !reaped {
                    reaped = true;
                    if poll_err!(reactor.reap_socket_completions()) > 0 {
                        continue;
                    }
                }
                poll_err!(self.stream.read_timeout.check(&reactor));
                multi.add_waiter_single(cx.waker());
                self.stream
                    .read_timeout
                    .maybe_set_timer(&reactor, cx.waker());
                return Poll::Pending;
            };
            let was_bundled = match &*multi.source_type() {
                SourceType::RecvMulti(state) => state.bundled,
                _ => unreachable!("rx_multi holds a multishot receive"),
            };
            self.rx_multi = None;
            match result {
                Ok(0) => return Poll::Ready(Ok(0)),
                Ok(_) => {}
                Err(err) if err.raw_os_error() == Some(libc::ENOBUFS) => {}
                Err(err) if err.raw_os_error() == Some(libc::EINVAL) => {
                    reactor.refuse_recv_multi(was_bundled);
                    if was_bundled {
                        continue;
                    }
                    let result = poll_err!(ready!(self.poll_fill(cx)));
                    self.rx_buf.handle_result(result);
                    return Poll::Ready(Ok(result));
                }
                Err(err) => return Poll::Ready(Err(err)),
            }
            let fd = self.stream.stream.as_raw_fd();
            if let Some(result) = self.stream.poll_read_speculative(self.rx_buf.unfilled()) {
                self.rx_multi = reactor.recv_multi(fd);
                let result = poll_err!(result);
                self.rx_buf.handle_result(result);
                return Poll::Ready(Ok(result));
            }
            match reactor.recv_multi(fd) {
                Some(source) => self.rx_multi = Some(source),
                None => {
                    let result = poll_err!(ready!(self.poll_fill(cx)));
                    self.rx_buf.handle_result(result);
                    return Poll::Ready(Ok(result));
                }
            }
        }
    }

    /// Moves queued arrivals into the receive buffer until it is full or the
    /// queue is empty, and returns how many bytes that was.
    fn take_arrivals(&mut self) -> usize {
        let mut total = 0;
        while let Some(n) = self.take_arrival() {
            self.rx_buf.handle_result(n);
            total += n;
            if self.rx_buf.unfilled().is_empty() {
                break;
            }
        }
        total
    }

    /// Copies the oldest queued arrival into the receive buffer, as much as
    /// fits, and gives the kernel its buffer back once it is drained.
    fn take_arrival(&mut self) -> Option<usize> {
        let multi = self.rx_multi.as_ref()?;
        let mut source_type = multi.source_type_mut();
        let SourceType::RecvMulti(state) = &mut *source_type else {
            unreachable!("rx_multi holds a multishot receive");
        };
        let arrival = state.ready.front_mut()?;
        let room = self.rx_buf.unfilled();
        let n = arrival.copy_to(&state.buffers.borrow(), arrival.taken, room);
        arrival.taken += n;
        if arrival.taken == arrival.len {
            let arrival = state.ready.pop_front().unwrap();
            let mut buffers = state.buffers.borrow_mut();
            for bid in arrival.bids {
                buffers.give_back(bid);
            }
            buffers.publish();
        }
        Some(n)
    }

    pub(crate) fn poll_write(&mut self, cx: &Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.stream.poll_write(cx, buf)
    }

    pub(crate) fn poll_write_vectored(
        &mut self,
        cx: &Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.stream.poll_write_vectored(cx, bufs)
    }

    pub(crate) fn poll_flush(&self, _cx: &Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    pub(crate) fn poll_close(&mut self, cx: &Context<'_>) -> Poll<io::Result<()>> {
        self.stream.poll_close(cx)
    }

    pub(crate) fn poll_shutdown(
        &self,
        cx: &mut Context<'_>,
        how: Shutdown,
    ) -> Poll<io::Result<()>> {
        self.stream.poll_shutdown(cx, how)
    }

    pub(crate) fn set_write_timeout(&self, dur: Option<Duration>) -> Result<()> {
        self.stream.write_timeout.set(dur)
    }

    pub(crate) fn set_read_timeout(&self, dur: Option<Duration>) -> Result<()> {
        self.stream.read_timeout.set(dur)
    }

    pub(crate) fn write_timeout(&self) -> Option<Duration> {
        self.stream.write_timeout.get()
    }

    pub(crate) fn read_timeout(&self) -> Option<Duration> {
        self.stream.read_timeout.get()
    }

    pub(crate) fn stream(&self) -> &S {
        &self.stream.stream
    }
}

impl<S: AsRawFd, B: Buffered> GlommioStream<S, B> {
    /// Stays ready at end of stream, where a completion read would otherwise
    /// return `Pending` again.
    pub(crate) fn poll_fill_buf(&mut self, cx: &Context<'_>) -> Poll<io::Result<&[u8]>> {
        if self.rx_buf.is_empty() && !self.rx_done.get() {
            poll_err!(ready!(self.poll_replenish_buffer(cx)));
        }
        Poll::Ready(Ok(self.rx_buf.as_bytes()))
    }

    pub(crate) fn consume(&mut self, amt: usize) {
        self.rx_buf.consume(amt);
    }
}

impl<S: AsRawFd> NonBufferedStream<S> {
    /// Splits into a receiving and a sending stream over one shared socket.
    ///
    /// The receiving side keeps the read readiness source and the read
    /// timeout, the sending side keeps the write readiness source and the
    /// write timeout. Each side gets a fresh timer id for the direction it
    /// does not own, so both remain complete streams; that id is never armed,
    /// so it may be anything, including 0 once the executor is gone.
    fn split(self) -> (NonBufferedStream<Rc<S>>, NonBufferedStream<Rc<S>>) {
        let spare_id = || self.reactor.upgrade().map_or(0, |r| r.register_timer());
        let (rx_spare, tx_spare) = (spare_id(), spare_id());
        let stream = Rc::new(self.stream);
        let rx = NonBufferedStream {
            reactor: self.reactor.clone(),
            stream: stream.clone(),
            source_tx: None,
            source_rx: self.source_rx,
            write_timeout: Timeout::new(rx_spare),
            read_timeout: self.read_timeout,
        };
        let tx = NonBufferedStream {
            reactor: self.reactor,
            stream,
            source_tx: self.source_tx,
            source_rx: None,
            write_timeout: self.write_timeout,
            read_timeout: Timeout::new(tx_spare),
        };
        (rx, tx)
    }
}

impl<S: AsRawFd> NonBufferedStream<Rc<S>> {
    /// Whether two halves came from the same [`split`](Self::split) and are
    /// the socket's only owners, which is what `unsplit` needs.
    fn is_pair(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.stream, &other.stream) && Rc::strong_count(&self.stream) == 2
    }

    /// Reassembles the stream `split` took apart. The caller has checked
    /// [`is_pair`](Self::is_pair).
    fn unsplit(rx: Self, tx: Self) -> NonBufferedStream<S> {
        if let Some(reactor) = rx.reactor.upgrade() {
            rx.write_timeout.cancel_timer(&reactor);
            tx.read_timeout.cancel_timer(&reactor);
        }
        drop(tx.stream);
        let stream = Rc::try_unwrap(rx.stream)
            .unwrap_or_else(|_| unreachable!("a split pair has exactly two owners"));
        NonBufferedStream {
            reactor: rx.reactor,
            stream,
            source_tx: tx.source_tx,
            source_rx: rx.source_rx,
            write_timeout: tx.write_timeout,
            read_timeout: rx.read_timeout,
        }
    }
}

/// The two halves [`GlommioStream::split`] produces: the read half keeps the
/// receive buffer, the write half never has one.
pub(crate) type SplitPair<S, B> = (GlommioStream<Rc<S>, B>, GlommioStream<Rc<S>, NonBuffered>);

impl<S: AsRawFd, B: RxBuf> GlommioStream<S, B> {
    /// Splits into a read half, which keeps the receive buffer, and a write
    /// half over the same socket.
    pub(crate) fn split(self) -> SplitPair<S, B> {
        let (rx, tx) = self.stream.split();
        (
            GlommioStream {
                stream: rx,
                rx_buf: self.rx_buf,
                rx_done: self.rx_done,
                rx_source: self.rx_source,
                rx_multi: self.rx_multi,
            },
            GlommioStream {
                stream: tx,
                rx_buf: NonBuffered,
                rx_done: Cell::new(false),
                rx_source: None,
                rx_multi: None,
            },
        )
    }
}

impl<S: AsRawFd, B: RxBuf> GlommioStream<Rc<S>, B> {
    /// Puts a split pair back together, or hands both halves back when they
    /// do not belong to the same stream.
    #[allow(
        clippy::result_large_err,
        reason = "the halves go back to the caller, who still owns the socket through them"
    )]
    pub(crate) fn unsplit(
        rx: Self,
        tx: GlommioStream<Rc<S>, NonBuffered>,
    ) -> std::result::Result<GlommioStream<S, B>, SplitPair<S, B>> {
        if !rx.stream.is_pair(&tx.stream) {
            return Err((rx, tx));
        }
        Ok(GlommioStream {
            stream: NonBufferedStream::unsplit(rx.stream, tx.stream),
            rx_buf: rx.rx_buf,
            rx_done: rx.rx_done,
            rx_source: rx.rx_source,
            rx_multi: rx.rx_multi,
        })
    }
}

impl<S: AsRawFd + IntoRawFd> NonBufferedStream<S> {
    /// Extracts the raw file descriptor, cleaning up glommio state
    /// but keeping the fd open.
    fn into_raw_fd(mut self) -> RawFd {
        self.source_tx.take();
        self.source_rx.take();

        if let Some(reactor) = self.reactor.upgrade() {
            self.write_timeout.cancel_timer(&reactor);
            self.read_timeout.cancel_timer(&reactor);
        }

        self.stream.into_raw_fd()
    }
}

impl<S: AsRawFd + IntoRawFd> GlommioStream<S, NonBuffered> {
    /// Extracts the raw file descriptor, cleaning up glommio state
    /// but keeping the fd open.
    pub(crate) fn into_raw_fd(self) -> RawFd {
        self.stream.into_raw_fd()
    }
}
