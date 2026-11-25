//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
//! Buffers that are friendly to be used with O_DIRECT files.
//! For the time being they are really only properly aligned,
//! but in the near future they can be coming from memory-areas
//! that are pre-registered for I/O uring.

use std::ptr;

use crate::{
    io::ReadResult,
    sys::uring::{UringBuffer, UringBufferAllocator},
};
use alloc::alloc::Layout;

#[derive(Debug)]
pub(crate) struct SysAlloc {
    data: ptr::NonNull<u8>,
    layout: Layout,
}

impl SysAlloc {
    fn new(size: usize) -> Option<Self> {
        if size == 0 {
            None
        } else {
            let layout = Layout::from_size_align(size, 4096).unwrap();
            let data = unsafe { alloc::alloc::alloc_zeroed(layout) };
            let data = ptr::NonNull::new(data)?;
            Some(SysAlloc { data, layout })
        }
    }

    fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_ptr()
    }

    /// An aligned, owned copy of `bytes`. An empty slice still gets one byte,
    /// because a zero-sized layout cannot be allocated, and that byte is never
    /// addressable through the buffer.
    fn copy_of(bytes: &[u8]) -> Self {
        let layout = Layout::from_size_align(bytes.len().max(1), 4096).unwrap();
        let data = unsafe { alloc::alloc::alloc(layout) };
        let data =
            ptr::NonNull::new(data).unwrap_or_else(|| alloc::alloc::handle_alloc_error(layout));
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), data.as_ptr(), bytes.len()) };
        SysAlloc { data, layout }
    }
}

impl Drop for SysAlloc {
    fn drop(&mut self) {
        unsafe {
            alloc::alloc::dealloc(self.data.as_ptr(), self.layout);
        }
    }
}

#[derive(Debug)]
pub(crate) enum BufferStorage {
    Sys(SysAlloc),
    Uring(UringBuffer),
    EventFd(*mut u8),
    ReadResult(ReadResult),
}

impl BufferStorage {
    fn as_ptr(&self) -> *const u8 {
        match self {
            BufferStorage::Sys(x) => x.as_ptr(),
            BufferStorage::Uring(x) => x.as_ptr(),
            BufferStorage::EventFd(x) => *x as *const u8,
            BufferStorage::ReadResult(x) => x.as_ptr(),
        }
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        match self {
            BufferStorage::Sys(x) => x.as_mut_ptr(),
            BufferStorage::Uring(x) => x.as_mut_ptr(),
            BufferStorage::EventFd(x) => *x,
            BufferStorage::ReadResult(_) => {
                unreachable!("DmaBuffer::make_owned replaces ReadResult storage before any write")
            }
        }
    }
}

#[derive(Debug)]
/// A buffer suitable for Direct I/O over io_uring
///
/// Direct I/O has strict requirements about alignment. On top of that, io_uring
/// places additional restrictions on memory placement if we are to use advanced
/// features like registered buffers.
///
/// The `DmaBuffer` is a buffer that adheres to those properties making it
/// suitable for io_uring's Direct I/O.
pub struct DmaBuffer {
    storage: BufferStorage,
    /// Invariant: at most the length of the original allocation.
    size: usize,
}

impl DmaBuffer {
    pub(crate) fn new(size: usize) -> Option<DmaBuffer> {
        Some(DmaBuffer {
            storage: BufferStorage::Sys(SysAlloc::new(size)?),
            size,
        })
    }

    pub(crate) fn with_storage(size: usize, storage: BufferStorage) -> DmaBuffer {
        DmaBuffer { storage, size }
    }

    pub(crate) fn uring_buffer_id(&self) -> Option<u32> {
        match &self.storage {
            BufferStorage::Uring(x) => x.uring_buffer_id(),
            _ => None,
        }
    }

    /// The index this buffer is registered under, if it came from `pool`.
    /// A buffer from another reactor's pool has an index into that reactor's
    /// registration, which means nothing to this one.
    pub(crate) fn registered_index_in(&self, pool: &UringBufferAllocator) -> Option<u32> {
        match &self.storage {
            BufferStorage::Uring(x) if x.is_from(pool) => x.uring_buffer_id(),
            _ => None,
        }
    }

    /// Reduce the length of this buffer to be `newsize`. This value must be <= the current
    /// [`len`](#method.len) and is a destructive operation (length cannot be increased).
    /// NOTE: When using this with DmaFile, you have to make sure that `newsize` is properly
    /// aligned or the write will fail due to O_DIRECT requirements.
    pub fn trim_to_size(&mut self, newsize: usize) {
        assert!(newsize <= self.size);
        self.size = newsize;
    }

