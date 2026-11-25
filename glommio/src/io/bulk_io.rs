use crate::io::{
    dma_file::{align_down, align_up},
    DmaFile, ReadResult, ScheduledSource,
};
use core::task::{Context, Poll};
use futures_lite::{ready, Stream, StreamExt};
use std::{
    cmp::{max, min},
    collections::VecDeque,
    os::unix::io::AsRawFd,
    pin::Pin,
    rc::Rc,
};

/// Set a limit to the size of merged IO requests.
#[derive(Debug, Default)]
pub enum MergedBufferLimit {
    /// Disables request coalescing
    NoMerging,

    /// Sets the limit to the maximum the kernel allows for the underlying
    /// device without breaking down the request into smaller ones
    /// (/sys/block/.../queue/max_sectors_kb)
    #[default]
    DeviceMaxSingleRequest,

    /// Sets a custom limit.
    /// This value should be a multiple of the file's alignment. If it is not,
    /// it'll be aligned down.
    Custom(usize),
}

/// Set a limit to the amount of read amplification in-between two mergeable IO
/// requests.
#[derive(Debug, Default)]
pub enum ReadAmplificationLimit {
    /// Deny read amplification.
    ///
    /// Note that, because IO request coalescing is done _before_ buffers are
    /// aligned, requests may be merged if they are smaller than the minimum IO
    /// size. For instance, if the minimum IO size is 4KiB and the user reads
    /// [0..256] and [2048..2560] then the two will be merged into [0..4096] to
    /// accommodate the 4KiB minimum IO size.
    #[default]
    NoAmplification,

    /// Merge two consecutive IO requests if the read amplification is below a
    /// limit.
    ///
    /// This value doesn't have any alignment constrain.
    Custom(usize),

    /// Always merge successive IO requests if possible, no matter the distance
    /// between them. This is likely not what you want.
    NoLimit,
}

/// An interface to an IO vector.
pub trait IoVec {
    /// The read position (the offset) in the file
    fn pos(&self) -> u64;
    /// The number of bytes to read at [`Self::pos`]
    fn size(&self) -> usize;
}

impl IoVec for (u64, usize) {
    fn pos(&self) -> u64 {
        self.0
    }

    fn size(&self) -> usize {
        self.1
    }
}

pub(crate) struct MergedIOVecs<V: IoVec> {
    pub(crate) coalesced_user_iovecs: VecDeque<V>,
    pub(crate) pos: u64,
    pub(crate) size: usize,
}

impl<V: IoVec> IoVec for MergedIOVecs<V> {
    fn pos(&self) -> u64 {
        self.pos
    }

    fn size(&self) -> usize {
        self.size
    }
}

impl<V: IoVec> MergedIOVecs<V> {
    fn deduplicate(&mut self, mut other: Self) -> Option<Self> {
        if self.pos() == other.pos() && self.size() == other.size() {
            self.coalesced_user_iovecs
                .append(&mut other.coalesced_user_iovecs);
            None
        } else {
            Some(std::mem::replace(self, other))
        }
    }
}

impl<V: IoVec> Iterator for MergedIOVecs<V> {
    type Item = (V, (u64, usize));

    fn next(&mut self) -> Option<Self::Item> {
        self.coalesced_user_iovecs
            .pop_front()
            .map(|io| (io, (self.pos(), self.size())))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (
            self.coalesced_user_iovecs.len(),
            Some(self.coalesced_user_iovecs.len()),
        )
    }
}

struct IOVecMerger<V: IoVec> {
    max_merged_buffer_size: usize,
    max_read_amp: Option<usize>,

    current: Option<(u64, usize)>,
    merged: VecDeque<V>,
}

