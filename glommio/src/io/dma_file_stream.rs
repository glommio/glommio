//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
use crate::{
    io::{read_result::ReadResult, DmaFile},
    sys::DmaBuffer,
    task, ByteSliceMutExt,
};
use ahash::AHashMap;
use core::task::Waker;
use futures_lite::{
    future::poll_fn,
    io::{AsyncRead, AsyncWrite},
    ready,
    stream::{self, StreamExt},
};
use scopeguard::ScopeGuard;
use std::{
    cell::RefCell,
    collections::VecDeque,
    io,
    os::unix::prelude::AsRawFd,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
    vec::Vec,
};

use crate::Result;

macro_rules! current_error {
    ( $state:expr ) => {
        $state.error.take().map(|err| Err(err.into()))
    };
}

#[derive(Debug, Clone)]
/// Builds a DmaStreamReader, allowing linear access to a Direct I/O [`DmaFile`]
///
/// [`DmaFile`]: struct.DmaFile.html
pub struct DmaStreamReaderBuilder {
    start: u64,
    end: u64,
    buffer_size: usize,
    read_ahead: usize,
    pub(super) file: Rc<DmaFile>,
}

impl DmaStreamReaderBuilder {
    /// Creates a new DmaStreamReaderBuilder, given a shared pointer to a
    /// [`DmaFile`]
    ///
    /// This method is useful in situations where a shared pointer was already
    /// present. For example if you need to scan a header in the file and
    /// also make it available for random reads.
    ///
    /// Other than that, it is the same as [`new`]
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use glommio::{
    ///     io::{DmaFile, DmaStreamReaderBuilder},
    ///     LocalExecutor,
    /// };
    /// use std::rc::Rc;
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let file = Rc::new(DmaFile::open("myfile.txt").await.unwrap());
    ///     let _reader = DmaStreamReaderBuilder::from_rc(file.clone()).build();
    ///
    ///     // issue random I/O now, even though a stream is open.
    ///     glommio::spawn_local(async move {
    ///         file.read_at(0, 8).await.unwrap();
    ///     })
    ///     .await;
    /// });
    /// ```
    /// [`DmaFile`]: struct.DmaFile.html
    /// [`DmaStreamReader`]: struct.DmaStreamReader.html
    /// [`build`]: #method.build
    /// [`new`]: #method.new
    #[must_use = "The builder must be built to be useful"]
    pub fn from_rc(file: Rc<DmaFile>) -> DmaStreamReaderBuilder {
        DmaStreamReaderBuilder {
            start: 0,
            end: u64::MAX,
            buffer_size: 128 << 10,
            read_ahead: 4,
            file,
        }
    }

    /// Creates a new DmaStreamReaderBuilder, given a [`DmaFile`]
    ///
    /// Various properties can be set by using its `with` methods.
    ///
    /// A [`DmaStreamReader`] can later be constructed from it by
    /// calling [`build`]
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use glommio::{
    ///     io::{DmaFile, DmaStreamReaderBuilder},
    ///     LocalExecutor,
    /// };
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let file = DmaFile::open("myfile.txt").await.unwrap();
    ///     let _reader = DmaStreamReaderBuilder::new(file).build();
    /// });
    /// ```
    /// [`DmaFile`]: struct.DmaFile.html
    /// [`DmaStreamReader`]: struct.DmaStreamReader.html
    /// [`build`]: #method.build
    #[must_use = "The builder must be built to be useful"]
    pub fn new(file: DmaFile) -> DmaStreamReaderBuilder {
        Self::from_rc(Rc::new(file))
    }

    /// Define a starting position.
    ///
    /// Reads from the [`DmaStreamReader`] will start from this position
    ///
    /// [`DmaStreamReader`]: struct.DmaStreamReader.html
    #[must_use = "The builder must be built to be useful"]
    pub fn with_start_pos(mut self, start: u64) -> Self {
        self.start = start;
        self
    }

    /// Define an end position.
    ///
    /// Reads from the [`DmaStreamReader`] will end at this position even if the
    /// file is larger.
    ///
    /// [`DmaStreamReader`]: struct.DmaStreamReader.html
    #[must_use = "The builder must be built to be useful"]
    pub fn with_end_pos(mut self, end: u64) -> Self {
        self.end = end;
        self
    }

    /// Define the number of read-ahead buffers that will be used by the
    /// [`DmaStreamReader`]
    ///
    /// Higher read-ahead numbers mean more parallelism but also more memory
    /// usage.
    ///
    /// [`DmaStreamReader`]: struct.DmaStreamReader.html
    #[must_use = "The builder must be built to be useful"]
    pub fn with_read_ahead(mut self, read_ahead: usize) -> Self {
        self.read_ahead = read_ahead;
        self
    }

    /// Define the buffer size that will be used by the [`DmaStreamReader`]
    ///
    /// [`DmaStreamReader`]: struct.DmaStreamReader.html
    #[must_use = "The builder must be built to be useful"]
    pub fn with_buffer_size(mut self, buffer_size: usize) -> Self {
        let buffer_size = std::cmp::max(buffer_size, 1);
        self.buffer_size = self.file.align_up(buffer_size as _) as usize;
        self
    }

    /// Builds a [`DmaStreamReader`] with the properties defined by this
    /// [`DmaStreamReaderBuilder`]
    ///
    /// [`DmaStreamReader`]: struct.DmaStreamReader.html
    /// [`DmaStreamReaderBuilder`]: struct.DmaStreamReaderBuilder.html
    pub fn build(self) -> DmaStreamReader {
        DmaStreamReader::new(self)
    }
}

#[derive(Debug)]
struct DmaStreamReaderState {
    buffer_read_ahead_current_pos: u64,
    max_pos: u64,
    buffer_size: u64,
    read_ahead: usize,
    wakermap: AHashMap<u64, Waker>,
    pending: AHashMap<u64, task::JoinHandle<()>>,
    error: Option<io::Error>,
    buffermap: AHashMap<u64, ReadResult>,
    cached_buffer: Option<(u64, ReadResult)>,
}

impl DmaStreamReaderState {
    /// Drops the buffers before `id`, cached or still being read. Only the
    /// read-ahead window is held, so this looks at what is there rather than
    /// at every id a long skip passed over.
    fn discard_buffers_before(&mut self, id: u64) {
        self.buffermap.retain(|&held, _| held >= id);
        self.pending.retain(|&held, handle| {
            if held < id {
                handle.cancel();
            }
            held >= id
        });
    }

    fn replenish_read_ahead(&mut self, state: Rc<RefCell<Self>>, file: Rc<DmaFile>) {
        for _ in self.buffermap.len() + self.pending.len()..self.read_ahead {
            self.fill_buffer(state.clone(), file.clone());
        }
    }

    fn fill_buffer(&mut self, read_state: Rc<RefCell<Self>>, file: Rc<DmaFile>) {
        let pos = self.buffer_read_ahead_current_pos;
        if self.fetch_buffer_at(pos, read_state, file) {
            self.buffer_read_ahead_current_pos = pos.saturating_add(self.buffer_size);
        }
    }

    /// Starts reading the buffer at `pos` unless it is past the end, cached
    /// or being read already, and returns whether it did. A read skipped
    /// because another failed still releases and wakes its waiter, which
    /// then asks for the buffer again.
    fn fetch_buffer_at(
        &mut self,
        pos: u64,
        read_state: Rc<RefCell<Self>>,
        file: Rc<DmaFile>,
    ) -> bool {
        let buffer_id = self.buffer_id(pos);

        if pos > self.max_pos {
            return false;
        }
        let len = self.buffer_size;

        if self.pending.get(&buffer_id).is_some() || self.buffermap.get(&buffer_id).is_some() {
            return false;
        }

        let pending = crate::spawn_local(async move {
            futures_lite::future::yield_now().await;

            let buffer = if read_state.borrow().error.is_some() {
                None
            } else {
                Some(file.read_at_aligned(pos, len as _).await)
            };

            let mut state = read_state.borrow_mut();
            match buffer {
                Some(Ok(buf)) => {
                    state.buffermap.insert(buffer_id, buf);
                }
                Some(Err(x)) => {
                    state.error = Some(x.into());
                }
                None => {}
            }
            state.pending.remove(&buffer_id);
            let mut wakers = state.wakermap.remove(&buffer_id);
            drop(state);
            if let Some(w) = wakers.take() {
                w.wake();
            }
        })
        .detach();

        self.pending.insert(buffer_id, pending);
        true
    }

    fn offset_of(&self, pos: u64) -> usize {
        (pos % self.buffer_size) as usize
    }

    fn buffer_id(&self, pos: u64) -> u64 {
        pos / self.buffer_size
    }

    /// Returns true if heir was no waker for this `buffer_id`, otherwise, it
    /// replaces the existing one and returns false.
    fn add_waker(&mut self, buffer_id: u64, waker: Waker) -> bool {
        self.wakermap.insert(buffer_id, waker).is_none()
    }

    fn cancel_all_in_flight(&mut self) -> Vec<task::JoinHandle<()>> {
        let mut handles = Vec::new();
        for (_k, v) in self.pending.drain() {
            v.cancel();
            handles.push(v);
        }
        handles
    }

    fn update_cached_buffer(&mut self, buffer_id: &u64) -> Option<&ReadResult> {
        if let Some(buffer) = self.buffermap.get(buffer_id) {
            self.cached_buffer = Some((*buffer_id, buffer.clone()));
            Some(buffer)
        } else {
            None
        }
    }

    fn get_cached_buffer(&mut self, buffer_id: &u64) -> Option<&ReadResult> {
        match &self.cached_buffer {
            None => return self.update_cached_buffer(buffer_id),
            Some((id, _)) => {
                if id != buffer_id {
                    self.update_cached_buffer(buffer_id);
                }
            }
        }

        if let Some((id, buffer)) = &self.cached_buffer {
            if id == buffer_id {
                return Some(buffer);
            }
        }
        None
    }
}

impl Drop for DmaStreamReaderState {
    fn drop(&mut self) {
        for (_k, v) in self.pending.drain() {
            v.cancel();
        }
    }
}

#[derive(Debug)]
/// Provides linear access to a [`DmaFile`]. The [`DmaFile`] is a convenient way
/// to manage a file through Direct I/O, but its interface is conductive to
/// random access, as a position must always be specified.
///
/// In situations where the file must be scanned linearly - either because a
/// large chunk is to be read or because we are scanning the whole file, it may
/// be more convenient to use a linear scan API.
///
/// The [`DmaStreamReader`] implements [`AsyncRead`], which can offer the user
/// with a convenient way of issuing reads. However note that this mandates a
/// copy between the Dma Buffer used to read the file contents and the
/// user-specified buffer.
///
/// To avoid that copy the [`DmaStreamReader`] provides the
/// [`get_buffer_aligned`] method which exposes the buffer as a byte slice.
/// Different situations will call for different APIs to be used.
///
/// [`DmaFile`]: struct.DmaFile.html
/// [`DmaBuffer`]: struct.DmaBuffer.html
/// [`DmaStreamReader`]: struct.DmaStreamReader.html
/// [`get_buffer_aligned`]:
/// struct.DmaStreamReader.html#method.get_buffer_aligned
/// [`AsyncRead`]: https://docs.rs/futures/0.3.5/futures/io/trait.AsyncRead.html
pub struct DmaStreamReader {
    _start: u64,
    end: u64,
    current_pos: u64,
    buffer_size: u64,
    file: Rc<DmaFile>,
    state: Rc<RefCell<DmaStreamReaderState>>,
}

macro_rules! collect_error {
    ( $state:expr, $res:expr ) => {{
        if let Err(x) = $res {
            $state.error = Some(x.into());
            true
        } else {
            false
        }
    }};
}

impl DmaStreamReader {
    /// Closes this [`DmaStreamReader`].
    ///
    /// It is illegal to close the [`DmaStreamReader`] more than once.
    ///
    /// # Examples
    /// ```no_run
    /// use glommio::{
    ///     io::{DmaFile, DmaStreamReaderBuilder},
    ///     LocalExecutor,
    /// };
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let file = DmaFile::open("myfile.txt").await.unwrap();
    ///     let mut reader = DmaStreamReaderBuilder::new(file).build();
    ///     reader.close().await.unwrap();
    /// });
    /// ```
    pub async fn close(self) -> Result<()> {
        let handles = {
            let mut state = self.state.borrow_mut();
            state.cancel_all_in_flight()
        };

        let to_cancel = handles.len();
        let cancelled = stream::iter(handles).then(|f| f).count().await;
        assert_eq!(to_cancel, cancelled);
        self.file.close_rc().await.unwrap();

        let mut state = self.state.borrow_mut();
        match state.error.take() {
            Some(err) => Err(err.into()),
            None => Ok(()),
        }
    }

    fn new(builder: DmaStreamReaderBuilder) -> DmaStreamReader {
        let state = DmaStreamReaderState {
            buffer_read_ahead_current_pos: builder.start
                - builder.start % builder.buffer_size as u64,
            read_ahead: builder.read_ahead,
            max_pos: builder.end,
            buffer_size: builder.buffer_size as u64,
            wakermap: AHashMap::with_capacity(builder.read_ahead),
            buffermap: AHashMap::with_capacity(builder.read_ahead),
            pending: AHashMap::with_capacity(builder.read_ahead),
            error: None,
            cached_buffer: None,
        };

        let state = Rc::new(RefCell::new(state));

        {
            let state_rc = state.clone();
            let mut state = state.borrow_mut();
            state.replenish_read_ahead(state_rc, builder.file.clone());
        }

        DmaStreamReader {
            file: builder.file,
            _start: builder.start,
            end: builder.end,
            current_pos: builder.start,
            buffer_size: builder.buffer_size as _,
            state,
        }
    }

    /// Skip reading bytes from this [`DmaStreamReader`].
    ///
    /// The file cursor is advanced by the provided bytes. As this is a linear
    /// access API, once those bytes are skipped they are gone. If you need to
    /// read those bytes, just not now, it is better to read them and save them
    /// in some temporary buffer.
    ///
    /// # Examples
    /// ```no_run
    /// use glommio::{
    ///     io::{DmaFile, DmaStreamReaderBuilder},
    ///     LocalExecutor,
    /// };
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let file = DmaFile::open("myfile.txt").await.unwrap();
    ///     let mut reader = DmaStreamReaderBuilder::new(file).build();
    ///     assert_eq!(reader.current_pos(), 0);
    ///     reader.skip(8);
    ///     assert_eq!(reader.current_pos(), 8);
    ///     reader.close().await.unwrap();
    /// });
    /// ```
    ///
    /// [`DmaStreamReader`]: struct.DmaStreamReader.html
    ///
    /// Note that the end range is exclusive, so if we didn't cross a buffer
    /// boundary we don't discard anything.
    pub fn skip(&mut self, bytes: u64) {
        let mut state = self.state.borrow_mut();
        let buffer_id = state.buffer_id(self.current_pos);
        if self.current_pos < self.end {
            self.current_pos = std::cmp::min(self.current_pos.saturating_add(bytes), self.end);
        }
        let new_buffer_id = state.buffer_id(self.current_pos);
        if buffer_id != new_buffer_id {
            let candidate_read_ahead_pos = self.current_pos - self.current_pos % self.buffer_size;
            state.buffer_read_ahead_current_pos = std::cmp::max(
                state.buffer_read_ahead_current_pos,
                candidate_read_ahead_pos,
            );

            state.discard_buffers_before(new_buffer_id);
            state.replenish_read_ahead(self.state.clone(), self.file.clone());
        }
    }

    /// Acquires the current position of this [`DmaStreamReader`].
    ///
    /// # Examples
    /// ```no_run
    /// use glommio::{
    ///     io::{DmaFile, DmaStreamReaderBuilder},
    ///     LocalExecutor,
    /// };
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let file = DmaFile::open("myfile.txt").await.unwrap();
    ///     let mut reader = DmaStreamReaderBuilder::new(file).build();
    ///     assert_eq!(reader.current_pos(), 0);
    ///     reader.skip(8);
    ///     assert_eq!(reader.current_pos(), 8);
    ///     reader.close().await.unwrap();
    /// });
    /// ```
    ///
    /// [`DmaStreamReader`]: struct.DmaStreamReader.html
    pub fn current_pos(&self) -> u64 {
        self.current_pos
    }

    /// Allows access to the buffer that holds the current position with no
    /// extra copy.
    ///
    /// This function returns a [`ReadResult`]. It contains all the bytes read,
    /// which can be less than the requested amount. Users are expected to call
    /// this in a loop like they would [`AsyncRead::poll_read`].
    ///
    /// The buffer is not released until the returned [`ReadResult`] goes
    /// out of scope. So if you plan to keep this alive for a long time this
    /// is probably the wrong API.
    ///
    /// If EOF is hit while reading with this method, the number of bytes in the
    /// returned buffer will be less than number requested and the remaining
    /// bytes will be 0.
    ///
    /// # Examples
    /// ```no_run
    /// use glommio::{
    ///     io::{DmaFile, DmaStreamReaderBuilder},
    ///     LocalExecutor,
    /// };
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let file = DmaFile::open("myfile.txt").await.unwrap();
    ///     let mut reader = DmaStreamReaderBuilder::new(file).build();
    ///     assert_eq!(reader.current_pos(), 0);
    ///     let result = reader.get_buffer_aligned(512).await.unwrap();
    ///     assert_eq!(result.len(), 512);
    ///     println!("First 512 bytes: {:?}", &*result);
    ///     reader.close().await.unwrap();
    /// });
    /// ```
    ///
    /// [`AsyncRead::poll_read`]: https://docs.rs/futures-io/latest/futures_io/trait.AsyncRead.html#tymethod.poll_read
    /// [`ReadResult`]: struct.ReadResult.html
    pub async fn get_buffer_aligned(&mut self, len: u64) -> Result<ReadResult> {
        poll_fn(|cx| self.poll_get_buffer_aligned(cx, len)).await
    }

    /// A variant of [`get_buffer_aligned`] that can be called from a poll
    /// function context.
    ///
    /// [`get_buffer_aligned`]: Self::get_buffer_aligned
    pub fn poll_get_buffer_aligned(
        &mut self,
        cx: &Context<'_>,
        mut len: u64,
    ) -> Poll<Result<ReadResult>> {
        let (start_id, buffer_len) = {
            let state = self.state.borrow();
            let start_id = state.buffer_id(self.current_pos);
            let offset = state.offset_of(self.current_pos);

            len = len.min(state.max_pos.saturating_sub(self.current_pos));

            (start_id, (self.buffer_size - offset as u64).min(len))
        };

        if len == 0 {
            return Poll::Ready(Ok(ReadResult::empty_buffer()));
        }

        let x = ready!(self.poll_get_buffer(cx, buffer_len, start_id))?;
        self.skip(x.len() as u64);
        Poll::Ready(Ok(x))
    }

    fn poll_get_buffer(
        &mut self,
        cx: &Context<'_>,
        len: u64,
        buffer_id: u64,
    ) -> Poll<Result<ReadResult>> {
        let mut state = self.state.borrow_mut();
        if let Some(err) = current_error!(state) {
            return Poll::Ready(err);
        }

        match state.get_cached_buffer(&buffer_id).cloned() {
            None => {
                if state.add_waker(buffer_id, cx.waker().clone()) {
                    let pos = buffer_id * state.buffer_size;
                    state.fetch_buffer_at(pos, self.state.clone(), self.file.clone());
                    state.fill_buffer(self.state.clone(), self.file.clone());
                }
                Poll::Pending
            }
            Some(buffer) => {
                let offset = state.offset_of(self.current_pos);
                if buffer.len() <= offset {
                    Poll::Ready(Ok(ReadResult::empty_buffer()))
                } else {
                    let len = std::cmp::min(len as usize, buffer.len() - offset);
                    Poll::Ready(ReadResult::slice(&buffer, offset, len).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "buffer offset out of range")
                            .into()
                    }))
                }
            }
        }
    }
}