    /// Returns a representation of the current addressable contents of this
    /// `DmaBuffer` as a byte slice
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.as_ptr(), self.size) }
    }

    /// Returns a representation of the current addressable contents of this
    /// `DmaBuffer` as a mutable byte slice
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.as_mut_ptr(), self.size) }
    }

    /// Returns the current number of bytes present in the addressable contents
    /// of this `DmaBuffer`
    pub fn len(&self) -> usize {
        self.size
    }

    /// Indicates whether this buffer is empty. An empty buffer can be,
    /// for instance, the result of a read past the end of a file.
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Returns a representation of the current addressable contents of this
    /// `DmaBuffer` as mutable pointer
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.make_owned();
        self.storage.as_mut_ptr()
    }

    /// A buffer made from a [`ReadResult`] shares its bytes with every other
    /// clone of that read, so before the first write the addressable bytes
    /// are copied into storage this buffer owns alone.
    fn make_owned(&mut self) {
        if let BufferStorage::ReadResult(shared) = &self.storage {
            let owned = SysAlloc::copy_of(&shared[..self.size]);
            self.storage = BufferStorage::Sys(owned);
        }
    }

    /// Returns a representation of the current addressable contents of this
    /// `DmaBuffer` as pointer
    pub fn as_ptr(&self) -> *const u8 {
        self.storage.as_ptr()
    }
}

impl AsRef<[u8]> for DmaBuffer {
    #[inline(always)]
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl AsMut<[u8]> for DmaBuffer {
    #[inline(always)]
    fn as_mut(&mut self) -> &mut [u8] {
        self.as_bytes_mut()
    }
}

impl From<ReadResult> for DmaBuffer {
    fn from(value: ReadResult) -> Self {
        Self {
            size: value.len(),
            storage: BufferStorage::ReadResult(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        io::{DmaFile, OpenOptions},
        test_utils::make_test_directories,
        LocalExecutor,
    };
    use std::path::Path;

    async fn read_write(path: &Path) -> DmaFile {
        OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .dma_open(path)
            .await
            .unwrap()
    }

    #[test]
    fn new_buffer_does_not_expose_freed_memory() {
        let mut first = DmaBuffer::new(1 << 16).unwrap();
        first.as_bytes_mut().fill(0xab);
        drop(first);
        let second = DmaBuffer::new(1 << 16).unwrap();
        assert!(second.as_bytes().iter().all(|&b| b == 0));
    }

    /// `From<ReadResult>` is public and safe, so every method on the buffer it
    /// builds has to work, and writing through it must not reach the storage
    /// other clones of the read still share.
    #[test]
    fn a_buffer_from_a_read_can_be_mutated_without_changing_the_read() {
        for dir in make_test_directories("dma-buffer-from-read") {
            let path = dir.path.clone();
            LocalExecutor::default().run(async move {
                let file = read_write(&path.join("testfile")).await;
                let mut source = file.alloc_dma_buffer(8192);
                source.as_bytes_mut().fill(0xaa);
                file.write_at(source, 0).await.unwrap();

                let read = file.read_at(0, 4096).await.unwrap();
                let other = read.clone();
                let mut buffer: DmaBuffer = read.into();
                buffer.as_bytes_mut().fill(0x55);
                assert_eq!(buffer.len(), 4096);
                assert!(buffer.as_bytes().iter().all(|&b| b == 0x55));
                assert!(other.iter().all(|&b| b == 0xaa));

                file.write_at(buffer, 4096).await.unwrap();
                let back = file.read_at(4096, 4096).await.unwrap();
                assert!(back.iter().all(|&b| b == 0x55));
                assert!(other.iter().all(|&b| b == 0xaa));
                file.close().await.unwrap();
            });
        }
    }

    /// A trimmed or sliced read keeps its length and contents when it is first
    /// mutated, and an empty one still hands out an empty slice.
    #[test]
    fn a_sliced_trimmed_or_empty_read_keeps_its_bytes_when_mutated() {
        for dir in make_test_directories("dma-buffer-from-sliced-read") {
            let path = dir.path.clone();
            LocalExecutor::default().run(async move {
                let file = read_write(&path.join("testfile")).await;
                let mut source = file.alloc_dma_buffer(4096);
                for (i, b) in source.as_bytes_mut().iter_mut().enumerate() {
                    *b = i as u8;
                }
                file.write_at(source, 0).await.unwrap();

                let read = file.read_at(0, 4096).await.unwrap();
                let slice = crate::io::ReadResult::slice(&read, 10, 100).unwrap();
                let mut buffer: DmaBuffer = slice.into();
                buffer.trim_to_size(50);
                buffer.as_bytes_mut()[0] = 0xff;
                assert_eq!(buffer.len(), 50);
                assert_eq!(buffer.as_bytes()[0], 0xff);
                for (i, &b) in buffer.as_bytes().iter().enumerate().skip(1) {
                    assert_eq!(b, (i + 10) as u8);
                }
                assert_eq!(read[10], 10);

                let past_end = file.read_at(1 << 20, 4096).await.unwrap();
                let mut empty: DmaBuffer = past_end.into();
                assert!(empty.as_bytes_mut().is_empty());
                assert!(!empty.as_mut_ptr().is_null());
                file.close().await.unwrap();
            });
        }
    }

    /// A buffer is its storage and a length; every view starts at the
    /// storage's first byte, so there is no offset to carry.
    #[test]
    fn a_buffer_is_its_storage_and_a_length() {
        assert_eq!(
            std::mem::size_of::<DmaBuffer>(),
            std::mem::size_of::<BufferStorage>() + std::mem::size_of::<usize>()
        );
    }
}