impl<V: IoVec> IOVecMerger<V> {
    pub(super) fn merge(&mut self, io: V) -> Option<MergedIOVecs<V>> {
        let (pos, size) = (io.pos(), io.size());
        match self.current {
            None => {
                self.current = Some((pos, size));
                self.merged.push_back(io);
                None
            }
            Some(cur) => {
                if let Some(gap) = self.max_read_amp {
                    if u64::saturating_sub(cur.pos(), pos + size as u64) > gap as u64
                        || u64::saturating_sub(pos, cur.pos() + cur.size() as u64) > gap as u64
                    {
                        let (pos, size) = self.current.replace((pos, size)).unwrap();
                        self.merged.push_front(io);

                        return Some(MergedIOVecs {
                            coalesced_user_iovecs: self.merged.drain(1..).collect(),
                            pos,
                            size,
                        });
                    }
                }
                let merged: (u64, u64) = (
                    min(cur.pos(), pos),
                    max(cur.pos() + cur.size() as u64, pos + size as u64),
                );
                if merged.1 - merged.0 > self.max_merged_buffer_size as u64 {
                    let (pos, size) = self.current.replace((pos, size)).unwrap();
                    self.merged.push_front(io);

                    return Some(MergedIOVecs {
                        coalesced_user_iovecs: self.merged.drain(1..).collect(),
                        pos,
                        size,
                    });
                }
                self.merged.push_back(io);
                self.current = Some((merged.0, (merged.1 - merged.0) as usize));
                None
            }
        }
    }

    pub(super) fn flush(&mut self) -> Option<MergedIOVecs<V>> {
        self.current.take().map(|x| MergedIOVecs {
            coalesced_user_iovecs: std::mem::take(&mut self.merged),
            pos: x.pos(),
            size: x.size(),
        })
    }
}

pub(crate) struct CoalescedReads<V: IoVec + Unpin, S: Stream<Item = V> + Unpin> {
    iter: S,
    merger: Option<IOVecMerger<V>>,
    alignment: Option<u64>,

    last: Option<MergedIOVecs<V>>,
}

impl<V: IoVec + Unpin, S: Stream<Item = V> + Unpin> CoalescedReads<V, S> {
    pub(crate) fn new(
        max_merged_buffer_size: usize,
        max_read_amp: Option<usize>,
        alignment: Option<u64>,
        iter: S,
    ) -> CoalescedReads<V, S> {
        CoalescedReads {
            iter,
            merger: Some(IOVecMerger {
                max_merged_buffer_size,
                max_read_amp,
                current: None,
                merged: Default::default(),
            }),
            alignment,
            last: None,
        }
    }
}

impl<V: IoVec + Unpin, S: Stream<Item = V> + Unpin> Stream for CoalescedReads<V, S> {
    /// `CoalescedReads` returns the original (offset, size) and the
    /// (offset, size) it was merged in.
    type Item = MergedIOVecs<V>;