impl AsyncRead for DmaStreamReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let res = ready!(self.poll_get_buffer_aligned(cx, buf.len() as u64))?;
        buf[..res.len()].copy_from_slice(&res);
        Poll::Ready(Ok(res.len()))
    }
}

#[derive(Debug)]
/// Builds a DmaStreamWriter, allowing linear access to a Direct I/O [`DmaFile`]
///
/// [`DmaFile`]: struct.DmaFile.html
pub struct DmaStreamWriterBuilder {
    buffer_size: usize,
    write_behind: usize,
    sync_on_close: bool,
    file: Rc<DmaFile>,
}

impl DmaStreamWriterBuilder {
    /// Creates a new DmaStreamWriterBuilder, given a [`DmaFile`]
    ///
    /// Various properties can be set by using its `with` methods.
    ///
    /// A [`DmaStreamWriter`] can later be constructed from it by
    /// calling [`build`]
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use glommio::{
    ///     io::{DmaFile, DmaStreamWriterBuilder},
    ///     LocalExecutor,
    /// };
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let file = DmaFile::create("myfile.txt").await.unwrap();
    ///     let _reader = DmaStreamWriterBuilder::new(file).build();
    /// });
    /// ```
    /// [`DmaFile`]: struct.DmaFile.html
    /// [`DmaStreamWriter`]: struct.DmaStreamWriter.html
    /// [`build`]: #method.build
    #[must_use = "The builder must be built to be useful"]
    pub fn new(file: DmaFile) -> DmaStreamWriterBuilder {
        DmaStreamWriterBuilder {
            buffer_size: 128 << 10,
            write_behind: 4,
            sync_on_close: true,
            file: Rc::new(file),
        }
    }

    /// Define the number of write-behind buffers that will be used by the
    /// [`DmaStreamWriter`]
    ///
    /// Higher write-behind numbers mean more parallelism but also more memory
    /// usage. As long as there is still write-behind buffers available
    /// writing to this [`DmaStreamWriter`] will not block.
    ///
    /// [`DmaStreamWriter`]: struct.DmaStreamWriter.html
    #[must_use = "The builder must be built to be useful"]
    pub fn with_write_behind(mut self, write_behind: usize) -> Self {
        self.write_behind = std::cmp::max(write_behind, 1);
        self
    }

    /// Does not issue a sync operation when closing the file. This is dangerous
    /// and in most cases may lead to data loss.
    #[must_use = "The builder must be built to be useful"]
    pub fn with_sync_on_close_disabled(mut self, flush_disabled: bool) -> Self {
        self.sync_on_close = !flush_disabled;
        self
    }

    /// Define the buffer size that will be used by the [`DmaStreamWriter`]
    ///
    /// [`DmaStreamWriter`]: struct.DmaStreamWriter.html
    #[must_use = "The builder must be built to be useful"]
    pub fn with_buffer_size(mut self, buffer_size: usize) -> Self {
        let buffer_size = std::cmp::max(buffer_size, 1);
        self.buffer_size = self.file.align_up(buffer_size as _) as usize;
        self
    }

    /// Builds a [`DmaStreamWriter`] with the properties defined by this
    /// [`DmaStreamWriterBuilder`]
    ///
    /// [`DmaStreamWriter`]: struct.DmaStreamWriter.html
    /// [`DmaStreamWriterBuilder`]: struct.DmaStreamWriterBuilder.html
    pub fn build(self) -> DmaStreamWriter {
        DmaStreamWriter::new(self)
    }
}

#[derive(Debug)]
enum FileStatus {
    /// Open, can be closed.
    Open,
    /// We are closing it.
    Closing,
    /// The close or seal has finished, and its result is waiting for the
    /// poll to collect it.
    Finished,
    /// It was closed, and the result was handed to the caller.
    Closed,
}

#[derive(Debug)]
enum FlushStatus {
    Pending(Option<task::JoinHandle<()>>),
    Complete,
}

#[derive(Debug)]
struct DmaStreamWriterFlushState {
    flushes: VecDeque<(u64, FlushStatus)>,
    pending_flush_count: usize,
    flushed_pos: u64,
}

impl DmaStreamWriterFlushState {
    fn new(write_behind: usize) -> DmaStreamWriterFlushState {
        DmaStreamWriterFlushState {
            flushes: VecDeque::with_capacity(write_behind * 2),
            pending_flush_count: 0,
            flushed_pos: 0,
        }
    }