    /// Pulls IO requests from the underlying stream and attempts to merge
    /// them with the previous ones, if any. To avoid adding undo latency, we
    /// flush whatever is in the merger if the underlying stream returns
    /// `Poll::Pending`.
    ///
    /// When the underlying stream is closed, pull out of the merger whatever
    /// is there, if anything. The merger is gone from then on, which is how
    /// a later call knows not to poll the underlying stream again: it need
    /// not be fused, and some streams panic when polled after their end.
    ///
    /// Two subsequent merged IO requests may ask for the exact same data
    /// (because we align then up and down after merging) so there is one more
    /// opportunity to deduplicate here.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let align = |mut merged_iovec: MergedIOVecs<V>, alignment: u64| {
            let (pos, size) = (merged_iovec.pos(), merged_iovec.size());
            let eff_pos = align_down(pos, alignment);
            let b = (pos - eff_pos) as usize;
            merged_iovec.pos = eff_pos;
            merged_iovec.size = align_up((size + b) as u64, alignment) as usize;
            merged_iovec
        };

        let next_inner = |this: &mut Self, cx: &mut Context<'_>| {
            let Some(merger) = this.merger.as_mut() else {
                return Poll::Ready(None);
            };
            loop {
                match this.iter.poll_next(cx) {
                    Poll::Ready(Some(io)) => {
                        if let Some(merged) = merger.merge(io) {
                            return Poll::Ready(Some(merged));
                        }
                    }
                    Poll::Pending => {
                        return if let Some(merged) = merger.flush() {
                            Poll::Ready(Some(merged))
                        } else {
                            Poll::Pending
                        };
                    }
                    _ => break,
                }
            }

            Poll::Ready(if let Some(mut merger) = this.merger.take() {
                merger.flush()
            } else {
                None
            })
        };

        let this = self.get_mut();
        loop {
            let mut inner = match next_inner(this, cx) {
                Poll::Ready(Some(inner)) => inner,
                Poll::Ready(None) => return Poll::Ready(this.last.take()),
                Poll::Pending => {
                    return this
                        .last
                        .take()
                        .map_or(Poll::Pending, |last| Poll::Ready(Some(last)))
                }
            };
            if let Some(alignment) = this.alignment {
                inner = align(inner, alignment);
            }

            if let Some(last) = &mut this.last {
                if let Some(last) = last.deduplicate(inner) {
                    return Poll::Ready(Some(last));
                }
            } else {
                this.last = Some(inner);
            }
        }
    }

    /// The upper bound counts the user reads still to come out, those held
    /// for merging included. Each merged read carries at least one, so it
    /// bounds the merged reads too, and [`ReadManyResult`], which yields one
    /// item per user read, relies on it counting them.
    fn size_hint(&self) -> (usize, Option<usize>) {
        let last = self
            .last
            .as_ref()
            .map_or(0, |last| last.coalesced_user_iovecs.len());
        let Some(merger) = &self.merger else {
            return (usize::from(last > 0), Some(last));
        };
        let held = last + merger.merged.len();
        let (low, high) = self.iter.size_hint();
        (
            usize::from(held > 0 || low > 0),
            high.and_then(|high| high.checked_add(held)),
        )
    }
}

#[derive(Debug)]
pub(crate) struct OrderedBulkIo<U: IoVec + Unpin, S: Stream<Item = (ScheduledSource, U)> + Unpin> {
    file: Rc<DmaFile>,
    iovs: S,

    inflight: VecDeque<(ScheduledSource, U)>,
    concurrency_cap: usize,
    inflight_memory: usize,
    memory_cap: usize,
    terminated: bool,
}

impl<U: IoVec + Unpin, S: Stream<Item = (ScheduledSource, U)> + Unpin> OrderedBulkIo<U, S> {
    pub(crate) fn new(file: Rc<DmaFile>, concurrency: usize, iovs: S) -> OrderedBulkIo<U, S> {
        assert!(concurrency > 0);
        OrderedBulkIo {
            file,
            iovs,
            inflight: VecDeque::with_capacity(concurrency),
            concurrency_cap: concurrency,
            inflight_memory: 0,
            memory_cap: usize::MAX,
            terminated: false,
        }
    }

    pub(crate) fn set_concurrency_limit(&mut self, limit: usize) {
        assert!(limit > 0, "concurrency must be at least 1");
        assert!(
            !self.terminated && self.inflight.is_empty(),
            "should be called before the first call to poll()"
        );
        self.inflight.reserve(limit);
        self.concurrency_cap = limit
    }

    pub(crate) fn set_memory_limit(&mut self, limit: Option<usize>) {
        assert!(
            !self.terminated && self.inflight.is_empty(),
            "should be called before the first call to poll()"
        );
        self.memory_cap = limit.unwrap_or(usize::MAX)
    }

    fn is_full(&self) -> bool {
        self.inflight.len() == self.concurrency_cap || self.inflight_memory > self.memory_cap
    }
}

impl<U: IoVec + Unpin, S: Stream<Item = (ScheduledSource, U)> + Unpin> Stream
    for OrderedBulkIo<U, S>
{
    type Item = (ScheduledSource, U);

    /// Poll the underlying stream and insert the resulting source, if any,
    /// in the local buffer; poll the local buffer for a fulfilled source, if
    /// any, and replace it with a new one from the underlying stream.
    ///
    /// When we have a source with a result in the buffer, we take it out and
    /// replace it with a new one from the stream, if any and if the buffer
    /// is not full without it, so the memory limit holds after small reads
    /// give way to large ones. When we have a source in the buffer but it's
    /// not ready yet, we register the buffer and return.
    ///
    /// When the internal buffer is full, we consume it instead of creating
    /// new ones. Otherwise fill the internal buffer as much as possible, and
    /// if there is anything we can return immediately, do so.
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        let poll_inner = |this: &mut Self, cx: &mut Context<'_>| match this.iovs.poll_next(cx) {
            Poll::Ready(Some(res)) => {
                this.inflight_memory += res.1.size();
                this.inflight.push_front(res);
                Poll::Ready(())
            }
            Poll::Ready(None) => {
                this.terminated = true;
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        };

        let poll_buffer = |this: &mut Self, cx: &mut Context<'_>| {
            if this.inflight.back_mut().unwrap().0.result().is_some() {
                let ret = this.inflight.pop_back().unwrap();
                this.inflight_memory -= ret.1.size();
                if !this.terminated && !this.is_full() {
                    let _ = poll_inner(this, cx);
                }
                Poll::Ready(Some(ret))
            } else {
                this.inflight
                    .back_mut()
                    .unwrap()
                    .0
                    .add_waiter_many(cx.waker().clone());
                Poll::Pending
            }
        };

        if this.is_full() || (this.terminated && !this.inflight.is_empty()) {
            poll_buffer(this, cx)
        } else {
            while !this.is_full() && !this.terminated {
                if poll_inner(this, cx).is_pending() {
                    break;
                }
            }

            if !this.inflight.is_empty() {
                poll_buffer(this, cx)
            } else if this.terminated {
                Poll::Ready(None)
            } else {
                Poll::Pending
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self.iovs.size_hint() {
            (low, Some(hi)) => (low + self.inflight.len(), Some(hi + self.inflight.len())),
            (low, None) => (low + self.inflight.len(), None),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReadManyArgs<V: IoVec + Unpin> {
    pub(crate) user_reads: VecDeque<V>,
    pub(crate) system_read: (u64, usize),
}

impl<V: IoVec + Unpin> IoVec for ReadManyArgs<V> {
    fn pos(&self) -> u64 {
        self.system_read.pos()
    }

    fn size(&self) -> usize {
        self.system_read.size()
    }
}

/// A stream of ReadResult produced asynchronously.
///
/// See [`DmaFile::read_many`] for more information
#[derive(Debug)]
pub struct ReadManyResult<
    V: IoVec + Unpin,
    S: Stream<Item = (ScheduledSource, ReadManyArgs<V>)> + Unpin,
> {
    pub(crate) inner: OrderedBulkIo<ReadManyArgs<V>, S>,
    pub(crate) current: Option<(ScheduledSource, ReadManyArgs<V>, usize)>,
}

impl<V: IoVec + Unpin, S: Stream<Item = (ScheduledSource, ReadManyArgs<V>)> + Unpin>
    ReadManyResult<V, S>
{
    /// Set the amount of IO concurrency of this stream, i.e., the number of IO
    /// requests this stream will submit at any one time
    ///
    /// Higher concurrency levels may improve performance and will extend the
    /// lifetime of IO requests, meaning they have a greater chance of being
    /// reused via IO request deduplication.
    /// However, higher values will increase memory usage and possibly starve
    /// by-standing IO-emitting tasks.
    ///
    /// This function should be called before the stream is first polled.
    ///
    /// # Panics
    ///
    /// Panics if `concurrency` is zero, since no read could ever be
    /// submitted, or if the stream has already been polled.
    #[must_use]
    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.inner.set_concurrency_limit(concurrency);
        self
    }

    /// Set a limit to the number of concurrent IO requests based on memory
    /// usage.
    ///
    /// This is a useful knob in conjunction with IO requests coalescing because
    /// it makes sense to keep a high number of concurrent small IO requests but
    /// less so if they are extremely large. i.e., if all the IO requests are
    /// 4MiB large, it doesn't make much sense to schedule more than a few at a
    /// time. Scheduling too many at once could starve other concurrent IO tasks
    /// for no throughput benefit. Conversely, It makes sense to schedule 4KiB
    /// requests with a higher concurrency level.
    ///
    /// Note that this is a soft limit. No matter how small this limit is set
    /// to, a single IO request will always be allowed to run. This can happen
    /// if you configure the IO merging logic very aggressively. A limit of
    /// `Some(0)` therefore runs one IO request at a time.
    ///
    /// Defaults to no limit.
    ///
    /// # Panics
    ///
    /// Panics if the stream has already been polled.
    #[must_use]
    pub fn with_memory_limit(mut self, limit: Option<usize>) -> Self {
        self.inner.set_memory_limit(limit);
        self
    }
}

impl<V: IoVec + Unpin, S: Stream<Item = (ScheduledSource, ReadManyArgs<V>)> + Unpin> Stream
    for ReadManyResult<V, S>
{
    type Item = super::Result<(V, ReadResult)>;

    /// Hands out the user reads carved from the current system read, and lets
    /// go of it with the last one, so its buffer lives only as long as the
    /// results that point into it.
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some((source, args, read)) = &mut self.current {
            if let Some(io) = args.user_reads.pop_front() {
                let offset = (io.pos() - args.system_read.pos()) as usize;
                let len = read.saturating_sub(offset).min(io.size());
                let result = ReadResult::from_sliced_buffer(source.clone(), offset, len);
                if args.user_reads.is_empty() {
                    self.current = None;
                }
                return Poll::Ready(Some(Ok((io, result))));
            }
        }

        match ready!(self.inner.poll_next(cx)) {
            None => Poll::Ready(None),
            Some((source, args)) => {
                let read = enhanced_try!(source.result().unwrap(), "Reading", self.inner.file)?;
                let read = source.bytes_in_range(read);
                self.current = Some((source, args, read));
                self.poll_next(cx)
            }
        }
    }

    /// A failed system read yields one error for all of its user reads, so
    /// the lower bound counts system reads and the upper bound user reads.
    fn size_hint(&self) -> (usize, Option<usize>) {
        let current = self
            .current
            .as_ref()
            .map_or(0, |(_, args, _)| args.user_reads.len());
        let inflight = &self.inner.inflight;
        let inflight_reads: usize = inflight.iter().map(|(_, args)| args.user_reads.len()).sum();
        let (low, high) = self.inner.iovs.size_hint();
        (
            low.saturating_add(current + inflight.len()),
            high.and_then(|high| high.checked_add(current + inflight_reads)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_lite::{stream, StreamExt};

    async fn collect_iovecs<V: IoVec, S: Stream<Item = MergedIOVecs<V>> + Unpin>(
        stream: S,
    ) -> Vec<(V, (u64, usize))> {
        stream
            .collect::<Vec<S::Item>>()
            .await
            .into_iter()
            .flatten()
            .collect()
    }

    /// A stream polled again after it ended may panic, as `unfold` does, so
    /// coalescing stops polling it at its first `None`.
    #[test]
    fn an_ended_stream_is_not_polled_again() {
        test_executor!(async move {
            let reads = futures::stream::unfold(0u64, |i| async move {
                (i < 3).then(|| ((i * 8192, 64usize), i + 1))
            });
            let merged =
                collect_iovecs(CoalescedReads::new(4096, None, None, Box::pin(reads))).await;
            assert_eq!(merged.len(), 3);
        });
    }

    #[test]
    fn monotonic_iovec_merging() {
        test_executor!(async move {
            let reads: Vec<(u64, usize)> =
                vec![(0, 64), (0, 256), (32, 4064), (4095, 10), (8192, 4096)];
            let merged = collect_iovecs(CoalescedReads::new(
                4096,
                None,
                None,
                stream::iter(reads.iter().copied()),
            ))
            .await;

            assert_eq!(
                merged,
                [
                    ((0, 64), (0, 4096)),
                    ((0, 256), (0, 4096)),
                    ((32, 4064), (0, 4096)),
                    ((4095, 10), (4095, 10)),
                    ((8192, 4096), (8192, 4096))
                ]
            );
        });
    }

    #[test]
    fn large_input_passthrough() {
        test_executor!(async move {
            let reads: Vec<(u64, usize)> =
                vec![(0, 64), (0, 256), (32, 4064), (4095, 10), (8192, 4096)];
            let merged = collect_iovecs(CoalescedReads::new(
                500,
                None,
                None,
                stream::iter(reads.iter().copied()),
            ))
            .await;

            assert_eq!(
                merged,
                [
                    ((0, 64), (0, 256)),
                    ((0, 256), (0, 256)),
                    ((32, 4064), (32, 4064)),
                    ((4095, 10), (4095, 10)),
                    ((8192, 4096), (8192, 4096))
                ]
            );

            let merged = collect_iovecs(CoalescedReads::new(
                0,
                None,
                None,
                stream::iter(reads.iter().copied()),
            ))
            .await;

            assert_eq!(
                merged,
                [
                    ((0, 64), (0, 64)),
                    ((0, 256), (0, 256)),
                    ((32, 4064), (32, 4064)),
                    ((4095, 10), (4095, 10)),
                    ((8192, 4096), (8192, 4096))
                ]
            );
        });
    }

    #[test]
    fn nonmonotonic_iovec_merging() {
        test_executor!(async move {
            let reads: Vec<(u64, usize)> = vec![(64, 256), (32, 4064), (4095, 10), (8192, 4096)];
            let merged = collect_iovecs(CoalescedReads::new(
                4096,
                None,
                None,
                stream::iter(reads.iter().copied()),
            ))
            .await;

            assert_eq!(
                merged,
                [
                    ((64, 256), (32, 4073)),
                    ((32, 4064), (32, 4073)),
                    ((4095, 10), (32, 4073)),
                    ((8192, 4096), (8192, 4096))
                ]
            );
        });
    }

    #[test]
    fn read_amplification_limit() {
        test_executor!(async move {
            let reads: Vec<(u64, usize)> = vec![
                (128, 128),
                (128, 1),
                (160, 96),
                (96, 160),
                (64, 192),
                (0, 256),
            ];
            let merged = collect_iovecs(CoalescedReads::new(
                4096,
                Some(0),
                None,
                stream::iter(reads.iter().copied()),
            ))
            .await;

            assert_eq!(
                merged,
                [
                    ((128, 128), (0, 256)),
                    ((128, 1), (0, 256)),
                    ((160, 96), (0, 256)),
                    ((96, 160), (0, 256)),
                    ((64, 192), (0, 256)),
                    ((0, 256), (0, 256)),
                ]
            );
        });
    }

    /// The gap is the same whichever side of the merged span a read lands on.
    #[test]
    fn read_amplification_limit_in_either_order() {
        test_executor!(async move {
            let merge = |reads: Vec<(u64, usize)>, limit| {
                collect_iovecs(CoalescedReads::new(
                    4096,
                    Some(limit),
                    None,
                    stream::iter(reads),
                ))
            };
            let ascending = vec![(100, 60), (200, 40), (256, 64)];
            let descending = vec![(256, 64), (200, 40), (100, 60)];

            assert_eq!(
                merge(ascending.clone(), 0).await,
                [
                    ((100, 60), (100, 60)),
                    ((200, 40), (200, 40)),
                    ((256, 64), (256, 64)),
                ]
            );
            assert_eq!(
                merge(descending.clone(), 0).await,
                [
                    ((256, 64), (256, 64)),
                    ((200, 40), (200, 40)),
                    ((100, 60), (100, 60)),
                ]
            );
            assert_eq!(
                merge(ascending, 32).await,
                [
                    ((100, 60), (100, 60)),
                    ((200, 40), (200, 120)),
                    ((256, 64), (200, 120)),
                ]
            );
            assert_eq!(
                merge(descending, 32).await,
                [
                    ((256, 64), (200, 120)),
                    ((200, 40), (200, 120)),
                    ((100, 60), (100, 60)),
                ]
            );
        });
    }

    #[test]
    fn read_no_coalescing() {
        test_executor!(async move {
            let reads: Vec<(u64, usize)> = vec![
                (128, 128),
                (128, 1),
                (160, 96),
                (96, 160),
                (64, 192),
                (0, 256),
            ];
            let merged = collect_iovecs(CoalescedReads::new(
                0,
                Some(0),
                None,
                stream::iter(reads.iter().copied()),
            ))
            .await;

            assert_eq!(
                merged,
                [
                    ((128, 128), (128, 128)),
                    ((128, 1), (128, 1)),
                    ((160, 96), (160, 96)),
                    ((96, 160), (96, 160)),
                    ((64, 192), (64, 192)),
                    ((0, 256), (0, 256)),
                ]
            );
        });
    }

    /// A temporary file of `bytes` bytes, a multiple of the `O_DIRECT`
    /// alignment.
    async fn a_file_of(bytes: usize) -> Rc<DmaFile> {
        let file = crate::io::OpenOptions::new()
            .write(true)
            .read(true)
            .tmpfile(true)
            .dma_open(std::env::temp_dir())
            .await
            .unwrap();
        let mut buf = file.alloc_dma_buffer(bytes);
        buf.as_bytes_mut().fill(7);
        file.write_at(buf, 0).await.unwrap();
        Rc::new(file)
    }

    /// A read returned to the caller is replaced only while the reads still
    /// in flight are under the memory limit, so small reads giving way to
    /// large ones do not carry the stream past it.
    #[test]
    fn replacing_returned_reads_keeps_to_the_memory_limit() {
        test_executor!(async move {
            const SMALL: usize = 4096;
            const LARGE: usize = 65536;
            const CAP: usize = 65536;
            let file = a_file_of(32 * SMALL + 32 * LARGE).await;
            let small = (0..32u64).map(|i| (i * SMALL as u64, SMALL));
            let large = (0..32u64).map(|i| (32 * SMALL as u64 + i * LARGE as u64, LARGE));
            let mut reads = file
                .read_many(
                    stream::iter(small.chain(large)),
                    MergedBufferLimit::NoMerging,
                    ReadAmplificationLimit::NoAmplification,
                )
                .with_memory_limit(Some(CAP));
            let mut most = 0;
            let mut yielded = 0;
            while let Some(read) = reads.next().await {
                read.unwrap();
                yielded += 1;
                most = most.max(reads.inner.inflight_memory);
            }
            assert_eq!(yielded, 64);
            assert!(
                most <= CAP + LARGE,
                "{most} bytes in flight against a limit of {CAP}"
            );
            file.close_rc().await.unwrap();
        });
    }

    /// Drains `reads`, checking before every item that its bounds cover the
    /// `total` items it must yield.
    async fn assert_size_hint_holds<S: Stream + Unpin>(mut reads: S, total: usize) {
        for remaining in (0..=total).rev() {
            let (low, high) = reads.size_hint();
            assert!(
                low <= remaining && high.is_none_or(|high| high >= remaining),
                "size_hint ({low}, {high:?}) with {remaining} left"
            );
            assert_eq!(reads.next().await.is_some(), remaining > 0);
        }
    }

    /// Reads merged into one system read still yield one item each, and the
    /// bounds must cover all of them: those of the read in hand, and those
    /// coalescing still holds when the concurrency limit stops it early.
    #[test]
    fn size_hint_bounds_the_reads_left_to_yield() {
        test_executor!(async move {
            let file = a_file_of(65536).await;
            let merged = file.read_many(
                stream::iter((0..16u64).map(|i| (i * 512, 512usize))),
                MergedBufferLimit::DeviceMaxSingleRequest,
                ReadAmplificationLimit::NoAmplification,
            );
            assert_size_hint_holds(merged, 16).await;
            let held = file
                .read_many(
                    stream::iter((0..16u64).map(|i| (i * 4096, 512usize))),
                    MergedBufferLimit::NoMerging,
                    ReadAmplificationLimit::NoAmplification,
                )
                .with_concurrency(1);
            assert_size_hint_holds(held, 16).await;
            file.close_rc().await.unwrap();
        });
    }

    /// Once the last read carved out of a system read is handed out, the
    /// stream lets go of its buffer: a later read of the same range finds
    /// nothing in flight to reuse.
    #[test]
    fn a_spent_system_read_is_released_by_the_stream() {
        test_executor!(async move {
            let file = a_file_of(4096).await;
            file.attach_scheduler();
            let mut reads = file.read_many(
                stream::iter((0..4u64).map(|i| (i * 512, 512usize))),
                MergedBufferLimit::DeviceMaxSingleRequest,
                ReadAmplificationLimit::NoAmplification,
            );
            for _ in 0..4 {
                drop(reads.next().await.unwrap().unwrap());
            }
            let deduped = || {
                crate::executor()
                    .io_stats()
                    .all_rings()
                    .file_deduped_reads()
                    .0
            };
            let before = deduped();
            file.read_at(0, 512).await.unwrap();
            assert_eq!(deduped(), before, "the stream still holds its last read");
            drop(reads);
            file.close_rc().await.unwrap();
        });
    }

    /// The memory limit documents that a single request always runs, so a
    /// limit of zero reads one at a time.
    #[test]
    fn a_zero_memory_limit_reads_one_at_a_time() {
        test_executor!(async move {
            let file = a_file_of(8192).await;
            let mut reads = file
                .read_many(
                    stream::iter((0..4u64).map(|i| (i * 2048, 512usize))),
                    MergedBufferLimit::NoMerging,
                    ReadAmplificationLimit::NoAmplification,
                )
                .with_memory_limit(Some(0));
            let mut yielded = 0;
            while let Some(read) = reads.next().await {
                assert_eq!(read.unwrap().1.len(), 512);
                assert!(reads.inner.inflight.len() <= 1);
                yielded += 1;
            }
            assert_eq!(yielded, 4);
            file.close_rc().await.unwrap();
        });
    }

    /// No read could ever be submitted under a concurrency of zero, so it is
    /// refused up front.
    #[test]
    #[should_panic(expected = "concurrency must be at least 1")]
    fn a_concurrency_of_zero_is_refused() {
        test_executor!(async move {
            let file = a_file_of(4096).await;
            let _ = file
                .read_many(
                    stream::iter([(0u64, 512usize)]),
                    MergedBufferLimit::NoMerging,
                    ReadAmplificationLimit::NoAmplification,
                )
                .with_concurrency(0);
        });
    }

    #[test]
    fn flush_merger() {
        let mut merger = IOVecMerger::<(u64, usize)> {
            max_merged_buffer_size: 102400,
            max_read_amp: None,
            current: None,
            merged: VecDeque::new(),
        };
        assert!(merger.flush().is_none());

        assert!(merger.merge((0u64, 4096usize)).is_none());
        let f = merger.flush().unwrap();
        assert_eq!(f.pos(), 0);
        assert_eq!(f.size(), 4096);

        assert!(merger.merge((4096u64, 4096usize)).is_none());
        let f = merger.flush().unwrap();
        assert_eq!(f.pos(), 4096);
        assert_eq!(f.size(), 4096);
    }
}