    fn last_flush_pos(&self) -> u64 {
        self.flushes
            .back()
            .map_or(self.flushed_pos, |(pos, _)| *pos)
    }

    fn take_pending_handles(&mut self) -> Vec<task::JoinHandle<()>> {
        self.take_pending_flushes()
            .into_iter()
            .map(|(_, handle)| handle)
            .collect()
    }

    fn take_pending_flushes(&mut self) -> VecDeque<(u64, task::JoinHandle<()>)> {
        let mut handles = VecDeque::with_capacity(self.pending_flush_count);
        for (pos, status) in &mut self.flushes {
            if let FlushStatus::Pending(handle) = status {
                if let Some(h) = handle.take() {
                    handles.push_back((*pos, h));
                }
            }
        }
        handles
    }

    /// Puts back a handle taken by [`Self::take_pending_flushes`] and not
    /// awaited to the end. One whose flush is no longer tracked has finished.
    fn restore_pending_flush(&mut self, flush_pos: u64, handle: task::JoinHandle<()>) {
        if let Some((_, FlushStatus::Pending(slot @ None))) =
            self.flushes.iter_mut().find(|(pos, _)| *pos == flush_pos)
        {
            *slot = Some(handle);
        }
    }

    fn on_start(&mut self, flush_pos: u64, handle: task::JoinHandle<()>) {
        self.pending_flush_count += 1;
        assert!(self.last_flush_pos() < flush_pos);
        self.flushes
            .push_back((flush_pos, FlushStatus::Pending(Some(handle))));
    }

    /// Notes:
    /// - writes can complete out-of-order
    /// - `flushes` is maintained in sorted order of position
    ///
    /// We will update the status of `flush_pos` to complete. We also count
    /// contiguous completed flushes from the front, `flushed_pos` can be
    /// updated accordingly and we don't need to track them anymore, so drain
    /// that range.
    fn on_complete(&mut self, flush_pos: u64) -> u64 {
        self.pending_flush_count -= 1;
        let mut drainable_len = 0usize;
        let mut counting = true;
        for (pos, status) in &mut self.flushes {
            if *pos == flush_pos {
                *status = FlushStatus::Complete;
            }
            if counting {
                if let FlushStatus::Complete = status {
                    drainable_len += 1;
                    self.flushed_pos = *pos;
                } else {
                    counting = false;
                }
            } else if *pos >= flush_pos {
                break;
            }
        }
        if drainable_len > 0 {
            self.flushes.drain(..drainable_len);
        }
        self.flushed_pos
    }
}

#[derive(Debug)]
struct DmaStreamWriterState {
    buffer_size: usize,
    /// Tasks waiting for a close to finish, woken only once it has, so that
    /// the buffer writes the close waits on do not wake them early.
    close_waiters: Vec<Waker>,
    file_status: FileStatus,
    error: Option<io::Error>,
    flush_state: DmaStreamWriterFlushState,
    current_buffer: Option<DmaBuffer>,
    aligned_pos: u64,
    synced_pos: u64,
    buffer_pos: usize,
    write_behind: usize,
    sync_on_close: bool,
    /// A flush failed, so nothing from its position on can reach the file and
    /// every later operation must fail too, after the first has reported why.
    write_failed: bool,
    /// Tasks waiting on a buffer write, woken each time one finishes: writes
    /// waiting for a block or a free buffer, and flushes waiting for the
    /// flushed position to reach a target. A caller cannot always wait on
    /// the writes themselves: a write's handle can be awaited only once, and
    /// a concurrent flush or close may hold it. Each waiter has a place of
    /// its own, since a writer shared by reference can have several.
    flush_waiters: Vec<Waker>,
}

/// We do it like this so that the write and read close errors are exactly the
/// same. The reader uses enhanced errors, so it has a message in the inner
/// attribute of `io::Error`.
fn closed_error() -> io::Error {
    io::Error::other(format!("{}", io::Error::from_raw_os_error(libc::EBADF)))
}

macro_rules! already_closed {
    () => {
        Poll::Ready(Err(closed_error()))
    };
}

macro_rules! ensure_not_closed {
    ( $file:expr ) => {
        match $file.take() {
            Some(file) => file,
            None => {
                return already_closed!();
            }
        }
    };
}

/// Adds `waker` to `waiters` unless a waker for the same task is there.
fn register_waiter(waiters: &mut Vec<Waker>, waker: &Waker) {
    if !waiters.iter().any(|w| w.will_wake(waker)) {
        waiters.push(waker.clone());
    }
}

impl DmaStreamWriterState {
    fn wait_for_flush(&mut self, cx: &Context<'_>) {
        register_waiter(&mut self.flush_waiters, cx.waker());
    }

    fn wait_for_close(&mut self, cx: &Context<'_>) {
        register_waiter(&mut self.close_waiters, cx.waker());
    }

    /// Ready once everything up to `target` is written, or once the stream
    /// has failed and nothing more will be.
    fn poll_flushed_to(&mut self, target: u64, cx: &Context<'_>) -> Poll<()> {
        if self.flushed_pos() >= target || self.error.is_some() || self.write_failed {
            return Poll::Ready(());
        }
        self.wait_for_flush(cx);
        Poll::Pending
    }

    fn current_error<T, E: From<io::Error>>(&mut self) -> Option<std::result::Result<T, E>> {
        let err = self.error.take().or_else(|| {
            self.write_failed
                .then(|| io::Error::other("an earlier write to this stream failed"))
        })?;
        Some(Err(err.into()))
    }

    fn must_truncate(&self) -> bool {
        if let FileStatus::Closed = self.file_status {
            return false;
        }
        self.flushed_pos() > self.aligned_pos
    }

    /// Flushes what is left, padded to the alignment, then truncates the
    /// padding away before syncing. The sync that close promises then covers
    /// the final size too; syncing first left the shrink to whenever the
    /// filesystem next committed, so a crash could bring the padding back.
    /// A truncate is synced even when an earlier `sync` already covered the
    /// data, since that sync saw the padded size.
    fn initiate_close(&mut self, state: Rc<RefCell<Self>>, file: Rc<DmaFile>, do_close: bool) {
        self.file_status = FileStatus::Closing;
        let final_pos = self.current_pos();
        self.flush_padded(state.clone(), file.clone());
        let mut pending = self.take_pending_handles();
        crate::spawn_local(async move {
            futures_lite::future::yield_now().await;

            defer! {
                let mut state = state.borrow_mut();
                state.file_status = FileStatus::Finished;
                let waiters = std::mem::take(&mut state.close_waiters);
                drop(state);
                waiters.into_iter().for_each(Waker::wake);
            }

            for flush in pending.drain(..) {
                flush.await;
            }
            futures_lite::future::poll_fn(|cx| state.borrow_mut().poll_flushed_to(final_pos, cx))
                .await;

            let (must_truncate, must_sync) = {
                let state = state.borrow();
                if state.error.is_some() || state.write_failed {
                    return;
                }

                assert_eq!(state.flushed_pos(), final_pos);
                let must_truncate = state.must_truncate();
                let must_sync =
                    state.sync_on_close && (state.synced_pos < final_pos || must_truncate);
                (must_truncate, must_sync)
            };

            if must_truncate {
                let res = file.truncate(final_pos).await;
                {
                    let mut state = state.borrow_mut();
                    if collect_error!(state, res) {
                        return;
                    }
                }
            }

            if must_sync {
                let res = file.fdatasync().await;
                {
                    let mut state = state.borrow_mut();
                    if collect_error!(state, res) {
                        return;
                    }
                    state.synced_pos = final_pos;
                }
            }

            if do_close {
                let res = file.close_rc().await;
                {
                    let mut state = state.borrow_mut();
                    collect_error!(state, res);
                }
            }
        })
        .detach();
    }

    fn current_pos(&self) -> u64 {
        self.aligned_pos + self.buffer_pos as u64
    }

    fn flushed_pos(&self) -> u64 {
        self.flush_state.flushed_pos
    }

    /// Whether a write of the partly filled block at `aligned_pos` is still
    /// in flight. Any further write to that block waits for it: two Direct
    /// I/O writes to one offset can complete in either order, and the older,
    /// shorter one landing last would put its zero padding over the newer
    /// bytes.
    fn partial_flush_in_flight(&self) -> bool {
        matches!(
            self.flush_state.flushes.back(),
            Some((pos, FlushStatus::Pending(_))) if *pos > self.aligned_pos
        )
    }

    fn pending_flush_count(&mut self) -> usize {
        self.flush_state.pending_flush_count
    }

    fn take_pending_handles(&mut self) -> Vec<task::JoinHandle<()>> {
        self.flush_state.take_pending_handles()
    }

    fn take_pending_flushes(&mut self) -> VecDeque<(u64, task::JoinHandle<()>)> {
        self.flush_state.take_pending_flushes()
    }

    fn restore_pending_flush(&mut self, flush_pos: u64, handle: task::JoinHandle<()>) {
        self.flush_state.restore_pending_flush(flush_pos, handle)
    }

    fn flush_padded(&mut self, state: Rc<RefCell<Self>>, file: Rc<DmaFile>) -> bool {
        if self.buffer_pos == 0 {
            return false;
        } else {
            assert!(
                self.buffer_pos < self.buffer_size,
                "full buffer should have already been flushed"
            );
        }
        let last_flush_pos = self.flush_state.last_flush_pos();
        let current_pos = self.current_pos();
        if last_flush_pos == current_pos {
            return false;
        } else {
            assert!(last_flush_pos < current_pos);
        }
        let mut buffer = self.current_buffer.take().unwrap();
        buffer.as_bytes_mut()[self.buffer_pos..].fill(0);
        if let FileStatus::Open = self.file_status {
            let mut buffer_copy = file.alloc_dma_buffer(self.buffer_size);
            buffer_copy.as_bytes_mut()[..self.buffer_pos]
                .copy_from_slice(&buffer.as_bytes()[..self.buffer_pos]);
            self.current_buffer = Some(buffer_copy);
        }
        self.flush_one_buffer(buffer, state, file);
        true
    }

    fn flush_one_buffer(&mut self, buffer: DmaBuffer, state: Rc<RefCell<Self>>, file: Rc<DmaFile>) {
        let aligned_pos = self.aligned_pos;
        let flush_pos = self.current_pos();
        let handle = crate::spawn_local(async move {
            let len = buffer.len();
            let res =
                file.write_at(buffer, aligned_pos)
                    .await
                    .and_then(|written| {
                        if written < len {
                            Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        format!("the file took {written} of the {len} bytes of an aligned write"),
                    )
                    .into())
                        } else {
                            Ok(written)
                        }
                    });
            let mut state = state.borrow_mut();
            if collect_error!(state, res) {
                state.write_failed = true;
            } else {
                state.flush_state.on_complete(flush_pos);
            }
            let waiters = std::mem::take(&mut state.flush_waiters);
            drop(state);
            waiters.into_iter().for_each(Waker::wake);
        })
        .detach();
        self.flush_state.on_start(flush_pos, handle);
    }
}

impl Drop for DmaStreamWriterState {
    fn drop(&mut self) {
        assert!(
            self.take_pending_handles().is_empty(),
            "DmaStreamerWriter::drop() should have cancelled pending flushes"
        );
    }
}

/// Cancelled writes never complete, so a close already waiting on them is
/// told the stream failed and woken, and lets go of the file.
impl Drop for DmaStreamWriter {
    fn drop(&mut self) {
        let mut state = self.state.borrow_mut();
        let mut pending = state.take_pending_handles();
        let waiters = if pending.is_empty() {
            Vec::new()
        } else {
            state.write_failed = true;
            std::mem::take(&mut state.flush_waiters)
        };
        for flush in pending.drain(..) {
            flush.cancel();
        }
        if let Some(file) = self.file.take().filter(|_| state.must_truncate()) {
            crate::executor()
                .reactor()
                .sys
                .async_truncate(file.as_raw_fd(), state.flushed_pos());
        }
        drop(state);
        waiters.into_iter().for_each(Waker::wake);
    }
}

#[derive(Debug)]
/// Provides linear access to a [`DmaFile`]. The [`DmaFile`] is a convenient way
/// to manage a file through Direct I/O, but its interface is conductive to
/// random access, as a position must always be specified.
///
/// Very rarely does one need to issue random writes to a file. Therefore, the
/// [`DmaStreamWriter`] is likely your go-to API when it comes to writing files.
///
/// The [`DmaStreamWriter`] implements [`AsyncWrite`]. Flushing writes out
/// everything written so far, including a partly filled buffer, which is
/// padded with zeros up to the buffer size, and waits for those writes to
/// complete. Later writes overwrite the padding, and closing truncates it
/// away. A flush does not sync; use [`sync`] for that. Closing the file
/// issues a sync so that the data can be flushed from the internal NVMe
/// caches, unless it was disabled with [`with_sync_on_close_disabled`].
///
/// [`sync`]: DmaStreamWriter::sync
/// [`with_sync_on_close_disabled`]: DmaStreamWriterBuilder::with_sync_on_close_disabled
/// [`DmaFile`]: struct.DmaFile.html
/// [`DmaStreamReader`]: struct.DmaStreamReader.html
/// [`get_buffer_aligned`]:
/// struct.DmaStreamReader.html#method.get_buffer_aligned
/// [`AsyncWrite`]: https://docs.rs/futures/0.3.5/futures/io/trait.AsyncWrite.html
pub struct DmaStreamWriter {
    file: RefCell<Option<Rc<DmaFile>>>,
    state: Rc<RefCell<DmaStreamWriterState>>,
}

impl DmaStreamWriter {
    fn new(builder: DmaStreamWriterBuilder) -> DmaStreamWriter {
        let state = DmaStreamWriterState {
            buffer_size: builder.buffer_size,
            write_behind: builder.write_behind,
            sync_on_close: builder.sync_on_close,
            current_buffer: None,
            close_waiters: Vec::new(),
            flush_state: DmaStreamWriterFlushState::new(builder.write_behind),
            error: None,
            file_status: FileStatus::Open,
            aligned_pos: 0,
            buffer_pos: 0,
            synced_pos: 0,
            write_failed: false,
            flush_waiters: Vec::new(),
        };

        let state = Rc::new(RefCell::new(state));
        DmaStreamWriter {
            file: RefCell::new(Some(builder.file)),
            state,
        }
    }

    /// Acquires the current position of this [`DmaStreamWriter`].
    ///
    /// # Examples
    /// ```no_run
    /// use futures::io::AsyncWriteExt;
    /// use glommio::{
    ///     io::{DmaFile, DmaStreamWriterBuilder},
    ///     LocalExecutor,
    /// };
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let file = DmaFile::create("myfile.txt").await.unwrap();
    ///     let mut writer = DmaStreamWriterBuilder::new(file).build();
    ///     assert_eq!(writer.current_pos(), 0);
    ///     writer.write_all(&[0, 1, 2, 3, 4]).await.unwrap();
    ///     assert_eq!(writer.current_pos(), 5);
    ///     writer.close().await.unwrap();
    /// });
    /// ```
    ///
    /// [`DmaStreamWriter`]: struct.DmaStreamWriter.html
    pub fn current_pos(&self) -> u64 {
        self.state.borrow().current_pos()
    }

    /// Acquires the current position of this [`DmaStreamWriter`] that is
    /// flushed to the underlying media.
    ///
    /// Warning: the position reported by this API is not restart or crash safe.
    /// You need to call [`sync`] for that. Although the DmaStreamWriter
    /// uses Direct I/O, modern storage devices have their own caches and
    /// may still lose data that sits on those caches upon a restart until
    /// [`sync`] is called (Note that [`close`] implies a sync).
    ///
    /// However within the same session, new readers trying to read from any
    /// position before what we return in this method will be guaranteed to
    /// read the data we just wrote.
    ///
    /// # Examples
    /// ```no_run
    /// use futures::io::AsyncWriteExt;
    /// use glommio::{
    ///     io::{DmaFile, DmaStreamWriterBuilder},
    ///     LocalExecutor,
    /// };
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let file = DmaFile::create("myfile.txt").await.unwrap();
    ///     let mut writer = DmaStreamWriterBuilder::new(file).build();
    ///     assert_eq!(writer.current_pos(), 0);
    ///     writer.write_all(&[0, 1, 2, 3, 4]).await.unwrap();
    ///     assert_eq!(writer.current_pos(), 5);
    ///     // The write above is not enough to cause a flush
    ///     assert_eq!(writer.current_flushed_pos(), 0);
    ///     writer.close().await.unwrap();
    ///     // Close implies a forced-flush and a sync.
    ///     assert_eq!(writer.current_flushed_pos(), 5);
    /// });
    /// ```
    ///
    /// [`DmaStreamWriter`]: struct.DmaStreamWriter.html
    /// [`sync`]: struct.DmaStreamWriter.html#method.sync
    /// [`close`]: https://docs.rs/futures/0.3.5/futures/io/trait.AsyncWriteExt.html#method.close
    pub fn current_flushed_pos(&self) -> u64 {
        self.state.borrow().flushed_pos()
    }

    /// Waits on the pending flushes through a guard that hands back those
    /// not yet awaited, so that a cancelled flush leaves them for `close`
    /// and `drop`.
    async fn flush_inner(&self, partial: bool) -> Result<u64> {
        let (target_pos, pending) = {
            let mut state = self.state.borrow_mut();
            if !matches!(state.file_status, FileStatus::Open) {
                return Err(closed_error().into());
            }
            let pos = if partial && state.buffer_pos > 0 {
                state.flush_padded(self.state.clone(), self.file.borrow().clone().unwrap());
                state.current_pos()
            } else {
                state.aligned_pos
            };
            (pos, state.take_pending_flushes())
        };
        let mut pending = scopeguard::guard(pending, |rest| {
            let mut state = self.state.borrow_mut();
            for (flush_pos, handle) in rest {
                state.restore_pending_flush(flush_pos, handle);
            }
        });
        while let Some((_, flush)) = pending.front_mut() {
            flush.await;
            pending.pop_front();
        }
        drop(ScopeGuard::into_inner(pending));
        futures_lite::future::poll_fn(|cx| self.state.borrow_mut().poll_flushed_to(target_pos, cx))
            .await;
        let mut state = self.state.borrow_mut();
        if let Some(err) = state.current_error() {
            return err;
        }
        assert!(state.flushed_pos() >= target_pos);
        Ok(state.flushed_pos())
    }

    async fn sync_inner(&self) -> Result<u64> {
        let (flushed_pos, presync_pos) = {
            let state = self.state.borrow();
            (state.flushed_pos(), state.synced_pos)
        };
        if presync_pos < flushed_pos {
            let Some(file) = self.file.borrow().clone() else {
                return Err(closed_error().into());
            };
            file.fdatasync().await?;
            let mut state = self.state.borrow_mut();
            state.synced_pos = state.synced_pos.max(flushed_pos);
        } else {
            assert_eq!(presync_pos, flushed_pos);
        }
        Ok(flushed_pos)
    }

    /// Waits for all currently in-flight buffers to be written to the
    /// underlying storage.
    ///
    /// This does not include the current buffer if it is not full. If all data
    /// must be flushed, use [`flush`].
    ///
    /// Returns the flushed position of the file.
    ///
    /// # Examples
    /// ```no_run
    /// use futures::io::AsyncWriteExt;
    /// use glommio::{
    ///     io::{DmaFile, DmaStreamWriterBuilder},
    ///     LocalExecutor,
    /// };
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let file = DmaFile::create("myfile.txt").await.unwrap();
    ///     let mut writer = DmaStreamWriterBuilder::new(file)
    ///         .with_buffer_size(4096)
    ///         .with_write_behind(2)
    ///         .build();
    ///     let buffer = [0u8; 5000];
    ///     writer.write_all(&buffer).await.unwrap();
    ///     // with 5000 bytes written into a 4096-byte buffer a flush
    ///     // has certainly started. But if very likely didn't finish right
    ///     // away.
    ///     assert_eq!(writer.current_flushed_pos(), 0);
    ///     assert_eq!(writer.flush_aligned().await.unwrap(), 4096);
    ///     writer.close().await.unwrap();
    /// });
    /// ```
    ///
    /// [`flush`]: https://docs.rs/futures/0.3.15/futures/io/trait.AsyncWriteExt.html#method.flush
    pub async fn flush_aligned(&self) -> Result<u64> {
        self.flush_inner(false).await
    }

    /// Waits for all currently in-flight buffers to be written to the
    /// underlying storage, and ensures they are safely persisted.
    ///
    /// This does not include the current buffer if it is not full. If all data
    /// must be synced, use [`Self::sync`].
    ///
    /// Returns the flushed position of the file at the time the sync started.
    ///
    /// # Examples
    /// ```no_run
    /// use futures::io::AsyncWriteExt;
    /// use glommio::{
    ///     io::{DmaFile, DmaStreamWriterBuilder},
    ///     LocalExecutor,
    /// };
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let file = DmaFile::create("myfile.txt").await.unwrap();
    ///     let mut writer = DmaStreamWriterBuilder::new(file)
    ///         .with_buffer_size(4096)
    ///         .with_write_behind(2)
    ///         .build();
    ///     let buffer = [0u8; 5000];
    ///     writer.write_all(&buffer).await.unwrap();
    ///     // with 5000 bytes written into a 4096-byte buffer a flush
    ///     // has certainly started. But if very likely didn't finish right
    ///     // away.
    ///     assert_eq!(writer.current_flushed_pos(), 0);
    ///     assert_eq!(writer.sync_aligned().await.unwrap(), 4096);
    ///     writer.close().await.unwrap();
    /// });
    /// ```
    pub async fn sync_aligned(&self) -> Result<u64> {
        self.flush_aligned().await?;
        self.sync_inner().await
    }

    /// Waits for all buffers to be written to the underlying storage, and
    /// ensures they are safely persisted.
    ///
    /// This includes the current buffer even if it is not full, by padding it.
    /// The padding will get over-written by future writes, or truncated upon
    /// [`close`].
    ///
    /// Returns the flushed position of the file at the time the sync started.
    ///
    /// # Examples
    /// ```no_run
    /// use futures::io::AsyncWriteExt;
    /// use glommio::{
    ///     io::{DmaFile, DmaStreamWriterBuilder},
    ///     LocalExecutor,
    /// };
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let file = DmaFile::create("myfile.txt").await.unwrap();
    ///     let mut writer = DmaStreamWriterBuilder::new(file)
    ///         .with_buffer_size(4096)
    ///         .with_write_behind(2)
    ///         .build();
    ///     let buffer = [0u8; 5000];
    ///     writer.write_all(&buffer).await.unwrap();
    ///     // with 5000 bytes written into a 4096-byte buffer a flush
    ///     // has certainly started. But if very likely didn't finish right
    ///     // away.
    ///     assert_eq!(writer.current_flushed_pos(), 0);
    ///     assert_eq!(writer.sync().await.unwrap(), 5000);
    ///     writer.close().await.unwrap();
    /// });
    /// ```
    ///
    /// [`close`]: https://docs.rs/futures/0.3.15/futures/io/trait.AsyncWriteExt.html#method.close
    pub async fn sync(&self) -> Result<u64> {
        self.flush_inner(true).await?;
        self.sync_inner().await
    }

    /// Internal function that does everything that close does (flushes
    /// buffers, etc), but leaves the file open. Useful for the immutable file
    /// abstraction.
    pub(super) fn poll_seal(
        &mut self,
        cx: &Context<'_>,
    ) -> Poll<io::Result<DmaStreamReaderBuilder>> {
        let mut state = self.state.borrow_mut();
        match state.file_status {
            FileStatus::Open => {
                let file = ensure_not_closed!(self.file.clone());
                let waiting = std::mem::take(&mut state.flush_waiters);
                state.wait_for_close(cx);
                state.initiate_close(self.state.clone(), file, false);
                drop(state);
                waiting.into_iter().for_each(Waker::wake);
                Poll::Pending
            }
            FileStatus::Closing => {
                state.wait_for_close(cx);
                Poll::Pending
            }
            FileStatus::Finished => {
                state.file_status = FileStatus::Closed;
                let file = ensure_not_closed!(self.file);

                let wb = state.write_behind;
                let bufsz = state.buffer_size;
                let builder = DmaStreamReaderBuilder::from_rc(file)
                    .with_read_ahead(wb)
                    .with_buffer_size(bufsz);

                match state.current_error() {
                    Some(err) => Poll::Ready(err),
                    None => Poll::Ready(Ok(builder)),
                }
            }
            FileStatus::Closed => {
                already_closed!()
            }
        }
    }
}

impl AsyncWrite for DmaStreamWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut &*self).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut &*self).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut &*self).poll_close(cx)
    }
}

impl AsyncWrite for &DmaStreamWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.state.borrow_mut();
        if !matches!(state.file_status, FileStatus::Open) {
            return already_closed!();
        }
        if let Some(err) = state.current_error() {
            return Poll::Ready(err);
        }
        if !buf.is_empty() && state.partial_flush_in_flight() {
            state.wait_for_flush(cx);
            return Poll::Pending;
        }

        let mut written = 0;
        while written < buf.len() {
            match state.current_buffer.take() {
                None => {
                    if state.pending_flush_count() < state.write_behind {
                        state.current_buffer = Some(
                            self.file
                                .borrow()
                                .as_ref()
                                .unwrap()
                                .alloc_dma_buffer(state.buffer_size),
                        );
                    } else {
                        break;
                    }
                }
                Some(mut buffer) => {
                    let writesz = buffer.write_at(state.buffer_pos, &buf[written..]);
                    written += writesz;
                    state.buffer_pos += writesz;
                    if state.buffer_pos == state.buffer_size {
                        state.flush_one_buffer(
                            buffer,
                            self.state.clone(),
                            self.file.borrow().clone().unwrap(),
                        );
                        state.aligned_pos += state.buffer_size as u64;
                        state.buffer_pos = 0;
                    } else {
                        state.current_buffer = Some(buffer);
                    }
                }
            }
        }

        if written == 0 && !buf.is_empty() {
            state.wait_for_flush(cx);
            Poll::Pending
        } else {
            Poll::Ready(Ok(written))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.state.borrow_mut();
        if matches!(
            state.file_status,
            FileStatus::Closing | FileStatus::Finished
        ) {
            return already_closed!();
        }
        if let Some(err) = state.current_error() {
            return Poll::Ready(err);
        }
        if state.flushed_pos() == state.current_pos() {
            return Poll::Ready(Ok(()));
        }
        if !matches!(state.file_status, FileStatus::Open) {
            return already_closed!();
        }
        state.wait_for_flush(cx);
        state.flush_padded(self.state.clone(), self.file.borrow().clone().unwrap());
        Poll::Pending
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.state.borrow_mut();
        match state.file_status {
            FileStatus::Open => {
                let waiting = std::mem::take(&mut state.flush_waiters);
                state.wait_for_close(cx);
                let file = self.file.borrow_mut().take();
                state.initiate_close(self.state.clone(), file.unwrap(), true);
                drop(state);
                waiting.into_iter().for_each(Waker::wake);
                Poll::Pending
            }
            FileStatus::Closing => {
                state.wait_for_close(cx);
                Poll::Pending
            }
            FileStatus::Finished => {
                state.file_status = FileStatus::Closed;
                match state.current_error() {
                    Some(err) => Poll::Ready(err),
                    None => Poll::Ready(Ok(())),
                }
            }
            FileStatus::Closed => {
                already_closed!()
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{enclose, io::dma_file::align_up, test_utils::make_test_directories, timer::Timer};
    use futures::{task::noop_waker_ref, AsyncRead, AsyncReadExt, AsyncWriteExt};
    use rusty_fork::rusty_fork_test;
    use std::{io::ErrorKind, path::Path, time::Duration};

    rusty_fork_test! {
        /// A buffer the kernel writes only part of, here at the process's
        /// file size limit, fails the stream instead of counting as written.
        /// Forked, since the limit is per process.
        #[test]
        fn a_short_dma_write_fails_the_stream() {
            let dir = make_test_directories("short-dma-write").remove(0);
            crate::test_utils::limit_file_size(4096);
            test_executor!(async move {
                let path = dir.path.join("out");
                let file = DmaFile::create(&path).await.unwrap();
                let mut writer = DmaStreamWriterBuilder::new(file)
                    .with_buffer_size(8192)
                    .build();
                let wrote = async {
                    writer.write_all(&[1; 8192]).await?;
                    writer.close().await
                };
                wrote
                    .await
                    .expect_err("a buffer cut short at the limit was reported as written");
            });
        }
    }

    const NUM_CONCURRENT_WRITERS: usize = 10;

    macro_rules! file_stream_read_test {
        ( $name:ident, $dir:ident, $kind:ident, $file:ident, $file_size:ident: $size:tt, $code:block) => {
            #[test]
            fn $name() {
                for dir in make_test_directories(stringify!($name)) {
                    let $dir = dir.path.clone();
                    let $kind = dir.kind;
                    test_executor!(async move {
                        let filename = $dir.join("testfile");
                        let new_file = DmaFile::create(&filename)
                            .await
                            .expect("failed to create file");
                        if $size > 0 {
                            let bufsz = align_up($size, 4096);
                            let mut buf = DmaBuffer::new(bufsz as _).unwrap();
                            for (v, x) in buf.as_bytes_mut().iter_mut().enumerate() {
                                *x = v as u8;
                            }
                            new_file.write_at(buf, 0).await.unwrap();
                            if bufsz != $size {
                                new_file.truncate($size).await.unwrap();
                            }
                        }
                        new_file.close().await.unwrap();
                        let $file = DmaFile::open(&filename).await.unwrap();
                        let $file_size = $size;
                        $code
                    });
                }
            }
        };
    }

    macro_rules! file_stream_write_test {
        ( $name:ident, $dir:ident, $kind:ident, $filename:ident, $file:ident, $code:block) => {
            #[test]
            fn $name() {
                for dir in make_test_directories(stringify!($name)) {
                    let $dir = dir.path.clone();
                    let $kind = dir.kind;
                    test_executor!(async move {
                        let $filename = $dir.join("testfile");
                        let $file = DmaFile::create(&$filename).await.unwrap();
                        $code
                    });
                }
            }
        };
    }

    macro_rules! multiple_file_streams_write_test {
        ( $name:ident, $dir:ident, $kind:ident, $filenames:ident, $files:ident, $code:block) => {
            #[test]
            fn $name() {
                for dir in make_test_directories(stringify!($name)) {
                    let $dir = dir.path.clone();
                    let $kind = dir.kind;
                    test_executor!(async move {
                        let $filenames = (0..NUM_CONCURRENT_WRITERS)
                            .into_iter()
                            .map(|i| $dir.join(format!("testfile-{}", i)))
                            .collect::<Vec<_>>();

                        let mut $files = Vec::with_capacity($filenames.len());
                        for filename in $filenames.iter() {
                            $files.push(DmaFile::create(filename).await.unwrap());
                        }
                        let $files = $files;

                        $code
                    });
                }
            }
        };
    }

    macro_rules! check_contents {
        ( $buf:expr, $start:expr ) => {
            for (idx, i) in $buf.iter().enumerate() {
                assert_eq!(*i, ($start + (idx as u64)) as u8);
            }
        };
    }

    fn file_size<P: AsRef<Path>>(path: P) -> u64 {
        std::fs::metadata(path).unwrap().len()
    }

    file_stream_read_test!(read_exact_empty_file, path, _k, file, _file_size: 0, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .build();

        let mut buf = [0u8; 128];
        match reader.read_exact(&mut buf).await {
            Err(x) => match x.kind() {
                ErrorKind::UnexpectedEof => {},
                _ => panic!("unexpected error"),
            }
            _ => panic!("unexpected success"),
        }
        reader.close().await.unwrap();
    });

    file_stream_write_test!(write_nothing, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file).build();
        let wrote = crate::timer::timeout(Duration::from_secs(1), async {
            Ok(writer.write(&[]).await)
        })
        .await
        .expect("an empty write never returned");
        assert_eq!(wrote.unwrap(), 0);
        writer.write_all(b"after").await.unwrap();
        writer.close().await.unwrap();
        assert_eq!(std::fs::read(&filename).unwrap(), b"after");
    });

    file_stream_write_test!(close_truncates_before_it_syncs, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file).build();
        writer.write_all(&[7; 100]).await.unwrap();
        let recording = crate::sys::test_support::record_file_ops();
        writer.close().await.unwrap();
        assert_eq!(
            recording.take(),
            ["truncate", "fdatasync"],
            "the sync that close promises has to cover the final size"
        );
        assert_eq!(std::fs::read(&filename).unwrap(), [7; 100]);
    });

    file_stream_write_test!(
        close_after_a_sync_still_syncs_its_truncate,
        path,
        _k,
        filename,
        file,
        {
            let mut writer = DmaStreamWriterBuilder::new(file).build();
            writer.write_all(&[7; 100]).await.unwrap();
            writer.sync().await.unwrap();
            let recording = crate::sys::test_support::record_file_ops();
            writer.close().await.unwrap();
            assert_eq!(
                recording.take(),
                ["truncate", "fdatasync"],
                "the earlier sync covered the padded size, not the final one"
            );
            assert_eq!(std::fs::read(&filename).unwrap(), [7; 100]);
        }
    );

    file_stream_write_test!(close_after_a_cancelled_flush, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(4096)
            .with_write_behind(2)
            .build();
        writer.write_all(&[0xAB; 4096]).await.unwrap();
        {
            let flush = writer.flush_aligned();
            futures_lite::pin!(flush);
            assert!(futures_lite::future::poll_once(&mut flush).await.is_none());
        }
        writer.close().await.unwrap();
        assert_eq!(std::fs::read(&filename).unwrap(), vec![0xAB; 4096]);
    });

    /// Guards against a read after a failed one waiting forever on a buffer
    /// whose read-ahead gave up when it saw the failure.
    #[test]
    fn a_read_after_a_failed_read_returns() {
        for dir in make_test_directories("read-after-failed-read") {
            test_executor!(async move {
                let path = dir.path.join("testfile");
                std::fs::write(&path, vec![7u8; 64 << 10]).unwrap();
                let file = crate::io::OpenOptions::new()
                    .write(true)
                    .dma_open(&path)
                    .await
                    .unwrap();
                let mut reader = DmaStreamReaderBuilder::new(file)
                    .with_buffer_size(4096)
                    .with_read_ahead(4)
                    .build();
                let mut buf = [0u8; 16];
                for attempt in 0..8 {
                    let read = crate::timer::timeout(Duration::from_secs(1), async {
                        Ok(reader.read(&mut buf).await)
                    })
                    .await;
                    assert!(
                        read.expect("a read never returned").is_err(),
                        "read {attempt}"
                    );
                }
                let _ = reader.close().await;
            });
        }
    }

    /// Guards against a read-ahead that skipped its read because another had
    /// failed leaving its buffer marked as being read, so that asking for it
    /// waited forever.
    #[test]
    fn a_read_ahead_skipped_after_a_failure_is_read_again() {
        for dir in make_test_directories("read-ahead-skipped") {
            test_executor!(async move {
                let path = dir.path.join("testfile");
                std::fs::write(&path, vec![7u8; 64 << 10]).unwrap();
                let file = DmaFile::open(&path).await.unwrap();
                let mut reader = DmaStreamReaderBuilder::new(file)
                    .with_buffer_size(4096)
                    .with_read_ahead(4)
                    .build();
                reader.state.borrow_mut().error =
                    Some(io::Error::other("a read that failed elsewhere"));
                crate::timer::sleep(Duration::from_millis(10)).await;
                let mut buf = [0u8; 16];
                assert!(reader.read(&mut buf).await.is_err());
                let read = crate::timer::timeout(Duration::from_secs(1), async {
                    Ok(reader.read(&mut buf).await)
                })
                .await
                .expect("the read never returned");
                assert_eq!(read.unwrap(), 16);
                assert_eq!(buf, [7u8; 16]);
                reader.close().await.unwrap();
            });
        }
    }

    file_stream_write_test!(write_more_than_write_behind, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(128 << 10)
            .with_write_behind(2)
            .build();

        for i in 0..1_000_000 {
            assert_eq!(writer.current_pos(), i);
            writer.write_all(&[i as u8]).await.unwrap();
        }

        writer.close().await.unwrap();

        let rfile = DmaFile::open(&filename).await.unwrap();
        assert_eq!(rfile.file_size().await.unwrap(), 1_000_000);
    });

    #[test]
    fn a_partial_flush_pads_with_zeroes() {
        let dir = make_test_directories("partial-flush-padding").remove(0);
        test_executor!(async move {
            let other = dir.path.join("other");
            std::fs::write(&other, vec![0xABu8; 4096]).unwrap();
            let other = DmaFile::open(&other).await.unwrap();
            drop(other.read_at(0, 4096).await.unwrap());
            other.close().await.unwrap();

            let path = dir.path.join("out");
            let writer = DmaStreamWriterBuilder::new(DmaFile::create(&path).await.unwrap())
                .with_buffer_size(4096)
                .build();
            (&writer).write_all(&[7u8; 100]).await.unwrap();
            writer.sync().await.unwrap();
            let on_disk = std::fs::read(&path).unwrap();
            assert!(on_disk[..100].iter().all(|&b| b == 7));
            assert!(on_disk[100..].iter().all(|&b| b == 0));
            drop(writer);
        });
    }

    /// A buffer size that is a multiple of the alignment but not a power of
    /// two still reads every byte from where it is in the file.
    #[test]
    fn read_with_a_buffer_size_that_is_not_a_power_of_two() {
        let dir = make_test_directories("read-npot-buffer").remove(0);
        test_executor!(async move {
            let filename = dir.path.join("testfile");
            let pattern = |v: usize| (v / 4096) as u8 ^ v as u8;
            let size = 16 * 4096;
            let file = DmaFile::create(&filename).await.unwrap();
            let mut buf = DmaBuffer::new(size).unwrap();
            for (v, x) in buf.as_bytes_mut().iter_mut().enumerate() {
                *x = pattern(v);
            }
            file.write_at(buf, 0).await.unwrap();
            file.close().await.unwrap();

            for start in [0, 13000] {
                let file = DmaFile::open(&filename).await.unwrap();
                let mut reader = DmaStreamReaderBuilder::new(file)
                    .with_buffer_size(3 * 4096)
                    .with_start_pos(start as u64)
                    .build();
                let mut out = Vec::new();
                reader.read_to_end(&mut out).await.unwrap();
                reader.close().await.unwrap();
                let expected: Vec<u8> = (start..size).map(pattern).collect();
                assert!(out == expected, "wrong bytes reading from {start}");
            }
        });
    }

    file_stream_read_test!(read_exact_zero_buffer, path, _k, file, _file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(0)
            .build();

        let mut buf = [0u8; 1024];
        reader.read_exact(&mut buf).await.unwrap();
        check_contents!(buf, 0);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_exact_single_buffer, path, _k, file, _file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(64 << 10)
            .build();

        let mut buf = [0u8; 1024];
        reader.read_exact(&mut buf).await.unwrap();
        check_contents!(buf, 0);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_exact_crosses_buffer, path, _k, file, _file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(1 << 10)
            .build();

        let mut buf = [0u8; 8000];
        reader.read_exact(&mut buf).await.unwrap();
        check_contents!(buf, 0);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_exact_skip, path, _k, file, _file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(1 << 10)
            .build();

        let mut buf = [0u8; 128];
        reader.skip(1000);
        reader.read_exact(&mut buf).await.unwrap();
        check_contents!(buf, 1000);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_exact_late_start, path, _k, file, file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_start_pos(file_size - 100)
            .with_buffer_size(1 << 10)
            .build();

        let mut buf = [0u8; 128];
        match reader.read_exact(&mut buf).await {
            Err(x) => match x.kind() {
                ErrorKind::UnexpectedEof => {},
                _ => panic!("unexpected error"),
            }
            _ => panic!("unexpected success"),
        }
        assert_eq!(reader.current_pos(), 128 << 10);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_exact_early_eof, path, _k, file, _file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_end_pos(100)
            .with_buffer_size(1 << 10)
            .build();

        let mut buf = [0u8; 128];
        match reader.read_exact(&mut buf).await {
            Err(x) => match x.kind() {
                ErrorKind::UnexpectedEof => {},
                _ => panic!("unexpected error"),
            }
            _ => panic!("unexpected success"),
        }
        assert_eq!(reader.current_pos(), 100);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_exact_unaligned_file_size, path, _k, file, file_size: 131074, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(1 << 10)
            .build();

        let mut buf = vec![0; file_size];
        reader.read_exact(&mut buf).await.unwrap();
        check_contents!(buf, 0);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_to_end_no_read_ahead, path, _k, file, file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(4096)
            .with_read_ahead(0)
            .build();

        let mut buf = Vec::new();
        let x = reader.read_to_end(&mut buf).await.unwrap();
        assert_eq!(x, file_size);
        check_contents!(buf, 0);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_to_end_of_empty_file, path, _k, file, _file_size: 0, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .build();

        let mut buf = Vec::new();
        let x = reader.read_to_end(&mut buf).await.unwrap();
        assert_eq!(x, 0);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_to_end_single_buffer, path, _k, file, file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(file_size)
            .build();

        let mut buf = Vec::new();
        let x = reader.read_to_end(&mut buf).await.unwrap();
        assert_eq!(x, file_size);
        check_contents!(buf, 0);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_to_end_buffer_crossing, path, _k, file, file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(1024)
            .build();

        let mut buf = Vec::new();
        let x = reader.read_to_end(&mut buf).await.unwrap();
        assert_eq!(x as u64, file_size);
        check_contents!(buf, 0);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_to_end_buffer_crossing_read_ahead, path, _k, file, file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(4096)
            .with_read_ahead(64)
            .build();

        let mut buf = Vec::new();
        let x = reader.read_to_end(&mut buf).await.unwrap();
        assert_eq!(x as u64, file_size);
        check_contents!(buf, 0);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_to_end_after_skip, path, _k, file, file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(4096)
            .with_read_ahead(2)
            .build();

        let mut buf = Vec::new();
        reader.skip(file_size - 128);
        let x = reader.read_to_end(&mut buf).await.unwrap();
        assert_eq!(x as u64, 128);
        check_contents!(buf, file_size - 128);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_simple, path, _k, file, file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .build();

        let mut buf = [0u8; 4096];
        for i in 0..100 {
            let x = reader.read(&mut buf).await.unwrap();
            if i < file_size / 4096 {
                assert_eq!(x, 4096);
                check_contents!(buf, {i * 4096});
            } else {
                assert_eq!(x, 0);
            }
        }
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_get_buffer_aligned, path, _k, file, _file_size: 131074, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(1024)
            .build();

        let buffer = reader.get_buffer_aligned(24).await.unwrap();
        assert_eq!(buffer.len(), 24);
        check_contents!(*buffer, 0);

        reader.skip(980);

        let buffer = reader.get_buffer_aligned(8).await.unwrap();
        assert_eq!(buffer.len(), 8);
        check_contents!(*buffer, 1004);
        reader.skip((128 << 10) - 1012);
        let eof_short_buffer = reader.get_buffer_aligned(4).await.unwrap();
        assert_eq!(eof_short_buffer.len(), 2);
        check_contents!(*eof_short_buffer, 131072);

        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_get_buffer_aligned_cross_boundaries, path, _k, file, _file_size: 16384, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(4096)
            .build();

        reader.skip(4094);

        match reader.get_buffer_aligned(130).await {
            Err(_) => panic!("Expected partial success"),
            Ok(res) => {
                assert_eq!(res.len(), 2);
                check_contents!(*res, 4094);
            },
        }
        assert_eq!(reader.current_pos(), 4096);

        match reader.get_buffer_aligned(64).await {
            Err(_) => panic!("Expected success"),
            Ok(res) => {
                assert_eq!(res.len(), 64);
                check_contents!(*res, 4096);
            },
        }
        assert_eq!(reader.current_pos(), 4160);

        reader.skip(12160);

        match reader.get_buffer_aligned(128).await {
            Err(_) => panic!("Expected success"),
            Ok(res) => {
                assert_eq!(res.len(), 64);
                check_contents!(*res, 1984);
            },
        }
        assert_eq!(reader.current_pos(), 16384);

        let eof = reader.get_buffer_aligned(64).await.unwrap();
        assert_eq!(eof.len(), 0);

        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_get_buffer_aligned_zero_buffer, path, _k, file, _file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(131072)
            .build();

        let buffer = reader.get_buffer_aligned(0).await.unwrap();
        assert_eq!(buffer.len(), 0);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_get_buffer_aligned_entire_buffer_at_once, path, _k, file, _file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(131072)
            .build();

        let buffer = reader.get_buffer_aligned(131072).await.unwrap();
        assert_eq!(buffer.len(), 131072);
        check_contents!(*buffer, 0);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_get_buffer_aligned_past_the_end, path, _k, file, _file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(131072)
            .build();

        reader.skip(131073);
        let buffer = reader.get_buffer_aligned(32).await.unwrap();
        assert_eq!(buffer.len(), 0);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(read_mixed_api, path, _k, file, _file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(1024)
            .build();

        let buffer = reader.get_buffer_aligned(24).await.unwrap();
        assert_eq!(buffer.len(), 24);
        check_contents!(*buffer, 0);

        let mut buf = [0u8; 2000];
        reader.read_exact(&mut buf).await.unwrap();
        check_contents!(buf, 24);
        assert_eq!(reader.current_pos(), 2024);

        reader.close().await.unwrap();
    });

    #[track_caller]
    fn expect_specific_error<T>(
        op: std::result::Result<T, io::Error>,
        expected_kind: Option<io::ErrorKind>,
        expected_err: &'static str,
    ) {
        match op {
            Ok(_) => panic!("should have failed"),
            Err(err) => {
                if let Some(expected_kind) = expected_kind {
                    assert_eq!(
                        expected_kind,
                        err.kind(),
                        "expected {:?}, got {:?}",
                        expected_kind,
                        err.kind()
                    );
                }

                let x = format!("{}", err.into_inner().unwrap());
                assert!(
                    x.starts_with(expected_err),
                    "expected {} got {}",
                    expected_err,
                    x
                );
            }
        }
    }

    file_stream_read_test!(read_wronly_file, path, _k, _file, _file_size: 131072, {
        let newfile = path.join("wronly_file");
        let rfile = DmaFile::create(&newfile).await.unwrap();
        let mut reader = DmaStreamReaderBuilder::new(rfile)
            .with_buffer_size(1024)
            .build();

        let mut buf = [0u8; 2000];
        expect_specific_error(reader.read_exact(&mut buf).await, None, "Bad file descriptor (os error 9)");
        reader.close().await.unwrap();
    });

    file_stream_read_test!(poll_reader_repeatedly, path, _k, file, _file_size: 131072, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_read_ahead(0)
            .build();

        let mut buf = [0u8; 2000];
        let mut cx = std::task::Context::from_waker(noop_waker_ref());

        assert!(matches!(Pin::new(&mut reader).poll_read(&mut cx, &mut buf), Poll::Pending));
        assert!(matches!(Pin::new(&mut reader).poll_read(&mut cx, &mut buf), Poll::Pending));

        reader.close().await.unwrap();
    });

    file_stream_write_test!(write_all, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(128 << 10)
            .build();

        writer.write_all(&[0, 1, 2, 3, 4]).await.unwrap();

        assert_eq!(writer.current_pos(), 5);
        writer.close().await.unwrap();
        assert_eq!(writer.current_pos(), 5);

        let rfile = DmaFile::open(&filename).await.unwrap();
        assert_eq!(rfile.file_size().await.unwrap(), 5);

        let mut reader = DmaStreamReaderBuilder::new(rfile)
            .with_buffer_size(1024)
            .build();

        let mut buf = Vec::new();
        let x = reader.read_to_end(&mut buf).await.unwrap();
        assert_eq!(x, 5);
        check_contents!(buf, 0);
        reader.close().await.unwrap();
    });

    file_stream_write_test!(write_multibuffer, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(1024)
            .with_write_behind(2)
            .build();

        for i in 0..4096 {
            assert_eq!(writer.current_pos(), i);
            writer.write_all(&[i as u8]).await.unwrap();
        }

        writer.close().await.unwrap();

        let rfile = DmaFile::open(&filename).await.unwrap();
        assert_eq!(rfile.file_size().await.unwrap(), 4096);

        let mut reader = DmaStreamReaderBuilder::new(rfile)
            .with_buffer_size(1024)
            .build();

        let mut buf = [0u8; 4096];
        reader.read_exact(&mut buf).await.unwrap();
        check_contents!(buf, 0);
        reader.close().await.unwrap();
    });

    file_stream_write_test!(write_close_twice, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(128 << 10)
            .build();

        writer.close().await.unwrap();
        expect_specific_error(
            writer.close().await,
            None,
            "Bad file descriptor (os error 9)",
        );
    });

    file_stream_write_test!(write_no_write_behind, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(1024)
            .with_write_behind(0)
            .build();

        for i in 0..4096 {
            assert_eq!(writer.current_pos(), i);
            writer.write_all(&[i as u8]).await.unwrap();
        }

        writer.close().await.unwrap();

        let rfile = DmaFile::open(&filename).await.unwrap();
        assert_eq!(rfile.file_size().await.unwrap(), 4096);

        let mut reader = DmaStreamReaderBuilder::new(rfile)
            .with_buffer_size(1024)
            .build();

        let mut buf = [0u8; 4096];
        reader.read_exact(&mut buf).await.unwrap();
        check_contents!(buf, 0);
        reader.close().await.unwrap();
    });

    file_stream_write_test!(write_zero_buffer, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(0)
            .build();

        for i in 0..4096 {
            assert_eq!(writer.current_pos(), i);
            writer.write_all(&[i as u8]).await.unwrap();
        }

        writer.close().await.unwrap();

        let rfile = DmaFile::open(&filename).await.unwrap();
        assert_eq!(rfile.file_size().await.unwrap(), 4096);

        let mut reader = DmaStreamReaderBuilder::new(rfile)
            .with_buffer_size(1024)
            .build();

        let mut buf = [0u8; 4096];
        reader.read_exact(&mut buf).await.unwrap();
        check_contents!(buf, 0);
        reader.close().await.unwrap();
    });

    file_stream_write_test!(write_with_readable_file, path, _k, filename, _file, {
        let rfile = DmaFile::open(&filename).await.unwrap();

        let mut writer = DmaStreamWriterBuilder::new(rfile)
            .with_buffer_size(4096)
            .build();

        for i in 0..1024 {
            assert_eq!(writer.current_pos(), i);
            writer.write_all(&[i as u8]).await.unwrap();
        }

        expect_specific_error(
            writer.close().await,
            None,
            "Bad file descriptor (os error 9)",
        );
    });

    file_stream_write_test!(flushed_position_small_buffer, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(4096)
            .build();

        assert_eq!(writer.current_pos(), 0);
        writer.write_all(&[0, 1, 2, 3, 4]).await.unwrap();
        assert_eq!(writer.current_pos(), 5);
        assert_eq!(
            writer.current_flushed_pos(),
            0,
            "the write above is not enough to cause a flush"
        );
        writer.close().await.unwrap();
        assert_eq!(
            writer.current_flushed_pos(),
            5,
            "close implies a forced-flush and a sync"
        );
    });

    file_stream_write_test!(flushed_position_big_buffer, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(4096)
            .with_write_behind(2)
            .build();

        assert_eq!(writer.current_pos(), 0);
        let buffer = [0u8; 5000];
        writer.write_all(&buffer).await.unwrap();

        assert_eq!(writer.current_flushed_pos(), 0);
        Timer::new(Duration::from_secs(1)).await;
        assert_eq!(writer.current_flushed_pos(), 4096);

        writer.close().await.unwrap();
        assert_eq!(writer.current_flushed_pos(), 5000);
    });

    file_stream_write_test!(flush_and_drop, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(4096)
            .with_write_behind(2)
            .build();

        assert_eq!(writer.current_pos(), 0);
        let buffer = [0u8; 5000];
        writer.write_all(&buffer).await.unwrap();
        assert_eq!(writer.flush_aligned().await.unwrap(), 4096);
        assert_eq!(file_size(&filename), 4096);
        writer.flush().await.unwrap();
        assert_eq!(writer.current_flushed_pos(), 5000);
        assert_eq!(file_size(&filename), 8192);
        drop(writer);
        assert_eq!(file_size(&filename), 5000);
    });

    file_stream_write_test!(sync_and_close, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(4096)
            .with_write_behind(2)
            .build();

        assert_eq!(writer.current_pos(), 0);
        let buffer = [0u8; 5000];
        writer.write_all(&buffer).await.unwrap();
        assert_eq!(writer.sync().await.unwrap(), 5000);
        assert_eq!(file_size(&filename), 8192);
        writer.write_all(&buffer).await.unwrap();
        writer.close().await.unwrap();

        assert_eq!(writer.current_flushed_pos(), 10000);
        assert_eq!(file_size(&filename), 10000);
    });

    file_stream_write_test!(close_after_cancelled_write, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(4096)
            .with_write_behind(1)
            .build();

        let data = [7u8; 3 * 4096];
        {
            let mut write = Box::pin(writer.write_all(&data));
            assert!(futures_lite::future::poll_once(&mut write).await.is_none());
        }
        writer.close().await.unwrap();

        assert_eq!(writer.current_flushed_pos(), 4096);
        assert_eq!(file_size(&filename), 4096);
    });

    file_stream_write_test!(write_after_close, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(4096)
            .build();

        writer.write_all(&[1u8; 10]).await.unwrap();
        writer.close().await.unwrap();

        writer.write(&[1u8; 10]).await.unwrap_err();
        writer.flush().await.unwrap();
        assert_eq!(file_size(&filename), 10);
    });

    file_stream_write_test!(close_while_another_task_writes, path, _k, filename, file, {
        let writer = Rc::new(
            DmaStreamWriterBuilder::new(file)
                .with_buffer_size(4096)
                .with_write_behind(1)
                .build(),
        );

        let other = crate::spawn_local(enclose! { (writer) async move {
            (&*writer).write_all(&[7u8; 3 * 4096]).await
        }});
        crate::executor().yield_task_queue_now().await;

        crate::timer::timeout(Duration::from_secs(1), async move {
            (&*writer).close().await.unwrap();
            other.await.unwrap_err();
            Ok(())
        })
        .await
        .expect("closing next to a waiting writer hung");
        assert_eq!(file_size(&filename), 4096);
    });

    file_stream_write_test!(
        close_while_another_task_flushes,
        path,
        _k,
        filename,
        file,
        {
            let writer = Rc::new(
                DmaStreamWriterBuilder::new(file)
                    .with_buffer_size(4096)
                    .build(),
            );
            (&*writer).write_all(&[7u8; 10]).await.unwrap();

            let other = crate::spawn_local(enclose! { (writer) async move {
                (&*writer).flush().await
            }});
            crate::executor().yield_task_queue_now().await;

            let close = crate::spawn_local(async move { (&*writer).close().await });
            crate::timer::timeout(Duration::from_secs(1), async move {
                close.await.unwrap();
                let _ = other.await;
                Ok(())
            })
            .await
            .expect("closing next to a waiting flush hung");
            assert_eq!(file_size(&filename), 10);
        }
    );

    file_stream_write_test!(
        two_aligned_flushes_at_once_both_wait,
        path,
        _k,
        filename,
        file,
        {
            let writer = DmaStreamWriterBuilder::new(file)
                .with_buffer_size(4096)
                .build();
            let mut w = &writer;
            w.write_all(&[7u8; 4096 + 10]).await.unwrap();
            let (first, second) = futures::join!(writer.flush_aligned(), writer.flush_aligned());
            assert_eq!(first.unwrap(), 4096);
            assert_eq!(
                second.unwrap(),
                4096,
                "the second flush did not wait for the first's writes"
            );
            w.close().await.unwrap();
            assert_eq!(file_size(&filename), 4096 + 10);
        }
    );

    file_stream_write_test!(a_repolled_flush_registers_once, path, _k, filename, file, {
        let writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(4096)
            .build();
        let mut w = &writer;
        w.write_all(&[7u8; 4096]).await.unwrap();
        {
            let mut first = Box::pin(writer.flush_aligned());
            assert!(futures_lite::future::poll_once(&mut first).await.is_none());
            let mut second = Box::pin(writer.flush_aligned());
            let mut cx = Context::from_waker(noop_waker_ref());
            for _ in 0..3 {
                assert!(std::future::Future::poll(second.as_mut(), &mut cx).is_pending());
            }
            assert_eq!(writer.state.borrow().flush_waiters.len(), 1);
        }
        w.close().await.unwrap();
        assert_eq!(file_size(&filename), 4096);
    });

    file_stream_write_test!(
        a_dropped_writer_releases_a_waiting_close,
        path,
        _k,
        filename,
        file,
        {
            let writer = DmaStreamWriterBuilder::new(file)
                .with_buffer_size(4096)
                .build();
            let state = Rc::downgrade(&writer.state);
            (&writer).write_all(&[7u8; 4096]).await.unwrap();
            {
                let mut flush = Box::pin(writer.flush_aligned());
                assert!(futures_lite::future::poll_once(&mut flush).await.is_none());
                let mut w = &writer;
                let mut close = Box::pin(w.close());
                assert!(futures_lite::future::poll_once(&mut close).await.is_none());
                for _ in 0..4 {
                    futures_lite::future::yield_now().await;
                }
                let state = writer.state.borrow();
                assert_eq!(state.flushed_pos(), 0, "the write finished too early");
                assert!(!state.flush_waiters.is_empty(), "close is not waiting");
            }
            drop(writer);
            for _ in 0..4 {
                futures_lite::future::yield_now().await;
            }
            assert!(
                state.upgrade().is_none(),
                "the close task still holds the stream"
            );
        }
    );

    file_stream_write_test!(
        flushes_and_syncs_after_close_fail,
        path,
        _k,
        filename,
        file,
        {
            let writer = DmaStreamWriterBuilder::new(file)
                .with_buffer_size(4096)
                .build();
            let mut w = &writer;
            w.write_all(&[7u8; 100]).await.unwrap();
            w.close().await.unwrap();
            writer.sync().await.expect_err("sync after close");
            writer
                .sync_aligned()
                .await
                .expect_err("sync_aligned after close");
            writer
                .flush_aligned()
                .await
                .expect_err("flush_aligned after close");
            assert_eq!(file_size(&filename), 100);
        }
    );

    file_stream_write_test!(write_after_cancelled_flush, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(4096)
            .build();

        writer.write_all(&[7u8; 10]).await.unwrap();
        {
            let mut flush = Box::pin(writer.flush());
            assert!(futures_lite::future::poll_once(&mut flush).await.is_none());
        }
        assert_eq!(writer.write(&[7u8; 10]).await.unwrap(), 10);

        writer.close().await.unwrap();
        assert_eq!(std::fs::read(&filename).unwrap(), [7u8; 20]);
    });

    file_stream_write_test!(
        a_write_waits_for_the_padded_flush_of_its_block,
        path,
        _k,
        filename,
        file,
        {
            let mut writer = DmaStreamWriterBuilder::new(file)
                .with_buffer_size(4096)
                .build();
            writer.write_all(&[1u8; 10]).await.unwrap();
            {
                let mut flush = Box::pin(writer.flush());
                assert!(futures_lite::future::poll_once(&mut flush).await.is_none());
            }
            let fill = vec![2u8; 4086];
            {
                let mut write = Box::pin(writer.write(&fill));
                assert!(
                    futures_lite::future::poll_once(&mut write).await.is_none(),
                    "a write to the block went out beside the padded flush still writing it"
                );
                assert_eq!(write.await.unwrap(), 4086);
            }
            writer.close().await.unwrap();
            let mut expected = vec![1u8; 10];
            expected.extend_from_slice(&fill);
            assert_eq!(std::fs::read(&filename).unwrap(), expected);
        }
    );

    file_stream_write_test!(
        a_flush_during_close_leaves_close_its_error,
        path,
        _k,
        filename,
        file,
        {
            let mut writer = DmaStreamWriterBuilder::new(file)
                .with_buffer_size(4096)
                .build();
            writer.write_all(&[1u8; 10]).await.unwrap();
            {
                let mut close = Box::pin(writer.close());
                assert!(futures_lite::future::poll_once(&mut close).await.is_none());
            }
            while !matches!(writer.state.borrow().file_status, FileStatus::Finished) {
                Timer::new(Duration::from_millis(1)).await;
            }
            writer.state.borrow_mut().error = Some(io::Error::other("the close failed"));
            assert!((&writer).flush().await.is_err());
            assert!(
                writer.close().await.is_err(),
                "a flush during the close took the close's error"
            );
        }
    );

    file_stream_write_test!(
        two_writers_waiting_on_one_block_both_wake,
        path,
        _k,
        filename,
        file,
        {
            let writer = Rc::new(
                DmaStreamWriterBuilder::new(file)
                    .with_buffer_size(4096)
                    .build(),
            );
            (&*writer).write_all(&[1u8; 10]).await.unwrap();
            {
                let mut w = &*writer;
                let mut flush = Box::pin(w.flush());
                assert!(futures_lite::future::poll_once(&mut flush).await.is_none());
            }
            let first = crate::spawn_local(enclose! { (writer) async move {
                (&*writer).write_all(&[2u8; 5]).await
            }});
            let second = crate::spawn_local(enclose! { (writer) async move {
                (&*writer).write_all(&[2u8; 5]).await
            }});
            crate::executor().yield_task_queue_now().await;
            assert_eq!(
                writer.current_flushed_pos(),
                0,
                "the flush finished too early"
            );

            crate::timer::timeout(Duration::from_secs(1), async move {
                second.await.unwrap();
                first.await.unwrap();
                Ok(())
            })
            .await
            .expect("a writer waiting beside another was never woken");
            (&*writer).close().await.unwrap();
            assert_eq!(file_size(&filename), 20);
        }
    );

    file_stream_write_test!(two_flushes_at_once_both_finish, path, _k, filename, file, {
        let writer = Rc::new(
            DmaStreamWriterBuilder::new(file)
                .with_buffer_size(4096)
                .build(),
        );
        (&*writer).write_all(&[1u8; 10]).await.unwrap();
        let first = crate::spawn_local(enclose! { (writer) async move {
            (&*writer).flush().await
        }});
        let second = crate::spawn_local(enclose! { (writer) async move {
            (&*writer).flush().await
        }});
        crate::executor().yield_task_queue_now().await;
        assert_eq!(
            writer.current_flushed_pos(),
            0,
            "the flush finished too early"
        );

        crate::timer::timeout(Duration::from_secs(1), async move {
            second.await.unwrap();
            first.await.unwrap();
            Ok(())
        })
        .await
        .expect("a flush waiting beside another was never woken");
        (&*writer).close().await.unwrap();
        assert_eq!(file_size(&filename), 10);
    });

    file_stream_write_test!(two_closes_at_once_both_finish, path, _k, filename, file, {
        let writer = Rc::new(
            DmaStreamWriterBuilder::new(file)
                .with_buffer_size(4096)
                .build(),
        );
        (&*writer).write_all(&[1u8; 10]).await.unwrap();
        let first = crate::spawn_local(enclose! { (writer) async move {
            (&*writer).close().await
        }});
        let second = crate::spawn_local(enclose! { (writer) async move {
            (&*writer).close().await
        }});
        crate::executor().yield_task_queue_now().await;
        assert!(
            !matches!(writer.state.borrow().file_status, FileStatus::Finished),
            "the close finished too early"
        );

        crate::timer::timeout(Duration::from_secs(1), async move {
            let results = [second.await, first.await];
            assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
            Ok(())
        })
        .await
        .expect("a close waiting beside another was never woken");
        assert_eq!(file_size(&filename), 10);
    });

    thread_local! {
        static WOKEN_STATE: RefCell<Option<Rc<RefCell<DmaStreamWriterState>>>> =
            const { RefCell::new(None) };
        static STATE_FREE_ON_WAKE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    }

    struct BorrowOnWake;

    impl futures::task::ArcWake for BorrowOnWake {
        fn wake_by_ref(_: &std::sync::Arc<Self>) {
            let free = WOKEN_STATE.with(|state| {
                state
                    .borrow()
                    .as_ref()
                    .map(|state| state.try_borrow_mut().is_ok())
            });
            STATE_FREE_ON_WAKE.with(|cell| cell.set(free));
        }
    }

    file_stream_write_test!(
        close_wakes_a_waiting_writer_outside_the_borrow,
        path,
        _k,
        filename,
        file,
        {
            let mut writer = DmaStreamWriterBuilder::new(file)
                .with_buffer_size(4096)
                .build();
            writer.write_all(&[1u8; 10]).await.unwrap();
            {
                let mut flush = Box::pin(writer.flush());
                assert!(futures_lite::future::poll_once(&mut flush).await.is_none());
            }
            WOKEN_STATE.with(|state| *state.borrow_mut() = Some(writer.state.clone()));
            let waker = futures::task::waker(std::sync::Arc::new(BorrowOnWake));
            let mut cx = Context::from_waker(&waker);
            assert!(Pin::new(&mut &writer)
                .poll_write(&mut cx, &[2u8; 5])
                .is_pending());
            {
                let mut close = Box::pin(writer.close());
                assert!(futures_lite::future::poll_once(&mut close).await.is_none());
            }
            WOKEN_STATE.with(|state| state.borrow_mut().take());
            assert_eq!(STATE_FREE_ON_WAKE.with(std::cell::Cell::get), Some(true));
            writer.close().await.unwrap();
        }
    );

    #[test]
    fn flushstate() {
        test_executor!(async move {
            let handle_gen = || crate::spawn_local(async move {}).detach();
            let mut state = DmaStreamWriterFlushState::new(4);

            state.on_start(8, handle_gen());
            assert_eq!(
                state.take_pending_handles().len(),
                1,
                "flushes: [8->Pending(Some)]"
            );
            assert_eq!(
                state.take_pending_handles().len(),
                0,
                "flushes: [8->Pending(None)]"
            );
            assert_eq!(state.flushes.len(), 1, "flushes: [8->Pending(None)]");
            assert_eq!(state.pending_flush_count, 1);
            assert_eq!(state.on_complete(8), 8);
            assert_eq!(state.flushes.len(), 0, "flushes: []");

            state.on_start(16, handle_gen());
            state.on_start(24, handle_gen());
            state.on_start(32, handle_gen());
            assert_eq!(
                state.flushes.len(),
                3,
                "flushes: [16->Pending, 24->Pending, 32->Pending]"
            );
            assert_eq!(state.on_complete(24), 8);
            assert_eq!(
                state.flushes.len(),
                3,
                "flushes: [16->Pending, 24->Complete, 32->Pending]"
            );
            assert_eq!(state.on_complete(32), 8);
            assert_eq!(state.pending_flush_count, 1);
            assert_eq!(
                32,
                state.on_complete(16),
                "flushes: [16->Pending, 24->Complete, 32->Complete]"
            );
            assert_eq!(state.flushes.len(), 0, "flushes: []");
        });
    }

    multiple_file_streams_write_test!(join_write_multiple_writers, path, _k, filenames, files, {
        let mut writers = files
            .into_iter()
            .map(|file| {
                DmaStreamWriterBuilder::new(file)
                    .with_buffer_size(4096)
                    .with_write_behind(1)
                    .build()
            })
            .collect::<Vec<_>>();

        let buffer = [0u8; 5000];
        let results = join_all(writers.iter_mut().map(|w| w.write_all(&buffer))).await;
        for result in results {
            result.unwrap();
        }

        for mut writer in writers {
            writer.close().await.unwrap();
        }
    });

    multiple_file_streams_write_test!(join_close_multiple_writers, path, _k, filenames, files, {
        let mut writers = files
            .into_iter()
            .map(|file| {
                DmaStreamWriterBuilder::new(file)
                    .with_buffer_size(8)
                    .with_write_behind(16)
                    .build()
            })
            .collect::<Vec<_>>();

        let buffer = [0u8; 5000];
        for writer in &mut writers {
            writer.write_all(&buffer).await.unwrap();
        }

        let results = join_all(writers.iter_mut().map(|w| w.close())).await;
        for result in results {
            result.unwrap();
        }

        for (writer, filename) in writers.into_iter().zip(filenames) {
            assert_eq!(writer.current_flushed_pos(), 5000);
            assert_eq!(file_size(&filename), 5000);
        }
    });

    file_stream_write_test!(drop_while_closing, path, _k, filename, file, {
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(4096)
            .build();
        writer.write_all(&[1u8; 5]).await.unwrap();
        {
            let mut close = Box::pin(writer.close());
            assert!(futures_lite::future::poll_once(&mut close).await.is_none());
        }
        while writer.current_flushed_pos() < 5 {
            Timer::new(Duration::from_millis(1)).await;
        }
        drop(writer);
    });

    file_stream_write_test!(failed_flush_keeps_failing, path, _k, filename, file, {
        file.close().await.unwrap();
        let file = DmaFile::open(&filename).await.unwrap();
        let writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(4096)
            .build();
        (&writer).write_all(&[1u8; 4096]).await.unwrap();
        writer.flush_aligned().await.unwrap_err();
        writer.flush_aligned().await.unwrap_err();
        writer.sync().await.unwrap_err();
        (&writer).flush().await.unwrap_err();
        (&writer).write_all(&[1u8; 5]).await.unwrap_err();
        (&writer).close().await.unwrap_err();
    });

    file_stream_write_test!(failed_flush_fails_seal, path, _k, filename, file, {
        file.close().await.unwrap();
        let file = DmaFile::open(&filename).await.unwrap();
        let mut writer = DmaStreamWriterBuilder::new(file)
            .with_buffer_size(4096)
            .build();
        (&writer).write_all(&[1u8; 4096]).await.unwrap();
        writer.flush_aligned().await.unwrap_err();
        std::future::poll_fn(|cx| writer.poll_seal(cx))
            .await
            .unwrap_err();
    });

    file_stream_read_test!(a_reader_that_starts_past_its_end_reads_nothing, path, _k, file, _file_size: 8192, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_start_pos(4096)
            .with_end_pos(1024)
            .with_buffer_size(1 << 10)
            .build();
        assert!(reader.get_buffer_aligned(512).await.unwrap().is_empty());
        let mut buf = [0u8; 128];
        assert_eq!(reader.read(&mut buf).await.unwrap(), 0);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(skipping_further_than_the_end_stops_at_the_end, path, _k, file, _file_size: 8192, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_end_pos(8192)
            .with_buffer_size(1 << 10)
            .build();
        reader.skip(100);
        reader.skip(u64::MAX);
        assert_eq!(reader.current_pos(), 8192);
        let mut buf = [0u8; 128];
        assert_eq!(reader.read(&mut buf).await.unwrap(), 0);
        reader.close().await.unwrap();
    });

    file_stream_read_test!(skipping_from_past_the_end_does_not_move_back, path, _k, file, _file_size: 8192, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_start_pos(4096)
            .with_end_pos(1024)
            .with_buffer_size(1 << 10)
            .build();
        reader.skip(1);
        assert_eq!(reader.current_pos(), 4096);
        reader.close().await.unwrap();
    });

    /// A reader with no end skipped to the last position must not walk every
    /// buffer in between. It runs on a thread with a deadline so a walk shows
    /// as a failed assertion rather than a hung test run.
    #[test]
    fn skipping_to_the_end_of_an_unbounded_reader_returns_at_once() {
        let dir = make_test_directories("skip-unbounded").remove(0);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            test_executor!(async move {
                let file = DmaFile::create(dir.path.join("testfile")).await.unwrap();
                let mut reader = DmaStreamReaderBuilder::new(file)
                    .with_buffer_size(1 << 10)
                    .build();
                reader.skip(u64::MAX);
                tx.send(reader.current_pos()).unwrap();
                let _ = reader.close().await;
            });
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)),
            Ok(u64::MAX),
            "skip did not return"
        );
    }

    file_stream_read_test!(asking_for_more_than_is_left_returns_what_is_left, path, _k, file, _file_size: 8192, {
        let mut reader = DmaStreamReaderBuilder::new(file)
            .with_buffer_size(1 << 10)
            .build();
        reader.skip(1000);
        let read = reader.get_buffer_aligned(u64::MAX).await.unwrap();
        assert_eq!(read.len(), 24);
        check_contents!(*read, 1000);
        reader.close().await.unwrap();
    });
}
