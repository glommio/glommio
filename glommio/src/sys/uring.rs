//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
use alloc::alloc::Layout;
use log::warn;
use nix::{
    poll::PollFlags,
    sys::socket::{SockaddrLike, SockaddrStorage},
};
use rlimit::Resource;
use std::{
    cell::{Cell, Ref, RefCell, RefMut},
    collections::VecDeque,
    convert::TryFrom,
    fmt,
    future::Future,
    io,
    ops::Range,
    os::unix::io::RawFd,
    panic,
    pin::Pin,
    ptr,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use io_uring::{cqueue, opcode, squeue, types, CompletionStatus, IoUring};

use crate::{
    free_list::{FreeList, Idx},
    sys::{
        self,
        blocking::{BlockingThreadOp, BlockingThreadPool},
        dma_buffer::{BufferStorage, DmaBuffer},
        membarrier, DirectIo, EnqueuedSource, EnqueuedStatus, InnerSource, IoBuffer,
        PollableStatus, SockAddrStorage, Source, SourceType, Statx, TimeSpec64,
    },
    GlommioError, IoRequirements, IoStats, ReactorErrorKind, RingIoStats, TaskQueueHandle,
};
use ahash::AHashMap;
use buddy_alloc::buddy_alloc::{BuddyAlloc, BuddyAllocParam};
use nix::sys::socket::{MsgFlags, SockFlag};
use smallvec::SmallVec;

#[allow(dead_code)]
#[derive(Debug)]
enum UringOpDescriptor {
    PollAdd(PollFlags),
    PollRemove(*const u8),
    Cancel(u64),
    Write(*const u8, usize, u64),
    WriteFixed(*const u8, usize, u64, u32),
    ReadFixed(u64, usize),
    Read(u64, usize),
    Open(*const u8, libc::c_int, u32),
    Close,
    FDataSync,
    Connect(*const SockaddrStorage),
    LinkTimeout(*const crate::sys::KernelTimespec),
    Accept(*mut SockAddrStorage),
    Fallocate(u64, u64, libc::c_int),
    StatxFd(RawFd, *mut Statx),
    Timeout(*const crate::sys::KernelTimespec, u32),
    TimeoutRemove(u64),
    SockSend(*const u8, usize, i32),
    SockSendMsg(*mut libc::msghdr, i32),
    SockRecv(usize, i32),
    SockRecvInto(i32),
    /// The bool is whether to bundle: one completion for everything the
    /// socket holds, over as many buffers as that takes. The request's
    /// length is left at zero on purpose: a length on a multishot receive
    /// is a per-shot cap, and the kernel then maps that many bytes of
    /// buffers for every shot however little arrives, which drained a small
    /// pool on the first byte.
    RecvMulti(bool),
    SockRecvMsg(usize, i32),
    Nop,
}

#[derive(Debug)]
pub(crate) struct UringDescriptor {
    fd: RawFd,
    flags: squeue::Flags,
    user_data: u64,
    args: UringOpDescriptor,
}

pub(crate) struct UringBufferAllocator {
    data: ptr::NonNull<u8>,
    size: usize,
    allocator: RefCell<BuddyAlloc>,
    layout: Layout,
    uring_buffer_id: Cell<Option<u32>>,
}

impl fmt::Debug for UringBufferAllocator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UringBufferAllocator")
            .field("data", &self.data)
            .finish()
    }
}

impl UringBufferAllocator {
    fn new(size: usize) -> Self {
        let layout = Layout::from_size_align(size, 4096).unwrap();
        let (data, allocator) = unsafe {
            let data = alloc::alloc::alloc(layout);
            let data = std::ptr::NonNull::new(data).unwrap();
            let allocator = BuddyAlloc::new(BuddyAllocParam::new(
                data.as_ptr(),
                layout.size(),
                layout.align(),
            ));
            (data, RefCell::new(allocator))
        };

        UringBufferAllocator {
            data,
            size,
            allocator,
            layout,
            uring_buffer_id: Cell::new(None),
        }
    }

    fn activate_registered_buffers(&self, idx: u32) {
        self.uring_buffer_id.set(Some(idx))
    }

    fn free(&self, ptr: ptr::NonNull<u8>) {
        let mut allocator = self.allocator.borrow_mut();
        allocator.free(ptr.as_ptr());
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.data.as_ptr(), self.size) }
    }

    fn new_buffer(self: &Rc<Self>, size: usize) -> Option<DmaBuffer> {
        let mut alloc = self.allocator.borrow_mut();
        match ptr::NonNull::new(alloc.malloc(size)) {
            Some(data) => {
                let ub = UringBuffer {
                    allocator: self.clone(),
                    data,
                    uring_buffer_id: self.uring_buffer_id.get(),
                };
                Some(DmaBuffer::with_storage(size, BufferStorage::Uring(ub)))
            }
            None => DmaBuffer::new(size),
        }
    }
}

impl Drop for UringBufferAllocator {
    fn drop(&mut self) {
        unsafe {
            alloc::alloc::dealloc(self.data.as_ptr(), self.layout);
        }
    }
}

pub(crate) struct UringBuffer {
    allocator: Rc<UringBufferAllocator>,
    data: ptr::NonNull<u8>,
    uring_buffer_id: Option<u32>,
}

impl fmt::Debug for UringBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UringBuffer")
            .field("data", &self.data)
            .field("uring_buffer_id", &self.uring_buffer_id)
            .finish()
    }
}

impl UringBuffer {
    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_ptr()
    }

    pub(crate) fn uring_buffer_id(&self) -> Option<u32> {
        self.uring_buffer_id
    }
}

impl Drop for UringBuffer {
    fn drop(&mut self) {
        let ptr = self.data;
        self.allocator.free(ptr);
    }
}

/// The opcodes glommio cannot run without.
static GLOMMIO_URING_OPS: &[(&str, u8)] = &[
    ("NOP", io_uring::opcode::Nop::CODE),
    ("READV", io_uring::opcode::Readv::CODE),
    ("WRITEV", io_uring::opcode::Writev::CODE),
    ("FSYNC", io_uring::opcode::Fsync::CODE),
    ("READ_FIXED", io_uring::opcode::ReadFixed::CODE),
    ("WRITE_FIXED", io_uring::opcode::WriteFixed::CODE),
    ("POLL_ADD", io_uring::opcode::PollAdd::CODE),
    ("POLL_REMOVE", io_uring::opcode::PollRemove::CODE),
    ("SENDMSG", io_uring::opcode::SendMsg::CODE),
    ("RECVMSG", io_uring::opcode::RecvMsg::CODE),
    ("TIMEOUT", io_uring::opcode::Timeout::CODE),
    ("TIMEOUT_REMOVE", io_uring::opcode::TimeoutRemove::CODE),
    ("ACCEPT", io_uring::opcode::Accept::CODE),
    ("LINK_TIMEOUT", io_uring::opcode::LinkTimeout::CODE),
    ("CONNECT", io_uring::opcode::Connect::CODE),
    ("FALLOCATE", io_uring::opcode::Fallocate::CODE),
    ("OPENAT", io_uring::opcode::OpenAt::CODE),
    ("CLOSE", io_uring::opcode::Close::CODE),
    ("STATX", io_uring::opcode::Statx::CODE),
    ("READ", io_uring::opcode::Read::CODE),
    ("WRITE", io_uring::opcode::Write::CODE),
    ("SEND", io_uring::opcode::Send::CODE),
    ("RECV", io_uring::opcode::Recv::CODE),
    ("ASYNC_CANCEL", io_uring::opcode::AsyncCancel::CODE),
];

/// Why this kernel cannot run glommio.
#[derive(Debug)]
pub(crate) enum UringUnsupported {
    /// `io_uring_setup` itself failed.
    SetupFailed(io::Error),
    /// The ring was created, but registering a probe against it failed.
    ProbeFailed(io::Error),
    /// The ring works and some opcodes glommio submits are missing.
    MissingOps(Vec<&'static str>),
}

impl fmt::Display for UringUnsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UringUnsupported::SetupFailed(err) => {
                write!(f, "failed to create an io_uring: {err}")?;
                match err.raw_os_error() {
                    Some(libc::ENOSYS) => write!(
                        f,
                        ". The kernel does not implement io_uring at all; glommio's supported \
                         minimum is 5.8"
                    ),
                    Some(libc::EPERM) => {
                        write!(
                            f,
                            ". io_uring is present but not permitted for this process"
                        )?;
                        match io_uring_disabled() {
                            Some(1) => write!(
                                f,
                                ", because kernel.io_uring_disabled=1 restricts it to processes \
                                 with CAP_SYS_ADMIN"
                            ),
                            Some(2) => {
                                write!(f, ", because kernel.io_uring_disabled=2 disables it")
                            }
                            _ => write!(
                                f,
                                ". A seccomp policy blocking io_uring_setup is the usual cause; \
                                 container runtimes often ship one"
                            ),
                        }
                    }
                    _ => Ok(()),
                }
            }
            UringUnsupported::ProbeFailed(err) => {
                write!(f, "failed to register a probe against io_uring: {err}")
            }
            UringUnsupported::MissingOps(ops) => write!(
                f,
                "the kernel's io_uring is missing operations glommio submits: {}. glommio's \
                 supported minimum is 5.8",
                ops.iter()
                    .map(|op| format!("IORING_OP_{op}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// Reads `kernel.io_uring_disabled`, which RHEL 9 and other distributions use
/// to restrict io_uring independently of the kernel version. Absent on kernels
/// older than 6.6 and on distributions that did not backport it.
fn io_uring_disabled() -> Option<u8> {
    std::fs::read_to_string("/proc/sys/kernel/io_uring_disabled")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Checks the kernel implements every opcode glommio submits.
fn check_supported_operations(ops: &[(&'static str, u8)]) -> Result<(), UringUnsupported> {
    let ring = io_uring::IoUring::new(1).map_err(UringUnsupported::SetupFailed)?;

    let mut probe = io_uring::Probe::new();
    ring.submitter()
        .register_probe(&mut probe)
        .map_err(UringUnsupported::ProbeFailed)?;

    let missing: Vec<_> = ops
        .iter()
        .filter(|(_, opcode)| !probe.is_supported(*opcode))
        .map(|(name, _)| *name)
        .collect();

    if missing.is_empty() {
        Ok(())
    } else {
        Err(UringUnsupported::MissingOps(missing))
    }
}

lazy_static! {
    /// The probe's verdict, once one has been reached. `None` means it has not
    /// been, which is not the same as "not tried yet" -- see below.
    static ref IO_URING_SUPPORT: std::sync::Mutex<Option<Result<(), String>>> =
        std::sync::Mutex::new(None);
}

/// Whether the probe might succeed if we tried again.
fn is_transient(err: &UringUnsupported) -> bool {
    match err {
        UringUnsupported::SetupFailed(err) | UringUnsupported::ProbeFailed(err) => matches!(
            err.raw_os_error(),
            Some(libc::EMFILE) | Some(libc::ENFILE) | Some(libc::ENOMEM)
        ),
        UringUnsupported::MissingOps(_) => false,
    }
}

/// Returns `Err` describing why this kernel cannot run glommio.
///
/// Cached once per process, but only a definitive answer: a probe that failed
/// for want of a descriptor is tried again.
///
/// A poisoned lock here carries no state worth protecting: the value behind
/// it is a cached verdict, and a panicking prober leaves it untouched.
pub(crate) fn check_uring_support() -> io::Result<()> {
    let unsupported = |reason: String| io::Error::new(io::ErrorKind::Unsupported, reason);
    let mut cached = IO_URING_SUPPORT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(verdict) = cached.as_ref() {
        return verdict.clone().map_err(unsupported);
    }

    match check_supported_operations(GLOMMIO_URING_OPS) {
        Ok(()) => {
            *cached = Some(Ok(()));
            Ok(())
        }
        Err(err) => {
            let reason = err.to_string();
            if !is_transient(&err) {
                *cached = Some(Err(reason.clone()));
            }
            Err(unsupported(reason))
        }
    }
}

/// Builds the submission queue entry for one descriptor.
///
/// # Safety
///
/// Every pointer passed to the opcodes below belongs to a `Source` that the
/// `SourceMap` keeps alive until the completion is reaped.
///
/// # Notes
///
/// * For `LinkTimeout`, the timespec is borrowed, not copied: it has to
///   outlive the SQE. Same layout as `__kernel_timespec`, asserted in
///   `sys/mod.rs`.
fn fill_sqe<F>(
    op: &UringDescriptor,
    buffer_allocation: F,
    source_map: &mut SourceMap,
) -> squeue::Entry
where
    F: FnOnce(usize) -> Option<DmaBuffer>,
{
    let mut user_data = op.user_data;
    let mut flags = op.flags;
    let fd = types::Fd(op.fd);

    let entry = unsafe {
        match op.args {
            UringOpDescriptor::PollAdd(events) => {
                opcode::PollAdd::new(fd, events.bits() as _).build()
            }
            UringOpDescriptor::PollRemove(to_remove) => {
                user_data = 0;
                opcode::PollRemove::new(to_remove as u64).build()
            }
            UringOpDescriptor::Cancel(to_remove) => {
                user_data = 0;
                opcode::AsyncCancel::new(to_remove).build()
            }
            UringOpDescriptor::Write(ptr, len, pos) => {
                opcode::Write::new(fd, ptr, len as u32).offset(pos).build()
            }
            UringOpDescriptor::Read(pos, len) => {
                source_map.peek_source_mut(from_user_data(op.user_data), |mut x| {
                    match &mut x.source_type {
                        SourceType::ForeignNotifier(result, _) => {
                            opcode::Read::new(fd, result as *mut u64 as *mut u8, 8)
                                .offset(pos)
                                .build()
                        }
                        SourceType::Read(PollableStatus::NonPollable(DirectIo::Disabled), slot) => {
                            let mut buf = buffer_allocation(len).expect("Buffer allocation failed");
                            let entry = opcode::Read::new(fd, buf.as_mut_ptr(), len as u32)
                                .offset(pos)
                                .build();
                            assert!(
                                slot.is_none(),
                                "if you have a buffer here, that very likely means you are \
                                 reusing the source; the kernel knows about that buffer \
                                 already, and will write to it, so this can only be called \
                                 if there is no buffer attached to it"
                            );
                            *slot = Some(IoBuffer::DmaSink(buf));
                            entry
                        }
                        _ => unreachable!("Expected Read source type"),
                    }
                })
            }
            UringOpDescriptor::Open(path, flags, mode) => {
                opcode::OpenAt::new(fd, path as *const libc::c_char)
                    .flags(flags)
                    .mode(mode)
                    .build()
            }
            UringOpDescriptor::FDataSync => opcode::Fsync::new(fd)
                .flags(types::FsyncFlags::DATASYNC)
                .build(),
            UringOpDescriptor::Connect(addr) => {
                opcode::Connect::new(fd, (*addr).as_ptr(), (*addr).len()).build()
            }
            UringOpDescriptor::LinkTimeout(timespec) => {
                opcode::LinkTimeout::new(timespec as *const types::Timespec).build()
            }
            UringOpDescriptor::Accept(addr) => {
                let (storage, len) = (*addr).as_raw_parts();
                opcode::Accept::new(fd, storage, len)
                    .flags(SockFlag::SOCK_CLOEXEC.bits())
                    .build()
            }
            UringOpDescriptor::Fallocate(offset, size, flags) => opcode::Fallocate::new(fd, size)
                .offset(offset)
                .mode(flags)
                .build(),
            UringOpDescriptor::StatxFd(statx_fd, statx_buf) => {
                const EMPTY_PATH: &[u8] = b"\0";
                /// Not defined by the libc crate for musl targets. 0 in the
                /// kernel UAPI (`linux/stat.h`): do whatever stat() does.
                const AT_STATX_SYNC_AS_STAT: libc::c_int = 0;
                let flags = AT_STATX_SYNC_AS_STAT | libc::AT_NO_AUTOMOUNT | libc::AT_EMPTY_PATH;
                opcode::Statx::new(
                    types::Fd(statx_fd),
                    EMPTY_PATH.as_ptr() as *const libc::c_char,
                    statx_buf as *mut types::statx,
                )
                .flags(flags)
                .mask(0x7ff)
                .build()
            }
            UringOpDescriptor::Timeout(timespec, events) => {
                opcode::Timeout::new(timespec as *const types::Timespec)
                    .count(events)
                    .build()
            }
            UringOpDescriptor::TimeoutRemove(timer) => opcode::TimeoutRemove::new(timer).build(),
            UringOpDescriptor::Close => opcode::Close::new(fd).build(),
            UringOpDescriptor::ReadFixed(pos, len) => {
                let mut buf = buffer_allocation(len).expect("Buffer allocation failed");
                source_map.peek_source_mut(from_user_data(op.user_data), |mut src| {
                    match &mut src.source_type {
                        SourceType::Read(PollableStatus::NonPollable(DirectIo::Disabled), slot) => {
                            let entry = opcode::Read::new(fd, buf.as_mut_ptr(), len as u32)
                                .offset(pos)
                                .build();
                            *slot = Some(IoBuffer::DmaSink(buf));
                            entry
                        }
                        SourceType::Read(_, slot) => {
                            let entry = match buf.uring_buffer_id() {
                                None => opcode::Read::new(fd, buf.as_mut_ptr(), len as u32)
                                    .offset(pos)
                                    .build(),
                                Some(idx) => opcode::ReadFixed::new(
                                    fd,
                                    buf.as_mut_ptr(),
                                    len as u32,
                                    idx as u16,
                                )
                                .offset(pos)
                                .build(),
                            };
                            *slot = Some(IoBuffer::DmaSink(buf));
                            entry
                        }
                        _ => unreachable!(),
                    }
                })
            }
            UringOpDescriptor::WriteFixed(ptr, len, pos, buf_index) => {
                opcode::WriteFixed::new(fd, ptr, len as u32, buf_index as u16)
                    .offset(pos)
                    .build()
            }
            UringOpDescriptor::SockSend(ptr, len, flags) => {
                opcode::Send::new(fd, ptr, len as u32).flags(flags).build()
            }
            UringOpDescriptor::SockSendMsg(hdr, flags) => {
                opcode::SendMsg::new(fd, hdr as *const libc::msghdr)
                    .flags(flags as u32)
                    .build()
            }
            UringOpDescriptor::SockRecvInto(flags) => {
                source_map.peek_source_mut(from_user_data(op.user_data), |mut src| {
                    match &mut src.source_type {
                        SourceType::SockRecvInto(slot) => {
                            let buffer = slot
                                .as_mut()
                                .expect("a SockRecvInto source without its buffer");
                            opcode::Recv::new(fd, buffer.as_mut_ptr(), buffer.len() as u32)
                                .flags(flags)
                                .build()
                        }
                        _ => unreachable!(),
                    }
                })
            }
            UringOpDescriptor::RecvMulti(bundle) => {
                flags |= squeue::Flags::BUFFER_SELECT;
                if bundle {
                    opcode::RecvMultiBundle::new(fd, RECV_BUFFER_GROUP).build()
                } else {
                    opcode::RecvMulti::new(fd, RECV_BUFFER_GROUP).build()
                }
            }
            UringOpDescriptor::SockRecv(len, flags) => {
                let mut buf = DmaBuffer::new(len).expect("failed to allocate buffer");
                let entry = opcode::Recv::new(fd, buf.as_mut_ptr(), len as u32)
                    .flags(flags)
                    .build();
                source_map.peek_source_mut(from_user_data(op.user_data), |mut src| {
                    match &mut src.source_type {
                        SourceType::SockRecv(slot) => {
                            *slot = Some(buf);
                        }
                        _ => unreachable!(),
                    };
                });
                entry
            }
            UringOpDescriptor::SockRecvMsg(len, flags) => {
                let mut buf = DmaBuffer::new(len).expect("failed to allocate buffer");
                source_map.peek_source_mut(from_user_data(op.user_data), |mut src| {
                    match &mut src.source_type {
                        SourceType::SockRecvMsg(slot, iov, hdr, msg_name) => {
                            iov.iov_base = buf.as_mut_ptr() as *mut libc::c_void;
                            iov.iov_len = len;

                            let msg_namelen =
                                std::mem::size_of::<nix::sys::socket::sockaddr_storage>()
                                    as libc::socklen_t;
                            hdr.msg_name = msg_name.as_mut_ptr() as *mut libc::c_void;
                            hdr.msg_namelen = msg_namelen;
                            hdr.msg_iov = iov as *mut libc::iovec;
                            hdr.msg_iovlen = 1;

                            let entry = opcode::RecvMsg::new(fd, hdr as *mut libc::msghdr)
                                .flags(flags as u32)
                                .build();
                            *slot = Some(buf);
                            entry
                        }
                        _ => unreachable!(),
                    }
                })
            }
            UringOpDescriptor::Nop => opcode::Nop::new().build(),
        }
    };

    entry.user_data(user_data).flags(flags)
}

/// Turns a raw CQE result into an `io::Result`. io_uring reports failure as a
/// negative errno in the completion, not through errno.
///
/// `ECANCELED` is converted to `ETIMEDOUT`. This will be the case for linked
/// `sqe`s with a timeout, and if we wanted to be really strict we'd check. But
/// if the operation is truly cancelled no one will check the result, and we
/// have no other use case for cancel at the moment so keep it simple
fn transmute_error(res: i32) -> io::Result<usize> {
    if res >= 0 {
        return Ok(res as usize);
    }
    Err(io::Error::from_raw_os_error(-res)).map_err(|x: io::Error| {
        if let Some(libc::ECANCELED) = x.raw_os_error() {
            io::Error::from_raw_os_error(libc::ETIMEDOUT)
        } else {
            x
        }
    })
}

fn record_stats<Ring: UringCommon>(
    ring: &mut Ring,
    src: &mut InnerSource,
    res: &io::Result<usize>,
) {
    if records_latency(src) {
        src.wakers.fulfilled_at = Some(Instant::now());
    }
    if let Some(fulfilled) = src.stats_collection.and_then(|x| x.fulfilled) {
        fulfilled(res, ring.io_stats_mut(), 1);
        if let Some(handle) = src.task_queue {
            fulfilled(res, ring.io_stats_for_task_queue_mut(handle), 1);
        }
    }

    let waiters = usize::saturating_sub(src.wakers.waiters.len(), 1);
    if waiters > 0 {
        if let Some(reused) = src.stats_collection.and_then(|x| x.reused) {
            reused(res, ring.io_stats_mut(), waiters as u64);
            if let Some(handle) = src.task_queue {
                reused(
                    res,
                    ring.io_stats_for_task_queue_mut(handle),
                    waiters as u64,
                );
            }
        }
    }
}

/// Find the next complete chain of events from the queue.
/// Returns None if the queue is empty.
fn peek_one_chain(queue: &VecDeque<UringDescriptor>, ring_size: usize) -> Option<Range<usize>> {
    if queue.is_empty() {
        return None;
    }
    let chain = queue
        .iter()
        .take(ring_size)
        .position(|sqe| {
            !sqe.flags
                .intersects(squeue::Flags::IO_LINK | squeue::Flags::IO_HARDLINK)
        })
        .expect("Unterminated SQE link chain or submission queue overflow");
    Some(0..chain + 1)
}

/// Whether this source's timestamps will ever be read.
///
/// `queued_at`, `submitted_at` and `fulfilled_at` exist to be subtracted from
/// each other in `Source::consume_result`, and only when a latency collection
/// function is installed -- which `record_io_latencies` does, off by default.
/// Taking the three clock readings regardless costs every I/O three
/// `clock_gettime` calls for fields nobody looks at.
///
/// This is why `submit_event_chain` carries its reading as an `Option`: a
/// chain whose sources all answer `false` here never reads the clock at all.
fn records_latency(src: &InnerSource) -> bool {
    src.stats_collection.and_then(|x| x.latency).is_some()
}

/// Extract a chain of events from the queue.
/// The chain be empty if the sources were cancelled
fn extract_one_chain(
    source_map: &mut SourceMap,
    queue: &mut VecDeque<UringDescriptor>,
    chain: Range<usize>,
    now: &mut Option<Instant>,
) -> SmallVec<[UringDescriptor; 1]> {
    queue
        .drain(chain)
        .filter(move |op| {
            if op.user_data > 0 {
                let id = from_user_data(op.user_data);
                let status = source_map.peek_source_mut(from_user_data(op.user_data), |mut x| {
                    if records_latency(&x) {
                        x.wakers.submitted_at = Some(*now.get_or_insert_with(Instant::now));
                    }
                    let current = x.enqueued.as_mut().expect("bug");
                    match current.status {
                        EnqueuedStatus::Enqueued => {
                            current.status = EnqueuedStatus::Dispatched;
                            EnqueuedStatus::Dispatched
                        }
                        EnqueuedStatus::Canceled => EnqueuedStatus::Canceled,
                        _ => unreachable!(),
                    }
                });
                if status == EnqueuedStatus::Canceled {
                    source_map.consume_source(id);
                    return false;
                }
            }
            true
        })
        .collect()
}

/// Submit the next complete chain of events from the queue if available.
///
/// A linked chain is only meaningful whole -- half of one is a link timeout
/// with no operation to guard. Bails if it doesn't fit and lets the caller
/// flush first.
///
/// The `sq.push` inside the unsafe block is sound because `op`'s buffers
/// belong to sources the `SourceMap` keeps alive until the completion is
/// reaped, and the capacity check above means the push cannot fail.
fn submit_event_chain(
    source_map: &mut SourceMap,
    ring: &mut IoUring,
    allocator: Rc<UringBufferAllocator>,
    queue: &mut VecDeque<UringDescriptor>,
    ring_size: usize,
) -> Option<bool> {
    let mut now = None;

    while let Some(chain) = peek_one_chain(queue, ring_size) {
        let mut sq = ring.submission();
        if sq.capacity() - sq.len() < chain.len() {
            return None;
        }

        let ops = extract_one_chain(source_map, queue, chain, &mut now);
        if ops.is_empty() {
            continue;
        }

        for op in ops {
            let allocator = allocator.clone();
            let entry = fill_sqe(&op, move |size| allocator.new_buffer(size), source_map);
            unsafe {
                sq.push(&entry)
                    .expect("chain was checked to fit in the submission queue");
            }
        }
        return Some(true);
    }
    Some(false)
}

/// What one completion did: whether it woke anyone, and whether it was the
/// source's last, which is what retires the source and frees its ring slot.
/// Every completion is the last one except a multishot receive's while the
/// kernel still holds the request.
struct Processed {
    woke: bool,
    retired: bool,
}

/// Queues the arrival a multishot completion carries, if any, on its
/// source, and cancels the request once the queue holds more than the
/// source's ceiling: a connection nobody reads must not drain the buffers
/// for every other one. The cancellation goes through the ring's own queue
/// and completes the request the ordinary way; the owner re-arms when it
/// has drained what is queued.
fn queue_multishot_arrival(inner: &mut InnerSource, value: &cqueue::Entry) {
    let over_ceiling = {
        let SourceType::RecvMulti(state) = &mut inner.source_type else {
            return;
        };
        if value.result() < 0 {
            return;
        }
        if let Some(bid) = cqueue::buffer_select(value.flags()) {
            let len = value.result() as usize;
            let bids = state.buffers.borrow_mut().lent(bid, len);
            state.ready.push_back(crate::sys::Arrival {
                bids,
                len,
                taken: 0,
            });
        }
        let over = state.armed && !state.throttled && state.queued() > state.ceiling;
        if over {
            state.throttled = true;
        }
        over
    };
    if over_ceiling {
        if let Some(EnqueuedSource { id, queue, status }) = inner.enqueued.as_ref() {
            if *status == EnqueuedStatus::Dispatched {
                queue.borrow_mut().cancel_request(*id);
            }
        }
    }
}

/// Processes a single completion event, if any.
///
/// No user data is `POLL_REMOVE` or `CANCEL`; we won't process it.
fn process_one_event<F, R>(
    cqe: Option<cqueue::Entry>,
    try_process: F,
    post_process: R,
    source_map: Rc<RefCell<SourceMap>>,
) -> Option<Processed>
where
    F: FnOnce(Ref<'_, InnerSource>) -> Option<()>,
    R: FnOnce(RefMut<'_, InnerSource>, io::Result<usize>) -> io::Result<usize>,
{
    if let Some(value) = cqe {
        if value.user_data() == 0 {
            return Some(Processed {
                woke: false,
                retired: true,
            });
        }
        let id = from_user_data(value.user_data());

        if cqueue::more(value.flags()) {
            let woke = source_map.borrow_mut().peek_source_mut(id, |mut inner| {
                queue_multishot_arrival(&mut inner, &value);
                inner.wakers.wake_waiters()
            });
            return Some(Processed {
                woke,
                retired: false,
            });
        }

        let src = source_map.borrow_mut().consume_source(id);

        let result = value.result();

        let mut woke = false;
        if try_process(src.borrow()).is_none() {
            {
                let mut inner = src.borrow_mut();
                queue_multishot_arrival(&mut inner, &value);
                if let SourceType::RecvMulti(state) = &mut inner.source_type {
                    state.armed = false;
                }
            }
            let res = Some(post_process(src.borrow_mut(), transmute_error(result)));
            let mut inner_source = src.borrow_mut();
            inner_source.wakers.result = res;
            woke = inner_source.wakers.wake_waiters();
        }
        return Some(Processed {
            woke,
            retired: true,
        });
    }
    None
}

type SourceMap = FreeList<Pin<Rc<RefCell<InnerSource>>>>;
pub(crate) type SourceId = Idx<Pin<Rc<RefCell<InnerSource>>>>;
fn from_user_data(user_data: u64) -> SourceId {
    SourceId::from_raw((user_data - 1) as usize)
}
fn to_user_data(id: SourceId) -> u64 {
    id.to_raw() as u64 + 1
}

impl SourceMap {
    fn add_source(&mut self, source: &Source, queue: ReactorQueue) -> SourceId {
        let item = source.inner.clone();
        let id = self.alloc(item);
        let status = EnqueuedStatus::Enqueued;
        source
            .inner
            .borrow_mut()
            .enqueued
            .replace(EnqueuedSource { id, queue, status });
        id
    }

    fn peek_source_mut<R, Fn: for<'a> FnOnce(RefMut<'a, InnerSource>) -> R>(
        &mut self,
        id: SourceId,
        f: Fn,
    ) -> R {
        f(self[id].borrow_mut())
    }

    fn consume_source(&mut self, id: SourceId) -> Pin<Rc<RefCell<InnerSource>>> {
        let source = self.dealloc(id);
        source.borrow_mut().enqueued.take();
        source
    }
}

#[derive(Debug)]
pub(crate) struct UringQueueState {
    submissions: VecDeque<UringDescriptor>,
    cancellations: VecDeque<UringDescriptor>,
}

pub(crate) type ReactorQueue = Rc<RefCell<UringQueueState>>;

impl UringQueueState {
    fn with_capacity(cap: usize) -> ReactorQueue {
        Rc::new(RefCell::new(UringQueueState {
            submissions: VecDeque::with_capacity(cap),
            cancellations: VecDeque::new(),
        }))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.submissions.is_empty() && self.cancellations.is_empty()
    }

    pub(crate) fn cancel_request(&mut self, id: SourceId) {
        self.cancellations.push_back(UringDescriptor {
            args: UringOpDescriptor::Cancel(to_user_data(id)),
            fd: -1,
            flags: squeue::Flags::empty(),
            user_data: 0,
        });
    }
}

pub(crate) trait UringCommon {
    fn submission_queue(&mut self) -> ReactorQueue;
    fn submit_sqes(&mut self) -> io::Result<usize>;
    /// `&mut self` because the ring's queue accessors need exclusive access.
    fn waiting_kernel_submission(&mut self) -> usize;
    #[allow(unused)]
    fn in_kernel(&self) -> usize;
    fn waiting_kernel_collection(&mut self) -> usize;
    fn needs_kernel_enter(&mut self) -> bool;
    fn can_sleep(&mut self) -> bool;
    /// None if it wasn't possible to acquire an `sqe`. `Some(true)` if it was
    /// possible and there was something to dispatch. `Some(false)` if there
    /// was nothing to dispatch
    fn submit_one_event(&mut self, queue: &mut VecDeque<UringDescriptor>) -> Option<bool>;
    /// Return `None` if no event is completed, `Some(true)` for a task is woken
    /// up and `Some(false)` for not.
    fn consume_one_event(&mut self) -> Option<bool>;
    fn name(&self) -> &'static str;
    fn io_stats_mut(&mut self) -> &mut RingIoStats;
    fn io_stats_for_task_queue_mut(&mut self, handle: TaskQueueHandle) -> &mut RingIoStats;
    fn submitter(&mut self) -> io_uring::Submitter<'_>;
    fn may_rush(&self) -> bool {
        true
    }

    fn consume_sqe_queue(
        &mut self,
        queue: &mut VecDeque<UringDescriptor>,
        mut dispatch: bool,
    ) -> io::Result<usize> {
        loop {
            match self.submit_one_event(queue) {
                None => {
                    dispatch = true;
                    break;
                }
                Some(true) => {}
                Some(false) => break,
            }
        }

        if dispatch && self.needs_kernel_enter() {
            self.submit_sqes()
        } else {
            Ok(0)
        }
    }

    /// We will not dispatch the cancellation queue unless we need to.
    /// Dispatches will come from the submission queue.
    fn consume_cancellation_queue(&mut self) -> io::Result<usize> {
        let q = self.submission_queue();
        let mut queue = q.borrow_mut();
        self.consume_sqe_queue(&mut queue.cancellations, false)
    }

    fn consume_submission_queue(&mut self) -> io::Result<usize> {
        let q = self.submission_queue();
        let mut queue = q.borrow_mut();
        self.consume_sqe_queue(&mut queue.submissions, true)
    }

    fn consume_completion_queue(&mut self, woke: &mut usize) -> usize {
        let mut completed = 0;
        loop {
            match self.consume_one_event() {
                None => break,
                Some(false) => completed += 1,
                Some(true) => {
                    completed += 1;
                    *woke += 1;
                }
            }
        }
        completed
    }

    /// It is important to process cancellations as soon as we see them,
    /// which is why they go into a separate queue. The reason is that
    /// cancellations can be racy if they are left to their own devices.
    ///
    /// Imagine that you have a write request to fd 3 and wants to cancel it.
    /// But before the cancellation is run fd 3 gets closed and another file
    /// is opened with the same fd.
    fn flush_cancellations(&mut self, woke: &mut usize) {
        let mut cnt = 0;
        loop {
            if self.consume_cancellation_queue().is_ok() {
                break;
            }
            self.consume_completion_queue(woke);
            cnt += 1;
            if cnt > 1_000_000 {
                panic!(
                    "i tried literally a million times but couldn't flush to the {} ring",
                    self.name()
                );
            }
        }
        self.consume_completion_queue(woke);
    }

    fn poll(&mut self, woke: &mut usize) -> io::Result<()> {
        self.consume_cancellation_queue()
            .or_else(Reactor::busy_ok)
            .or_else(Reactor::again_ok)
            .or_else(Reactor::intr_ok)?;
        self.consume_submission_queue()
            .or_else(Reactor::busy_ok)
            .or_else(Reactor::again_ok)
            .or_else(Reactor::intr_ok)?;
        self.consume_completion_queue(woke);
        Ok(())
    }
}

struct PollRing {
    ring: IoUring,
    size: usize,
    submission_queue: ReactorQueue,
    allocator: Rc<UringBufferAllocator>,
    stats: RingIoStats,
    task_queue_stats: AHashMap<TaskQueueHandle, RingIoStats>,
    source_map: Rc<RefCell<SourceMap>>,
    in_kernel: usize,
}

impl PollRing {
    fn new(
        size: usize,
        allocator: Rc<UringBufferAllocator>,
        source_map: Rc<RefCell<SourceMap>>,
    ) -> io::Result<Self> {
        let ring = IoUring::builder().setup_iopoll().build(size as _)?;
        Ok(PollRing {
            size,
            ring,
            submission_queue: UringQueueState::with_capacity(size * 4),
            allocator,
            stats: RingIoStats::default(),
            task_queue_stats: AHashMap::new(),
            source_map,
            in_kernel: 0,
        })
    }

    pub(crate) fn alloc_dma_buffer(&mut self, size: usize) -> DmaBuffer {
        self.allocator.new_buffer(size).unwrap()
    }
}

impl UringCommon for PollRing {
    fn name(&self) -> &'static str {
        "poll"
    }

    fn io_stats_mut(&mut self) -> &mut RingIoStats {
        &mut self.stats
    }

    fn io_stats_for_task_queue_mut(&mut self, handle: TaskQueueHandle) -> &mut RingIoStats {
        self.task_queue_stats.entry(handle).or_default()
    }

    fn submitter(&mut self) -> io_uring::Submitter<'_> {
        self.ring.submitter()
    }

    /// We need to enter the kernel to submit and collect CQEs so if the number
    /// of submitted requests doesn't match the number of request we collected,
    /// we need to poll.
    fn needs_kernel_enter(&mut self) -> bool {
        self.in_kernel > 0 || self.waiting_kernel_submission() > 0
    }

    fn can_sleep(&mut self) -> bool {
        self.submission_queue.borrow().is_empty() && !self.needs_kernel_enter()
    }

    fn waiting_kernel_submission(&mut self) -> usize {
        self.ring.submission().len()
    }

    fn in_kernel(&self) -> usize {
        self.in_kernel
    }

    fn waiting_kernel_collection(&mut self) -> usize {
        self.ring.completion().len()
    }

    fn submission_queue(&mut self) -> ReactorQueue {
        self.submission_queue.clone()
    }

    fn submit_sqes(&mut self) -> io::Result<usize> {
        let x = self.ring.submit()?;
        self.in_kernel += x;
        Ok(x)
    }

    /// Reaps the completion before the closure below borrows `self`, since
    /// the completion queue borrows the ring exclusively.
    fn consume_one_event(&mut self) -> Option<bool> {
        let source_map = self.source_map.clone();
        let cqe = self.ring.completion().next();
        process_one_event(
            cqe,
            |_| None,
            |mut src, res| {
                record_stats(self, &mut src, &res);
                res
            },
            source_map,
        )
        .map(|processed| {
            if processed.retired {
                self.in_kernel -= 1;
            }
            processed.woke
        })
    }

    fn submit_one_event(&mut self, queue: &mut VecDeque<UringDescriptor>) -> Option<bool> {
        submit_event_chain(
            &mut self.source_map.borrow_mut(),
            &mut self.ring,
            self.allocator.clone(),
            queue,
            self.size,
        )
    }
}

/// The buffer group a ring's provided buffers are registered under. One
/// group per ring is all multishot receive needs.
const RECV_BUFFER_GROUP: u16 = 0;

/// `IORING_ENTER_GETEVENTS`, the same value on every architecture: an entry
/// that asks for events, which is what runs a deferred ring's task work.
const GETEVENTS: u32 = 1;

/// Receive buffers lent to the kernel through a provided-buffer ring.
///
/// `entries` is the ring the kernel reads buffer descriptors from and
/// `memory` the buffers themselves, `count` of `size` bytes each, both
/// page-aligned and alive for as long as the registration. Handing a buffer
/// back is a write to the ring's tail, no syscall.
///
/// The kernel wants the ring's entry count to be a power of two; the number
/// of buffers in it is anyone's, so `ring_entries` is the power of two at or
/// above `count` and the buffers are exactly `count`. Rounding the buffers
/// up instead cost up to twice the memory asked for.
pub(crate) struct ProvidedBuffers {
    entries: *mut types::BufRingEntry,
    memory: *mut u8,
    ring_entries: u16,
    count: u16,
    size: usize,
    tail: u16,
    /// One bit per buffer, set while the kernel has filled it and nobody has
    /// given it back. This is the invariant the unsafe below rests on: a
    /// buffer is read only while lent, and given back exactly once. A
    /// counter hid a double return; the set catches it.
    lent: Vec<u64>,
    /// The buffers in the order the kernel will take them, which is the
    /// order they were given back in. A bundled completion names only its
    /// first buffer; the rest are the next entries here.
    order: std::collections::VecDeque<u16>,
}

impl ProvidedBuffers {
    /// Registers `count` buffers of `size` bytes with `ring`. `count` is
    /// between 1 and 32768 and `size` between 1 and the kernel's 32-bit
    /// length, which [`RecvBuffers`](crate::RecvBuffers) guarantees and this
    /// checks again, once per executor, because the allocations below are
    /// unsafe on a zero or wrapped size. The memory must outlive the
    /// registration and every request armed on it: the kernel cancels armed
    /// requests when the ring is released, and the reactor keeps this behind
    /// an `Rc` that every multishot source holds.
    fn register(ring: &IoUring, count: usize, size: usize) -> io::Result<Self> {
        assert!(
            (1..=32768).contains(&count) && (1..=u32::MAX as usize).contains(&size),
            "provided buffers: {count} buffers of {size} bytes is outside what the kernel takes"
        );
        let ring_entries = count.next_power_of_two() as u16;
        let bytes = count
            .checked_mul(size)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        let count = count as u16;
        let ring_layout = Layout::from_size_align(
            ring_entries as usize * size_of::<types::BufRingEntry>(),
            4096,
        )
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let memory_layout = Layout::from_size_align(bytes, 4096)
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let entries =
            unsafe { alloc::alloc::alloc_zeroed(ring_layout) } as *mut types::BufRingEntry;
        if entries.is_null() {
            return Err(io::Error::from_raw_os_error(libc::ENOMEM));
        }
        let memory = unsafe { alloc::alloc::alloc_zeroed(memory_layout) };
        if memory.is_null() {
            unsafe { alloc::alloc::dealloc(entries as *mut u8, ring_layout) };
            return Err(io::Error::from_raw_os_error(libc::ENOMEM));
        }
        if let Err(err) = unsafe {
            ring.submitter().register_buf_ring_with_flags(
                entries as u64,
                ring_entries,
                RECV_BUFFER_GROUP,
                0,
            )
        } {
            unsafe {
                alloc::alloc::dealloc(entries as *mut u8, ring_layout);
                alloc::alloc::dealloc(memory, memory_layout);
            }
            return Err(err);
        }
        let mut buffers = ProvidedBuffers {
            entries,
            memory,
            ring_entries,
            count,
            size,
            tail: 0,
            lent: vec![0; (count as usize).div_ceil(64)],
            order: std::collections::VecDeque::with_capacity(count as usize),
        };
        for bid in 0..count {
            buffers.lent[bid as usize / 64] |= 1 << (bid % 64);
            buffers.give_back(bid);
        }
        buffers.publish();
        Ok(buffers)
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> usize {
        self.count as usize
    }

    pub(crate) fn size(&self) -> usize {
        self.size
    }

    /// How many buffers are filled and waiting to be given back.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.lent.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Where buffer `bid` starts, for a test that checks the layout without
    /// borrowing a buffer the kernel holds.
    #[cfg(test)]
    pub(crate) fn address(&self, bid: u16) -> usize {
        self.memory as usize + bid as usize * self.size
    }

    fn is_lent(&self, bid: u16) -> bool {
        self.lent[bid as usize / 64] & (1 << (bid % 64)) != 0
    }

    /// Records that the kernel filled `len` bytes starting at buffer
    /// `first`, over as many consecutive buffers as that takes, and returns
    /// them in order. They are the receiver's to give back.
    fn lent(&mut self, first: u16, len: usize) -> smallvec::SmallVec<[u16; 4]> {
        let needed = if len == 0 { 1 } else { len.div_ceil(self.size) };
        let mut bids = smallvec::SmallVec::new();
        for i in 0..needed {
            let bid = self
                .order
                .pop_front()
                .expect("the kernel filled a buffer it was never given");
            if i == 0 {
                assert_eq!(
                    bid, first,
                    "a completion named a buffer out of ring order: the kernel and the ring \
                     disagree on which buffers hold whose bytes"
                );
            }
            assert!(
                !self.is_lent(bid),
                "the kernel filled buffer {bid} while it was still lent out"
            );
            self.lent[bid as usize / 64] |= 1 << (bid % 64);
            bids.push(bid);
        }
        bids
    }

    /// The bytes the kernel wrote into buffer `bid`, `len` of them. Sound
    /// because `bid` is lent: the kernel writes a buffer only while it sits
    /// in the ring, a lent buffer is out of the ring until given back, and
    /// giving back needs `&mut self`, which this borrow excludes. `len` is at
    /// most the buffer's size, so the slice stays inside the allocation.
    pub(crate) fn bytes(&self, bid: u16, len: usize) -> &[u8] {
        debug_assert!(self.is_lent(bid) && len <= self.size);
        unsafe { std::slice::from_raw_parts(self.memory.add(bid as usize * self.size), len) }
    }

    /// Queues buffer `bid`, which must be lent, for the kernel to pick
    /// again. Visible to the kernel after [`publish`](Self::publish). Sound
    /// because the ring has `ring_entries` slots and never more than `count`
    /// buffers in it, so the masked tail never reaches a slot the kernel has
    /// not consumed; a buffer given back twice would break that, which is
    /// why the lent set is checked rather than counted.
    pub(crate) fn give_back(&mut self, bid: u16) {
        assert!(
            self.is_lent(bid),
            "buffer {bid} given back while the kernel held it, or twice"
        );
        self.lent[bid as usize / 64] &= !(1 << (bid % 64));
        self.order.push_back(bid);
        let slot = (self.tail & (self.ring_entries - 1)) as usize;
        unsafe {
            let entry = &mut *self.entries.add(slot);
            entry.set_addr(self.memory.add(bid as usize * self.size) as u64);
            entry.set_len(self.size as u32);
            entry.set_bid(bid);
        }
        self.tail = self.tail.wrapping_add(1);
    }

    /// Makes every buffer given back so far visible to the kernel. The
    /// tail lives in the first ring entry, where the kernel reads it with
    /// acquire ordering after this release store; `entries` is alive for as
    /// long as `self`.
    pub(crate) fn publish(&self) {
        unsafe {
            let tail =
                types::BufRingEntry::tail(self.entries) as *const std::sync::atomic::AtomicU16;
            (*tail).store(self.tail, std::sync::atomic::Ordering::Release);
        }
    }
}

impl fmt::Debug for ProvidedBuffers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProvidedBuffers")
            .field("count", &self.count)
            .field("size", &self.size)
            .field(
                "held",
                &self.lent.iter().map(|w| w.count_ones()).sum::<u32>(),
            )
            .finish()
    }
}

impl Drop for ProvidedBuffers {
    /// Frees the two allocations of [`register`](Self::register) with the
    /// layouts they were made with. The ring this was registered with is
    /// declared before it in [`SleepableRing`] and every multishot source
    /// holds this behind an `Rc`, so by the time this runs the ring is
    /// closed, the kernel has cancelled what was armed on it, and no slice
    /// into the memory is live.
    fn drop(&mut self) {
        unsafe {
            alloc::alloc::dealloc(
                self.entries as *mut u8,
                Layout::from_size_align_unchecked(
                    self.ring_entries as usize * size_of::<types::BufRingEntry>(),
                    4096,
                ),
            );
            alloc::alloc::dealloc(
                self.memory,
                Layout::from_size_align_unchecked(self.count as usize * self.size, 4096),
            );
        }
    }
}

struct SleepableRing {
    ring: IoUring,
    /// Declared after `ring` so the registration is released first. Shared
    /// with every multishot source, which gives its buffers back through it.
    provided_buffers: Option<Rc<RefCell<ProvidedBuffers>>>,
    /// Whether the kernel was told to defer this ring's task work until
    /// this thread enters the ring (`IORING_SETUP_DEFER_TASKRUN`, which
    /// needs `IORING_SETUP_SINGLE_ISSUER`). A poll wakeup then only queues
    /// the receive; it runs, and takes everything the socket holds, when the
    /// reactor enters, so a burst of arrivals costs one receive and one
    /// completion instead of one of each per arrival. Nothing such a ring
    /// completes is posted until it is entered, so every entry on it asks
    /// for events, and the reactor enters it once per pass.
    ///
    /// The kernel also reports pending work in the submission queue's flags
    /// (`IORING_SQ_TASKRUN`), and that flag is deliberately never read: a
    /// multishot receive that re-queues itself while the work runs leaves
    /// it set over an empty queue, where no entry clears it, and a reactor
    /// that took it for pending work spun at 140k entries a second.
    cooperative: bool,
    /// Whether a stream reaping inline has already paid the one entry it is
    /// allowed between two reactor passes. Reset by the pass and by a reap
    /// that found something, so a burst of empty polls costs one syscall.
    /// The pass reads it too: an entry a few microseconds old has run the
    /// deferred work already, and a pass that entered again behind it was
    /// a busy poll that found the reply before the sleep could park, at
    /// the cost of the whole core.
    inline_enter_spent: bool,
    size: usize,
    submission_queue: ReactorQueue,
    name: &'static str,
    allocator: Rc<UringBufferAllocator>,
    stats: RingIoStats,
    task_queue_stats: AHashMap<TaskQueueHandle, RingIoStats>,
    source_map: Rc<RefCell<SourceMap>>,
    in_kernel: usize,
}

impl SleepableRing {
    /// Builds the ring, with deferred task work when asked and the kernel
    /// allows it; a kernel before 6.1 refuses the flags and the ring is
    /// built plain.
    fn new(
        size: usize,
        name: &'static str,
        allocator: Rc<UringBufferAllocator>,
        source_map: Rc<RefCell<SourceMap>>,
        cooperative: bool,
    ) -> io::Result<Self> {
        check_uring_support()?;
        let (ring, cooperative) = match cooperative {
            true => match IoUring::builder()
                .setup_single_issuer()
                .setup_defer_taskrun()
                .build(size as _)
            {
                Ok(ring) => (ring, true),
                Err(_) => (IoUring::new(size as _)?, false),
            },
            false => (IoUring::new(size as _)?, false),
        };
        Ok(SleepableRing {
            ring,
            provided_buffers: None,
            cooperative,
            inline_enter_spent: false,
            size,
            submission_queue: UringQueueState::with_capacity(size * 4),
            name,
            allocator,
            stats: RingIoStats::default(),
            task_queue_stats: AHashMap::new(),
            source_map,
            in_kernel: 0,
        })
    }

    fn ring_fd(&self) -> RawFd {
        std::os::unix::io::AsRawFd::as_raw_fd(&self.ring)
    }

    /// Enters the kernel with nothing to submit and nothing to wait for, so
    /// a deferred ring's pending task work runs and its completions are
    /// posted. One syscall for however many are pending, which is the whole
    /// point of deferring. Does nothing on a plain ring, whose work the
    /// kernel runs on its own, or on a ring with no request in flight.
    fn run_deferred_work(&mut self) -> io::Result<()> {
        if !self.cooperative || self.in_kernel == 0 {
            return Ok(());
        }
        self.enter_for_events()
    }

    /// The reactor pass's entry. Skipped on a pass that may sleep, since
    /// the sleep's own entry runs the work and returns at once if it posted
    /// anything; entering before the sleep instead found a depth-1 reply on
    /// the second or third pass and never parked, at the cost of the whole
    /// core. Also skipped when a stream entered since the last pass, since
    /// that entry ran the work, unless the pass comes after a preempt slice,
    /// in which case the stream's entry is a slice old and a reply that
    /// landed behind it would wait a second slice beside a CPU-bound queue.
    /// A busy executor whose readers are all parked gets the entry, which is
    /// what keeps their arrivals surfacing without a sleep.
    fn run_deferred_work_for_pass(
        &mut self,
        may_sleep: bool,
        slice_expired: bool,
    ) -> io::Result<()> {
        let spent = std::mem::replace(&mut self.inline_enter_spent, false);
        if may_sleep || (spent && !slice_expired) {
            return Ok(());
        }
        self.run_deferred_work()
    }

    /// A raw entry with nothing to submit, so the kernel runs the deferred
    /// work; the only argument that needs care is the flags word, and it
    /// carries nothing the kernel reads memory through.
    fn enter_for_events(&mut self) -> io::Result<()> {
        unsafe {
            self.ring
                .submitter()
                .enter::<libc::sigset_t>(0, 0, GETEVENTS, None)
        }
        .map(|_| ())
        .or_else(Reactor::busy_ok)
        .or_else(Reactor::again_ok)
        .or_else(Reactor::intr_ok)
    }

    /// The one entry a stream reaping inline may make between two passes,
    /// when the completion queue is empty: it runs whatever the kernel has
    /// deferred, so a reply already in the socket is seen on the first
    /// poll. A second empty poll before the next pass enters nothing.
    fn run_deferred_work_once(&mut self) -> io::Result<()> {
        if self.inline_enter_spent || !self.ring.completion().is_empty() {
            return Ok(());
        }
        self.inline_enter_spent = true;
        self.run_deferred_work()
    }

    /// This function prepares a timer that fires unconditionally after a
    /// certain duration. The timer is added at the front of the queue such
    /// that it will be the first SQE submitted the next time we enter the
    /// latency ring
    fn prepare_latency_preemption_timer(&mut self, d: Duration) -> Source {
        let source = Source::new(
            IoRequirements::default(),
            -1,
            SourceType::Timeout(TimeSpec64::try_from(d).unwrap(), 0),
            None,
            None,
        );
        let op = match &*source.source_type() {
            SourceType::Timeout(ts, events) => {
                UringOpDescriptor::Timeout(&ts.raw as *const _, *events)
            }
            _ => unreachable!(),
        };

        self.submission_queue()
            .borrow_mut()
            .submissions
            .push_front(UringDescriptor {
                args: op,
                fd: -1,
                flags: squeue::Flags::empty(),
                user_data: to_user_data(
                    self.source_map
                        .borrow_mut()
                        .add_source(&source, self.submission_queue.clone()),
                ),
            });
        source
    }

    /// This function prepares a timer that fires only after a given number of
    /// CQEs are available on the main ring. This timer is linked with a write
    /// to the latency ring's event fd such that this timer triggers a
    /// preemption via the latency ring.
    fn prepare_throughput_preemption_timer(&mut self, min_events: u32, event_fd: RawFd) -> Source {
        assert!(min_events >= 1, "min_events should be at least 1");
        let timer_source = Source::new(
            IoRequirements::default(),
            -1,
            SourceType::Timeout(TimeSpec64::MAX, min_events),
            None,
            None,
        );

        const EVENTFD_WAKEUP: &[u64; 1] = &[1u64; 1];
        let write_op = UringDescriptor {
            fd: event_fd,
            flags: squeue::Flags::empty(),
            user_data: 0,
            args: UringOpDescriptor::Write(EVENTFD_WAKEUP as *const u64 as _, 8, 0),
        };

        let op = match &*timer_source.source_type() {
            SourceType::Timeout(ts, events) => {
                UringOpDescriptor::Timeout(&ts.raw as *const _, *events)
            }
            _ => unreachable!(),
        };
        let queue = self.submission_queue();
        queue.borrow_mut().submissions.push_front(write_op);
        queue.borrow_mut().submissions.push_front(UringDescriptor {
            args: op,
            fd: -1,
            flags: squeue::Flags::IO_LINK,
            user_data: to_user_data(
                self.source_map
                    .borrow_mut()
                    .add_source(&timer_source, queue.clone()),
            ),
        });
        timer_source
    }

    /// Now must wait on the `eventfd` in case someone wants to wake us up.
    /// If we can't then we can't sleep and will just bail immediately.
    ///
    /// The read targets the eventfd source's own buffer, which the
    /// `SourceMap` keeps alive until the completion is reaped. The
    /// `is_full()` check above means the push cannot fail.
    fn install_eventfd(&mut self, eventfd_src: &Source) -> bool {
        if !self.ring.submission().is_full() {
            let op = UringDescriptor {
                fd: eventfd_src.raw(),
                flags: squeue::Flags::empty(),
                user_data: to_user_data(
                    self.source_map
                        .borrow_mut()
                        .add_source(eventfd_src, self.submission_queue.clone()),
                ),
                args: UringOpDescriptor::Read(0, 8),
            };

            let buffer_ptr = {
                match &mut *eventfd_src.source_type_mut() {
                    SourceType::ForeignNotifier(result, _) => result as *mut _ as _,
                    _ => unreachable!("Expected ForeignNotifier source type"),
                }
            };

            let entry = fill_sqe(
                &op,
                |size| {
                    Some(DmaBuffer::with_storage(
                        size,
                        BufferStorage::EventFd(buffer_ptr),
                    ))
                },
                &mut self.source_map.borrow_mut(),
            );
            unsafe {
                self.ring
                    .submission()
                    .push(&entry)
                    .expect("submission queue was checked to have room");
            }

            match &mut *eventfd_src.source_type_mut() {
                SourceType::ForeignNotifier(_, installed) => {
                    *installed = self.submit_sqes().is_ok();
                    *installed
                }
                _ => unreachable!("Expected ForeignNotifier source type"),
            }
        } else {
            false
        }
    }

    /// Sleeps until an event arrives, by preparing an SQE that polls the link
    /// source and links the two rings.
    ///
    /// We have now prepared the SQE that links the two rings; we need to
    /// submit it successfully to be able to safely sleep. If we fail to
    /// submit the `SQE` that links the rings, we can't sleep: waiting here is
    /// unsafe because we could end up waiting much longer than needed.
    ///
    /// The entry stays queued. io-uring can't rewrite a pushed entry and
    /// doesn't need to: it goes out with the next submit and its completion
    /// retires the source. Wakes nobody, costs a spurious CQE.
    ///
    /// On success the rings are linked. Goodnight!
    ///
    /// Can't link rings because we ran out of `CQE`s? Just can't sleep.
    /// Submit what we have; once we're out of here we'll consume them and at
    /// some point will be able to sleep again.
    ///
    /// The polls target the other rings' fds, which outlive this reactor,
    /// and the room check above means the pushes cannot fail.
    fn sleep(&mut self, links: &[Source]) -> io::Result<usize> {
        assert_eq!(
            self.waiting_kernel_submission(),
            0,
            "sleeping with pending SQEs"
        );
        let room = {
            let sq = self.ring.submission();
            sq.capacity() - sq.len()
        };
        if room >= links.len() {
            for link in links {
                let op = UringDescriptor {
                    fd: link.raw(),
                    flags: squeue::Flags::empty(),
                    user_data: to_user_data(
                        self.source_map
                            .borrow_mut()
                            .add_source(link, self.submission_queue.clone()),
                    ),
                    args: UringOpDescriptor::PollAdd(common_flags() | read_flags()),
                };
                let entry = fill_sqe(&op, DmaBuffer::new, &mut self.source_map.borrow_mut());
                unsafe {
                    self.ring
                        .submission()
                        .push(&entry)
                        .expect("submission queue was checked to have room");
                }
            }

            let submitted = self
                .ring
                .submit_and_wait(1)
                .or_else(Reactor::busy_ok)
                .or_else(Reactor::again_ok)
                .or_else(Reactor::intr_ok)?;
            self.in_kernel += submitted;
            if submitted != links.len() {
                Err(io::Error::from_raw_os_error(libc::EBUSY))
            } else {
                Ok(1)
            }
        } else {
            self.submit_sqes()
                .or_else(Reactor::busy_ok)
                .or_else(Reactor::again_ok)
                .or_else(Reactor::intr_ok)
        }
    }
}

impl UringCommon for SleepableRing {
    fn name(&self) -> &'static str {
        self.name
    }

    fn io_stats_mut(&mut self) -> &mut RingIoStats {
        &mut self.stats
    }

    fn io_stats_for_task_queue_mut(&mut self, handle: TaskQueueHandle) -> &mut RingIoStats {
        self.task_queue_stats.entry(handle).or_default()
    }

    fn submitter(&mut self) -> io_uring::Submitter<'_> {
        self.ring.submitter()
    }

    fn may_rush(&self) -> bool {
        false
    }

    /// We only need to enter the kernel to submit SQEs, not to collect CQEs
    /// (the kernel posts the CQEs asynchronously for us)
    fn needs_kernel_enter(&mut self) -> bool {
        self.waiting_kernel_submission() > 0
    }

    fn can_sleep(&mut self) -> bool {
        self.submission_queue.borrow().is_empty()
            && self.waiting_kernel_submission() == 0
            && self.waiting_kernel_collection() == 0
    }

    fn waiting_kernel_submission(&mut self) -> usize {
        self.ring.submission().len()
    }

    fn in_kernel(&self) -> usize {
        self.in_kernel
    }

    fn waiting_kernel_collection(&mut self) -> usize {
        self.ring.completion().len()
    }

    fn submission_queue(&mut self) -> ReactorQueue {
        self.submission_queue.clone()
    }

    /// A deferred ring asks for events on every entry, since nothing it
    /// completes is posted otherwise; a submit-only entry would leave a
    /// cancelled timeout counted as in flight and the ring unable to sleep.
    /// The raw entry submits exactly the entries pushed, which `submit`
    /// would have done the same way.
    fn submit_sqes(&mut self) -> io::Result<usize> {
        let x = if self.cooperative {
            let to_submit = self.ring.submission().len() as u32;
            unsafe {
                self.ring
                    .submitter()
                    .enter::<libc::sigset_t>(to_submit, 0, GETEVENTS, None)?
            }
        } else {
            self.ring.submit()?
        };
        self.in_kernel += x;
        Ok(x)
    }

    /// As above: reap first, then borrow `self` in the post-process closure.
    fn consume_one_event(&mut self) -> Option<bool> {
        let source_map = self.source_map.clone();
        let cqe = self.ring.completion().next();
        process_one_event(
            cqe,
            |source| match source.source_type {
                SourceType::LinkRings => Some(()),
                _ => None,
            },
            |mut src, res| {
                record_stats(self, &mut src, &res);
                if let SourceType::ForeignNotifier(_, installed) = &mut src.source_type {
                    *installed = false;
                }
                res
            },
            source_map,
        )
        .map(|processed| {
            if processed.retired {
                self.in_kernel -= 1;
            }
            processed.woke
        })
    }

    fn submit_one_event(&mut self, queue: &mut VecDeque<UringDescriptor>) -> Option<bool> {
        submit_event_chain(
            &mut self.source_map.borrow_mut(),
            &mut self.ring,
            self.allocator.clone(),
            queue,
            self.size,
        )
    }
}

pub(crate) struct Reactor {
    /// FIXME: it is starting to feel we should clean this up to a Inner pattern
    main_ring: RefCell<SleepableRing>,
    latency_ring: RefCell<SleepableRing>,
    poll_ring: RefCell<PollRing>,
    /// Multishot receives, and nothing else, on a ring that defers its task
    /// work, built only when the executor asked for receive buffers: an
    /// executor without them sleeps as it always did, and measured 9.5%
    /// more CPU per round trip when it had a ring it never used. The
    /// latency ring stays plain so that the preempt timer and a foreign
    /// wake still interrupt a running task, which deferred work cannot: it
    /// is posted only when this thread enters.
    socket_ring: Option<RefCell<SleepableRing>>,

    latency_preemption_timeout_src: Cell<Option<Source>>,
    throughput_preemption_timeout_src: Cell<Option<Source>>,

    /// The main and latency rings' fds. With a socket ring the reactor
    /// sleeps in it, since its deferred work wakes only its own submitter
    /// and never a poller of its fd, and watches the other two from there;
    /// without one it sleeps in the main ring watching the latency ring.
    link_fds: [RawFd; 2],

    /// This keeps the `eventfd` alive. Drop will close it when we're done
    notifier: Arc<sys::SleepNotifier>,
    /// This is the source used to handle the notifications into the ring.
    /// It is reused, unlike the timeout src, because it is possible and likely
    /// that it will be in the ring through many calls to the reactor loop. It only ever gets
    /// completed if this reactor is woken up from another one
    eventfd_src: Source,
    source_map: Rc<RefCell<SourceMap>>,

    blocking_thread: BlockingThreadPool,

    rings_depth: usize,

    /// Set once the kernel has rejected the multishot receive flag.
    recv_multi_refused: Cell<bool>,
    /// Set once the kernel has rejected the bundle flag (before 6.10), after
    /// which multishot receives are armed one buffer per completion.
    bundle_refused: Cell<bool>,
    /// Multishot receives that ended because the buffers ran out, since the
    /// stats were last taken.
    recv_buffers_exhausted: Cell<u64>,
}

pub(crate) fn common_flags() -> PollFlags {
    PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL
}

/// Epoll flags for all possible readability events.
pub(crate) fn read_flags() -> PollFlags {
    PollFlags::POLLIN | PollFlags::POLLPRI
}

macro_rules! consume_rings {
    (into $woke:expr; $( $ring:expr ),+ ) => {{
        let mut consumed = 0;
        $(
            consumed += $ring.consume_completion_queue($woke);
        )*
        consumed
    }}
}
macro_rules! flush_cancellations {
    (into $output:expr; $( $ring:expr ),+ ) => {{
        $(
            $ring.flush_cancellations($output);
        )*
    }}
}

macro_rules! flush_rings {
    ($( $ring:expr ),+ ) => {{
        let mut ret = 0;
        $(
            ret += $ring.consume_submission_queue()
                .or_else(Reactor::busy_ok)
                .or_else(Reactor::again_ok)
                .or_else(Reactor::intr_ok)?;
        )*
        io::Result::Ok(ret)
    }}
}

fn align_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

impl Reactor {
    /// Creates a new reactor.
    ///
    /// Always have at least some small amount of memory for the slab. The
    /// `register_buffers` calls are sound because `registry` borrows the
    /// allocator's arena, which lives as long as the reactor and therefore
    /// outlives the registration.
    pub(crate) fn new(
        notifier: Arc<sys::SleepNotifier>,
        mut io_memory: usize,
        ring_depth: usize,
        blocking_thread: BlockingThreadPool,
        recv_buffers: crate::executor::RecvBuffers,
    ) -> crate::Result<Reactor, ()> {
        const MIN_MEMLOCK_LIMIT: u64 = 512 * 1024;
        let (memlock_limit, _) = Resource::MEMLOCK.get()?;
        if memlock_limit < MIN_MEMLOCK_LIMIT {
            return Err(GlommioError::ReactorError(ReactorErrorKind::MemLockLimit(
                memlock_limit,
                MIN_MEMLOCK_LIMIT,
            )));
        }

        let source_map = Rc::new(RefCell::new(SourceMap::default()));
        io_memory = std::cmp::max(align_up(io_memory, 4096), 65536);

        let allocator = Rc::new(UringBufferAllocator::new(io_memory));
        let registry = {
            let bytes = allocator.as_bytes();
            vec![libc::iovec {
                iov_base: bytes.as_ptr() as *mut libc::c_void,
                iov_len: bytes.len(),
            }]
        };

        let mut main_ring = SleepableRing::new(
            ring_depth,
            "main",
            allocator.clone(),
            source_map.clone(),
            false,
        )?;
        let mut poll_ring = PollRing::new(ring_depth, allocator.clone(), source_map.clone())?;
        let mut latency_ring = SleepableRing::new(
            ring_depth,
            "latency",
            allocator.clone(),
            source_map.clone(),
            false,
        )?;
        let socket_ring = if recv_buffers.count() > 0 {
            let mut socket_ring = SleepableRing::new(
                ring_depth,
                "socket",
                allocator.clone(),
                source_map.clone(),
                true,
            )?;
            match ProvidedBuffers::register(
                &socket_ring.ring,
                recv_buffers.count(),
                recv_buffers.size(),
            ) {
                Ok(buffers) => {
                    socket_ring.provided_buffers = Some(Rc::new(RefCell::new(buffers)));
                    Some(socket_ring)
                }
                Err(err) => {
                    warn!(
                        "this kernel takes no provided-buffer ring ({err}); buffered sockets \
                         will read without multishot receive"
                    );
                    None
                }
            }
        } else {
            None
        };

        match unsafe { main_ring.submitter().register_buffers(&registry) } {
            Err(x) => warn!("Error: registering buffers in the main ring. Skipping{x:#?}"),
            Ok(_) => match unsafe { poll_ring.submitter().register_buffers(&registry) } {
                Err(x) => {
                    warn!("Error: registering buffers in the poll ring. Skipping{x:#?}");
                    main_ring.submitter().unregister_buffers().unwrap();
                }
                Ok(_) => {
                    match unsafe { latency_ring.submitter().register_buffers(&registry) } {
                        Err(x) => {
                            warn!("Error: registering buffers in the poll ring. Skipping{x:#?}");
                            poll_ring.submitter().unregister_buffers().unwrap();
                            main_ring.submitter().unregister_buffers().unwrap();
                        }
                        Ok(_) => {
                            allocator.activate_registered_buffers(0);
                        }
                    };
                }
            },
        }

        let link_fds = [main_ring.ring_fd(), latency_ring.ring_fd()];

        let eventfd_src = Source::new(
            IoRequirements::default(),
            notifier.eventfd_fd(),
            SourceType::ForeignNotifier(0, false),
            None,
            None,
        );

        if !eventfd_src.is_installed().unwrap() {
            latency_ring.install_eventfd(&eventfd_src);
        }

        Ok(Reactor {
            main_ring: RefCell::new(main_ring),
            latency_ring: RefCell::new(latency_ring),
            poll_ring: RefCell::new(poll_ring),
            socket_ring: socket_ring.map(RefCell::new),
            latency_preemption_timeout_src: Cell::new(None),
            throughput_preemption_timeout_src: Cell::new(None),
            blocking_thread,
            link_fds,
            notifier,
            eventfd_src,
            source_map,
            rings_depth: ring_depth,
            recv_multi_refused: Cell::new(false),
            bundle_refused: Cell::new(false),
            recv_buffers_exhausted: Cell::new(0),
        })
    }

    pub(crate) fn id(&self) -> usize {
        self.notifier.id()
    }

    pub(crate) fn ring_depth(&self) -> usize {
        self.rings_depth
    }

    /// The receive buffers lent to the kernel, as `(count, size)`, or `None`
    /// when there are none, by configuration or because the kernel refused.
    #[cfg(test)]
    pub(crate) fn provided_buffers(&self) -> Option<(usize, usize)> {
        self.recv_buffers().map(|b| {
            let b = b.borrow();
            (b.count(), b.size())
        })
    }

    /// The ring of receive buffers multishot sources draw from, when the
    /// executor has one and the kernel has not refused the multishot flag.
    pub(crate) fn recv_buffers(&self) -> Option<Rc<RefCell<ProvidedBuffers>>> {
        if self.recv_multi_refused.get() {
            return None;
        }
        self.socket_ring
            .as_ref()
            .and_then(|ring| ring.borrow().provided_buffers.clone())
    }

    /// Remembers that this kernel answered a multishot receive with EINVAL,
    /// so no further stream tries one.
    pub(crate) fn refuse_recv_multi(&self) {
        self.recv_multi_refused.set(true);
    }

    /// Whether a new multishot receive should ask for bundled completions.
    pub(crate) fn recv_multi_bundles(&self) -> bool {
        !self.bundle_refused.get()
    }

    /// Remembers that this kernel answered a bundled receive with EINVAL.
    pub(crate) fn refuse_bundle(&self) {
        self.bundle_refused.set(true);
    }

    /// Counts a multishot receive the kernel ended for want of a buffer.
    pub(crate) fn note_recv_buffers_exhausted(&self) {
        self.recv_buffers_exhausted
            .set(self.recv_buffers_exhausted.get() + 1);
    }

    /// Consumes what the socket ring has posted, for a stream that found
    /// its queue empty and would otherwise wait for a reactor pass. The
    /// queue is shared memory, so this costs no syscall when the kernel has
    /// already posted; when it has not, the ring is entered once between
    /// passes so deferred work runs, which is what keeps a reply already in
    /// the socket visible on the first poll. The preempt timer lives on the
    /// latency ring and is never touched here, so a reap asks for no pass.
    pub(crate) fn reap_socket_completions(&self) -> io::Result<usize> {
        let mut woke = 0;
        let Some(ring) = self.socket_ring.as_ref() else {
            return Ok(0);
        };
        let mut ring = ring.borrow_mut();
        ring.consume_cancellation_queue()
            .or_else(Reactor::busy_ok)
            .or_else(Reactor::again_ok)
            .or_else(Reactor::intr_ok)?;
        ring.consume_submission_queue()
            .or_else(Reactor::busy_ok)
            .or_else(Reactor::again_ok)
            .or_else(Reactor::intr_ok)?;
        ring.run_deferred_work_once()?;
        let consumed = ring.consume_completion_queue(&mut woke);
        if consumed > 0 {
            ring.inline_enter_spent = false;
        }
        Ok(consumed)
    }

    /// Whether there is a socket ring and it defers its task work.
    #[cfg(test)]
    pub(crate) fn socket_ring_is_cooperative(&self) -> bool {
        self.socket_ring
            .as_ref()
            .is_some_and(|ring| ring.borrow().cooperative)
    }

    pub(crate) fn install_eventfd(&self) {
        if !self.eventfd_src.is_installed().unwrap() {
            self.latency_ring
                .borrow_mut()
                .install_eventfd(&self.eventfd_src);
        }
    }

    pub(crate) fn process_foreign_wakes(&self) -> usize {
        self.notifier.process_foreign_wakes()
    }

    pub(crate) fn alloc_dma_buffer(&self, size: usize) -> DmaBuffer {
        let mut poll_ring = self.poll_ring.borrow_mut();
        poll_ring.alloc_dma_buffer(size)
    }

    pub(crate) fn write_dma(&self, source: &Source, pos: u64) {
        let op = match &*source.source_type() {
            SourceType::Write(
                PollableStatus::NonPollable(DirectIo::Disabled),
                IoBuffer::DmaSource(buf),
            ) => UringOpDescriptor::Write(buf.as_ptr(), buf.len(), pos),
            SourceType::Write(_, IoBuffer::DmaSource(buf)) => match buf.uring_buffer_id() {
                Some(id) => UringOpDescriptor::WriteFixed(buf.as_ptr(), buf.len(), pos, id),
                None => UringOpDescriptor::Write(buf.as_ptr(), buf.len(), pos),
            },
            x => panic!("Unexpected source type for write: {:?}", x),
        };
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    pub(crate) fn write_buffered(&self, source: &Source, pos: u64) {
        let op = match &*source.source_type() {
            SourceType::Write(
                PollableStatus::NonPollable(DirectIo::Disabled),
                IoBuffer::Buffered(buf),
            ) => UringOpDescriptor::Write(buf.as_ptr(), buf.len(), pos),
            x => panic!("Unexpected source type for write: {:?}", x),
        };
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    pub(crate) fn read_dma(&self, source: &Source, pos: u64, size: usize) {
        let op = UringOpDescriptor::ReadFixed(pos, size);
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    pub(crate) fn read_buffered(&self, source: &Source, pos: u64, size: usize) {
        let op = UringOpDescriptor::Read(pos, size);
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    pub(crate) fn poll_ready(&self, source: &Source, flags: PollFlags) {
        let op = UringOpDescriptor::PollAdd(flags);
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    pub(crate) fn send(&self, source: &Source, flags: MsgFlags) {
        let op = match &*source.source_type() {
            SourceType::SockSend(buf) => {
                UringOpDescriptor::SockSend(buf.as_ptr(), buf.len(), flags.bits())
            }
            _ => unreachable!(),
        };
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    pub(crate) fn sendmsg(&self, source: &Source, flags: MsgFlags) {
        let op = match &mut *source.source_type_mut() {
            SourceType::SockSendMsg(_, iov, hdr, addr) => {
                let msg_name = addr.as_ptr() as *mut libc::c_void;
                let msg_namelen = addr.len();

                hdr.msg_iov = iov as *mut libc::iovec;
                hdr.msg_iovlen = 1;
                hdr.msg_name = msg_name;
                hdr.msg_namelen = msg_namelen;

                UringOpDescriptor::SockSendMsg(hdr, flags.bits())
            }
            _ => unreachable!(),
        };
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    pub(crate) fn recv_into(&self, source: &Source, flags: MsgFlags) {
        let op = UringOpDescriptor::SockRecvInto(flags.bits());
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    /// Arms a multishot receive: one request that completes once per
    /// arrival into a buffer from the executor's ring.
    pub(crate) fn recv_multi(&self, source: &Source, bundle: bool) {
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            UringOpDescriptor::RecvMulti(bundle),
            &mut self.source_map.borrow_mut(),
        );
    }

    pub(crate) fn recv(&self, source: &Source, len: usize, flags: MsgFlags) {
        let op = UringOpDescriptor::SockRecv(len, flags.bits());
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    pub(crate) fn recvmsg(&self, source: &Source, len: usize, flags: MsgFlags) {
        let op = UringOpDescriptor::SockRecvMsg(len, flags.bits());
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    pub(crate) fn connect(&self, source: &Source) {
        let op = match &*source.source_type() {
            SourceType::Connect(addr) => UringOpDescriptor::Connect(addr as *const _),
            x => panic!("Unexpected source type for connect: {:?}", x),
        };
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    pub(crate) fn accept(&self, source: &Source) {
        let op = match &mut *source.source_type_mut() {
            SourceType::Accept(addr) => UringOpDescriptor::Accept(addr as *mut SockAddrStorage),
            x => panic!("Unexpected source type for accept: {:?}", x),
        };
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    pub(crate) fn fdatasync(&self, source: &Source) {
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            UringOpDescriptor::FDataSync,
            &mut self.source_map.borrow_mut(),
        );
    }

    pub(crate) fn fallocate(&self, source: &Source, offset: u64, size: u64, flags: libc::c_int) {
        let op = UringOpDescriptor::Fallocate(offset, size, flags);
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    fn enqueue_blocking_request(
        &self,
        source: Pin<Rc<RefCell<InnerSource>>>,
        op: BlockingThreadOp,
    ) -> impl Future<Output = ()> {
        self.blocking_thread.push(op, source)
    }

    pub(crate) fn truncate(&self, source: &Source, size: u64) -> impl Future<Output = ()> {
        let op = BlockingThreadOp::Truncate(source.raw(), size as _);
        self.enqueue_blocking_request(source.inner.clone(), op)
    }

    pub(crate) fn rename(&self, source: &Source) -> impl Future<Output = ()> {
        let (old_path, new_path) = match &*source.source_type() {
            SourceType::Rename(o, n) => (o.clone(), n.clone()),
            _ => panic!("Unexpected source for rename operation"),
        };

        let op = BlockingThreadOp::Rename(old_path, new_path);
        self.enqueue_blocking_request(source.inner.clone(), op)
    }

    pub(crate) fn copy_file_range(&self, source: &Source, pos: u64) -> impl Future<Output = ()> {
        let (fd_in, off_in, len) = match &*source.source_type() {
            SourceType::CopyFileRange(fd_in, off_in, len) => (*fd_in, *off_in, *len),
            _ => panic!("Unexpected source for copy_file_range operation"),
        };

        let op = BlockingThreadOp::CopyFileRange(
            fd_in,
            (off_in).try_into().unwrap(),
            source.raw(),
            pos.try_into().unwrap(),
            len,
        );
        self.enqueue_blocking_request(source.inner.clone(), op)
    }

    pub(crate) fn create_dir(
        &self,
        source: &Source,
        mode: libc::c_int,
    ) -> impl Future<Output = ()> {
        let path = match &*source.source_type() {
            SourceType::CreateDir(p) => p.clone(),
            _ => panic!("Unexpected source for rename operation"),
        };

        let op = BlockingThreadOp::CreateDir(path, mode);
        self.enqueue_blocking_request(source.inner.clone(), op)
    }

    pub(crate) fn remove_file(&self, source: &Source) -> impl Future<Output = ()> {
        let path = match &*source.source_type() {
            SourceType::Remove(path) => path.clone(),
            _ => panic!("Unexpected source for remove operation"),
        };

        let op = BlockingThreadOp::Remove(path);
        self.enqueue_blocking_request(source.inner.clone(), op)
    }

    pub(crate) fn run_blocking(
        &self,
        source: &Source,
        f: Box<dyn FnOnce() + Send + 'static>,
    ) -> impl Future<Output = ()> {
        assert!(matches!(&*source.source_type(), SourceType::BlockingFn));

        let op = BlockingThreadOp::Fn(f);
        self.enqueue_blocking_request(source.inner.clone(), op)
    }

    pub(crate) fn close(&self, source: &Source) {
        let op = UringOpDescriptor::Close;
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    pub(crate) fn statx_fd(&self, source: &Source) {
        let op = match &*source.source_type() {
            SourceType::Statx(buf) => {
                let buf = buf.as_ptr();
                UringOpDescriptor::StatxFd(source.raw(), buf)
            }
            _ => panic!("Unexpected source for statx operation"),
        };
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    pub(crate) fn open_at(&self, source: &Source, flags: libc::c_int, mode: libc::mode_t) {
        let pathptr = match &*source.source_type() {
            SourceType::Open(cstring) => cstring.as_c_str().as_ptr(),
            _ => panic!("Wrong source type!"),
        };
        let op = UringOpDescriptor::Open(pathptr as _, flags, mode as _);
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            op,
            &mut self.source_map.borrow_mut(),
        );
    }

    #[cfg(feature = "bench")]
    pub(crate) fn nop(&self, source: &Source) {
        queue_request_into_ring(
            &mut *self.ring_for_source(source),
            source,
            UringOpDescriptor::Nop,
            &mut self.source_map.borrow_mut(),
        );
    }

    /// io_uring can return `EBUSY` when submitting more requests would
    /// over-commit the system. This is fine: we just need to make sure that we
    /// don't sleep and that we don't failed rushed polls. So we just ignore
    /// this error
    fn busy_ok<T: Default>(x: std::io::Error) -> io::Result<T> {
        match x.raw_os_error() {
            Some(libc::EBUSY) => Ok(Default::default()),
            Some(_) => Err(x),
            None => Err(x),
        }
    }

    /// io_uring can return `EAGAIN` when the CQE queue is full, and we try to
    /// push more requests. This is fine: we just need to make sure that we
    /// don't sleep and that we don't failed rushed polls. So we just ignore
    /// this error
    fn again_ok<T: Default>(x: std::io::Error) -> io::Result<T> {
        match x.raw_os_error() {
            Some(libc::EAGAIN) => Ok(Default::default()),
            Some(_) => Err(x),
            None => Err(x),
        }
    }

    /// io_uring can return `EINTR` if the syscall was interrupted by a signal
    /// delivery. This is fine: we just need to make sure that we
    /// don't sleep and that we don't failed rushed polls. So we just ignore
    /// this error
    fn intr_ok<T: Default>(x: std::io::Error) -> io::Result<T> {
        match x.raw_os_error() {
            Some(libc::EINTR) => Ok(Default::default()),
            Some(_) => Err(x),
            None => Err(x),
        }
    }

    /// We want to go to sleep, but we can only go to sleep in one of the rings,
    /// as we only have one thread. There are more than one sleepable rings, so
    /// what we do is we take advantage of the fact that the ring's `ring_fd` is
    /// pollable and register a `POLL_ADD` event into the ring we will wait on.
    ///
    /// We may not be able to register an `SQE` at this point, so we return an
    /// Error and will just not sleep.
    fn link_rings_and_sleep(&self, ring: &mut SleepableRing, fds: &[RawFd]) -> io::Result<()> {
        let links: Vec<Source> = fds
            .iter()
            .map(|&fd| {
                Source::new(
                    IoRequirements::default(),
                    fd,
                    SourceType::LinkRings,
                    None,
                    None,
                )
            })
            .collect();
        ring.sleep(&links).or_else(Self::busy_ok).map(|_| {})
    }

    pub(crate) fn poll_io(&self, woke: &mut usize) -> io::Result<()> {
        self.poll_ring.borrow_mut().poll(woke)?;
        self.main_ring.borrow_mut().poll(woke)?;
        self.latency_ring.borrow_mut().poll(woke)?;
        if let Some(socket_ring) = self.socket_ring.as_ref() {
            let mut socket_ring = socket_ring.borrow_mut();
            socket_ring.run_deferred_work_for_pass(false, false)?;
            socket_ring.poll(woke)?;
        }
        *woke += self.flush_syscall_thread();
        Ok(())
    }

    pub(crate) fn rush_dispatch(&self, src: &Source, woke: &mut usize) -> io::Result<()> {
        let ring = &mut *self.ring_for_source(src);
        if ring.may_rush() {
            ring.poll(woke)
        } else {
            Ok(())
        }
    }

    /// This function can be passed two timers. Because they play different
    /// roles we keep them separate instead of overloading the same
    /// parameter.
    ///
    /// * The first is the preempt timer. It is designed to take the current
    ///   task queue out of the cpu. If nothing else fires in the latency ring
    ///   the preempt timer will, making need_preempt return true. Currently, we
    ///   always install a preempt timer in the upper layers but from the point
    ///   of view of the io_uring implementation it is optional: it is perfectly
    ///   valid not to have one. Preempt timers are installed by Glommio
    ///   executor runtime.
    ///
    /// * The second is the user timer. It is installed per a user request when
    ///   the user creates a `Timer` (or `TimerAction`).
    ///
    /// At some level, those are both just timers and can be coalesced. And they
    /// certainly are: if there is a user timer that needs to fire in 1ms, and
    /// we want the preempt_timer to also fire around 1ms, there is no need
    /// to register two timers. At the end of the day, all that matters is
    /// that the latency ring flares and that we leave the CPU. That is
    /// because unlike I/O, we don't have one Source per timer, and
    /// parking.rs just keeps them on a wheel and just tell us about what is
    /// the next expiration.
    ///
    /// However, they are also different. The main source of difference is sleep
    /// and wake behavior:
    ///
    /// * When there is no more work to do, and we go to sleep, we do not want
    ///   to register the preempt timer: it is designed to fire periodically to
    ///   take us out of the CPU and if there is no task queue running, we don't
    ///   want to wake up and spend power just for that. However, if there is a
    ///   user timer that needs to fire in the future we must register it.
    ///   Otherwise, we will sleep and never wake up.
    ///
    /// * The user timer point of expiration never changes. So once we register
    ///   it we don't need to rearm it until it fires. But the preempt timer has
    ///   to be rearmed every time. Moreover, it needs to give every task queue
    ///   a fair shot at running. So it needs to be rearmed as close as possible
    ///   to the point where we *leave* this method. For instance: if we spin
    ///   here for 3ms and the preempt timer is 10ms that would leave the next
    ///   task queue just 7ms to run.
    ///
    /// The steps, in order:
    ///
    /// * Consume all events from the rings.
    /// * Cancel the old timer regardless of whether we can sleep: if we
    ///   won't sleep, we will register the new timer with its new value. But
    ///   if we will sleep, there might be a timer registered that needs to
    ///   be removed otherwise we'll wake up when it expires.
    /// * Schedule the throughput-based timeout immediately: it won't matter
    ///   if we end up sleeping.
    /// * Flush cancellations. This will only dispatch if we run out of sqes.
    ///   Which means until `flush_rings!` nothing is really send to the
    ///   kernel... which happens right here. If you ever reorder this code
    ///   just be careful about this dependency.
    /// * Pick up the results of any cancellations.
    /// * If we generated any event so far, we can't sleep. Need to handle
    ///   them.
    ///
    /// When sleeping:
    ///
    /// * It's ok to sleep, but if there is a timer set, we need to make sure
    ///   we wake up to handle it.
    /// * From this moment on the remote executors are aware that we are
    ///   sleeping. We have to sweep the remote channels function once more
    ///   because since last time until now it could be that something
    ///   happened in a remote executor that opened up room. If it did we
    ///   bail on sleep and go process it.
    ///   This final remote-event check can also destroy abandoned tasks.
    ///   Ring borrows are released around the callback so their destructors
    ///   can submit I/O, then reacquired before further ring operations.
    /// * `membarrier::heavy()` -- see
    ///   <https://www.scylladb.com/2018/02/15/memory-barriers-seastar-linux/>
    ///   for details. This translates to `sys_membarrier()` /
    ///   `MEMBARRIER_CMD_PRIVATE_EXPEDITED`.
    /// * May have new cancellations related to the link ring fd; flush them.
    /// * Woke up, so no need to notify us anymore.
    ///
    /// # A note about `need_preempt`
    ///
    /// If in the last call to `consume_rings!` some events completed, the
    /// tail and head would have moved to match. So it does not matter that
    /// events were generated after we registered the timer: since we
    /// consumed them here, `need_preempt()` should be false at this point.
    /// As soon as the next event in the preempt ring completes, though, then
    /// it will be true.
    pub(crate) fn wait<Preempt, F>(
        &self,
        preempt_timer: Preempt,
        user_timer: Option<Duration>,
        mut woke: usize,
        process_remote_channels: F,
    ) -> io::Result<bool>
    where
        Preempt: Fn() -> Option<Duration>,
        F: Fn() -> usize,
    {
        woke += self.flush_syscall_thread();

        let mut poll_ring = self.poll_ring.borrow_mut();
        let mut main_ring = self.main_ring.borrow_mut();
        let mut lat_ring = self.latency_ring.borrow_mut();
        let mut sock_ring = self.socket_ring.as_ref().map(|ring| ring.borrow_mut());

        if let Some(sock_ring) = sock_ring.as_deref_mut() {
            let slice_expired = lat_ring.waiting_kernel_collection() > 0;
            sock_ring.run_deferred_work_for_pass(preempt_timer().is_none(), slice_expired)?;
            consume_rings!(into &mut woke; sock_ring);
        }
        consume_rings!(into &mut woke; lat_ring, poll_ring, main_ring);

        drop(self.latency_preemption_timeout_src.take());

        self.throughput_preemption_timeout_src.replace(Some(
            main_ring.prepare_throughput_preemption_timer(
                self.ring_depth() as u32,
                self.eventfd_src.raw(),
            ),
        ));

        flush_cancellations!(into &mut woke; lat_ring, poll_ring, main_ring);
        flush_rings!(lat_ring, poll_ring, main_ring)?;
        consume_rings!(into &mut woke; lat_ring, poll_ring, main_ring);
        if let Some(sock_ring) = sock_ring.as_deref_mut() {
            flush_cancellations!(into &mut woke; sock_ring);
            flush_rings!(sock_ring)?;
            consume_rings!(into &mut woke; sock_ring);
        }

        let should_sleep = preempt_timer().is_none()
            && (woke == 0)
            && poll_ring.can_sleep()
            && main_ring.can_sleep()
            && lat_ring.can_sleep()
            && sock_ring.as_deref_mut().is_none_or(|ring| ring.can_sleep());

        if should_sleep {
            if let Some(dur) = user_timer {
                self.latency_preemption_timeout_src
                    .set(Some(lat_ring.prepare_latency_preemption_timer(dur)));
                assert!(flush_rings!(lat_ring)? > 0);
            }
            self.notifier.prepare_to_sleep();
            membarrier::heavy();
            drop((poll_ring, main_ring, lat_ring, sock_ring));
            let events = process_remote_channels() + self.flush_syscall_thread();
            poll_ring = self.poll_ring.borrow_mut();
            main_ring = self.main_ring.borrow_mut();
            lat_ring = self.latency_ring.borrow_mut();
            sock_ring = self.socket_ring.as_ref().map(|ring| ring.borrow_mut());
            if events == 0 {
                if self.eventfd_src.is_installed().unwrap() {
                    match sock_ring.as_deref_mut() {
                        Some(sock_ring) => self
                            .link_rings_and_sleep(sock_ring, &self.link_fds)
                            .expect("some error"),
                        None => self
                            .link_rings_and_sleep(&mut main_ring, &self.link_fds[1..])
                            .expect("some error"),
                    }
                    flush_cancellations!(into &mut 0; lat_ring, poll_ring, main_ring);
                    flush_rings!(lat_ring, poll_ring, main_ring)?;
                    consume_rings!(into &mut 0; lat_ring, poll_ring, main_ring);
                    if let Some(sock_ring) = sock_ring.as_deref_mut() {
                        flush_cancellations!(into &mut 0; sock_ring);
                        flush_rings!(sock_ring)?;
                        consume_rings!(into &mut 0; sock_ring);
                    }
                }
                self.notifier.wake_up();
            }
        }

        if let Some(preempt) = preempt_timer() {
            self.latency_preemption_timeout_src
                .set(Some(lat_ring.prepare_latency_preemption_timer(preempt)));
            flush_rings!(lat_ring, main_ring)?;
        }

        Ok(should_sleep)
    }

    pub(crate) fn flush_syscall_thread(&self) -> usize {
        self.blocking_thread.flush()
    }

    /// The status must not outlive the ring, and the `Reactor` that stores it
    /// owns this whole structure. Bound to a local first because the
    /// `CompletionQueue` it comes from only borrows `lat_ring`.
    pub(crate) fn preempt_status(&self) -> CompletionStatus {
        let mut lat_ring = self.latency_ring.borrow_mut();
        let status = unsafe { lat_ring.ring.completion().status() };
        status
    }

    /// RAII-truncate asynchronously files that required it, e.g. because of
    /// padded writes, but were not closed explicitly.
    ///
    /// Actually synchronous for now!
    pub(crate) fn async_truncate(&self, fd: RawFd, size: u64) {
        let _ = sys::truncate_file(fd, size);
    }

    /// RAII-close asynchronously files that were not closed explicitly.
    /// We can't do this through a Source, because the Source will be dropped
    /// when the file is dropped.
    pub(crate) fn async_close(&self, fd: RawFd) {
        let q = self.main_ring.borrow_mut().submission_queue();
        let mut queue = q.borrow_mut();
        queue.submissions.push_back(UringDescriptor {
            args: UringOpDescriptor::Close,
            fd,
            flags: squeue::Flags::empty(),
            user_data: 0,
        });
    }

    /// Dispatch requests according to the following rules:
    /// * Disk reads/writes go to the poll ring if possible, or the main ring
    ///   otherwise;
    /// * Network Rx and connect/accept go the latency ring;
    /// * Every other request are dispatched to the main ring;
    ///
    /// We avoid putting requests that come in high numbers on the latency
    /// ring because the more request we issue there, the less effective it
    /// becomes.
    pub(crate) fn ring_for_source(&self, source: &Source) -> RefMut<'_, dyn UringCommon> {
        match &*source.source_type() {
            SourceType::Read(p, _) | SourceType::Write(p, _) => match p {
                PollableStatus::Pollable => self.poll_ring.borrow_mut(),
                PollableStatus::NonPollable(_) => self.main_ring.borrow_mut(),
            },
            SourceType::RecvMulti(_) => self
                .socket_ring
                .as_ref()
                .expect("a multishot receive is armed only through the socket ring's buffers")
                .borrow_mut(),
            SourceType::SockRecv(_)
            | SourceType::SockRecvMsg(_, _, _, _)
            | SourceType::Accept(_)
            | SourceType::Connect(_) => self.latency_ring.borrow_mut(),
            SourceType::Invalid => {
                unreachable!("called ring_for_source on invalid source")
            }
            _ => self.main_ring.borrow_mut(),
        }
    }

    pub fn io_stats(&self) -> IoStats {
        IoStats::new(
            std::mem::take(&mut self.main_ring.borrow_mut().stats),
            std::mem::take(&mut self.latency_ring.borrow_mut().stats),
            std::mem::take(&mut self.poll_ring.borrow_mut().stats),
            self.recv_buffers_exhausted.take(),
        )
    }

    pub(crate) fn task_queue_io_stats(&self, h: &TaskQueueHandle) -> Option<IoStats> {
        let main = self
            .main_ring
            .borrow_mut()
            .task_queue_stats
            .get_mut(h)
            .map(std::mem::take);
        let lat = self
            .latency_ring
            .borrow_mut()
            .task_queue_stats
            .get_mut(h)
            .map(std::mem::take);
        let poll = self
            .poll_ring
            .borrow_mut()
            .task_queue_stats
            .get_mut(h)
            .map(std::mem::take);

        if let (None, None, None) = (&main, &lat, &poll) {
            None
        } else {
            Some(IoStats::new(
                main.unwrap_or_default(),
                lat.unwrap_or_default(),
                poll.unwrap_or_default(),
                0,
            ))
        }
    }
}

fn queue_request_into_ring(
    ring: &mut (impl UringCommon + ?Sized),
    source: &Source,
    descriptor: UringOpDescriptor,
    source_map: &mut SourceMap,
) {
    let mut inner = source.inner.borrow_mut();
    if records_latency(&inner) {
        inner.wakers.queued_at = Some(Instant::now());
    }
    drop(inner);
    let q = ring.submission_queue();
    let id = source_map.add_source(source, Rc::clone(&q));

    let flags = match &*source.timeout_ref() {
        Some(_) => squeue::Flags::IO_LINK,
        _ => squeue::Flags::empty(),
    };

    let mut queue = q.borrow_mut();
    queue.submissions.push_back(UringDescriptor {
        args: descriptor,
        fd: source.raw(),
        flags,
        user_data: to_user_data(id),
    });

    if let Some(ref ts) = &*source.timeout_ref() {
        queue.submissions.push_back(UringDescriptor {
            args: UringOpDescriptor::LinkTimeout(&ts.raw as *const _),
            flags: squeue::Flags::empty(),
            fd: -1,
            user_data: 0,
        });
    }
}

/// Closes the descriptors `async_close` queued that never reached a ring.
impl Drop for Reactor {
    fn drop(&mut self) {
        let queues = [
            self.main_ring.get_mut().submission_queue(),
            self.latency_ring.get_mut().submission_queue(),
            self.poll_ring.get_mut().submission_queue(),
        ];
        let socket_queue = self
            .socket_ring
            .as_mut()
            .map(|ring| ring.get_mut().submission_queue());
        for queue in queues.into_iter().chain(socket_queue) {
            for op in queue.borrow().submissions.iter() {
                if let (UringOpDescriptor::Close, 0) = (&op.args, op.user_data) {
                    let _ = nix::unistd::close(op.fd);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::PoolPlacement;
    use std::time::Instant;

    use super::*;

    /// The probe list is the only thing standing between an unsupported
    /// kernel and an -EINVAL on a completion nobody is expecting, so it has
    /// to name every opcode fill_sqe can build.
    #[test]
    fn probes_every_opcode_glommio_submits() {
        let submitted = [
            opcode::Nop::CODE,
            opcode::Fsync::CODE,
            opcode::ReadFixed::CODE,
            opcode::WriteFixed::CODE,
            opcode::PollAdd::CODE,
            opcode::PollRemove::CODE,
            opcode::SendMsg::CODE,
            opcode::RecvMsg::CODE,
            opcode::Timeout::CODE,
            opcode::TimeoutRemove::CODE,
            opcode::Accept::CODE,
            opcode::LinkTimeout::CODE,
            opcode::Connect::CODE,
            opcode::Fallocate::CODE,
            opcode::OpenAt::CODE,
            opcode::Close::CODE,
            opcode::Statx::CODE,
            opcode::Read::CODE,
            opcode::Write::CODE,
            opcode::Send::CODE,
            opcode::Recv::CODE,
            opcode::AsyncCancel::CODE,
        ];

        for code in submitted {
            assert!(
                GLOMMIO_URING_OPS.iter().any(|(_, probed)| *probed == code),
                "opcode {code} is submitted but never probed"
            );
        }
    }

    /// The pool the builder asked for is the pool the latency ring holds.
    #[test]
    fn provided_buffers_follow_the_builder() {
        let ex = crate::LocalExecutorBuilder::new(crate::Placement::Unbound)
            .recv_buffers(crate::RecvBuffers::exact(64, 4096))
            .make()
            .unwrap();
        ex.run(async {
            assert_eq!(
                crate::executor().reactor().sys.provided_buffers(),
                Some((64, 4096))
            );
        });
    }

    /// A count that is not a power of two is kept: the kernel's rule is on
    /// the ring's entries, not on the buffers in it, and every buffer asked
    /// for is one the kernel can take.
    #[test]
    fn a_count_that_is_not_a_power_of_two_is_kept_and_usable() {
        let ex = crate::LocalExecutorBuilder::new(crate::Placement::Unbound)
            .recv_buffers(crate::RecvBuffers::exact(3, 16))
            .make()
            .unwrap();
        ex.run(async {
            assert_eq!(
                crate::executor().reactor().sys.provided_buffers(),
                Some((3, 16))
            );
            let (mut writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
            use std::os::fd::AsRawFd as _;
            let reactor = crate::executor().reactor();
            let source = reactor.recv_multi(reader.as_raw_fd(), usize::MAX).unwrap();
            settle(&reactor);
            for msg in [&b"aaaa"[..], b"bbbb", b"cccc"] {
                std::io::Write::write_all(&mut writer, msg).unwrap();
                settle(&reactor);
            }
            let (ready, _) = arrivals(&source);
            assert_eq!(
                ready.concat(),
                b"aaaabbbbcccc",
                "the third buffer was not usable"
            );
        });
    }

    /// Zero buffers means no ring registered at all.
    #[test]
    fn zero_provided_buffers_registers_nothing() {
        let ex = crate::LocalExecutorBuilder::new(crate::Placement::Unbound)
            .recv_buffers(crate::RecvBuffers::exact(0, 4096))
            .make()
            .unwrap();
        ex.run(async {
            assert_eq!(crate::executor().reactor().sys.provided_buffers(), None);
        });
    }

    /// Drives submission and completion without sleeping, the way a reactor
    /// pass does, after giving the kernel a moment to deliver.
    fn settle(reactor: &crate::reactor::Reactor) {
        std::thread::sleep(Duration::from_millis(20));
        let mut woke = 0;
        reactor.sys.poll_io(&mut woke).unwrap();
    }

    fn arrivals(source: &Source) -> (Vec<Vec<u8>>, bool) {
        match &*source.source_type() {
            SourceType::RecvMulti(state) => {
                let buffers = state.buffers.borrow();
                (
                    state
                        .ready
                        .iter()
                        .map(|a| {
                            let mut out = vec![0u8; a.len];
                            a.copy_to(&buffers, 0, &mut out);
                            out
                        })
                        .collect(),
                    state.armed,
                )
            }
            other => panic!("not a multishot source: {other:?}"),
        }
    }

    /// One request, a completion per arrival, and the kernel's end-of-file
    /// retires it with a zero result.
    #[test]
    fn a_multishot_source_queues_every_arrival_and_retires_on_eof() {
        let ex = crate::LocalExecutorBuilder::new(crate::Placement::Unbound)
            .recv_buffers(crate::RecvBuffers::exact(8, 64))
            .make()
            .unwrap();
        ex.run(async {
            let (mut writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
            use std::os::fd::AsRawFd as _;
            let reactor = crate::executor().reactor();
            let source = reactor.recv_multi(reader.as_raw_fd(), usize::MAX).unwrap();
            settle(&reactor);

            for msg in [&b"one"[..], b"two", b"three"] {
                std::io::Write::write_all(&mut writer, msg).unwrap();
                settle(&reactor);
            }
            let (ready, armed) = arrivals(&source);
            assert_eq!(ready.concat(), b"onetwothree");
            assert!(armed, "the kernel dropped the request before end of file");
            assert!(source.result().is_none(), "retired before end of file");
            let buffers = reactor.sys.recv_buffers().unwrap();
            assert_eq!(buffers.borrow().held(), ready.len());

            drop(writer);
            settle(&reactor);
            let (_, armed) = arrivals(&source);
            assert!(!armed);
            assert_eq!(source.result().unwrap().unwrap(), 0);

            drop(source);
            assert_eq!(buffers.borrow().held(), 0, "buffers leaked with the source");
        });
    }

    /// Running out of buffers ends the request rather than losing data: what
    /// arrived is queued, the rest is still in the socket for a re-armed
    /// request. A plain multishot ends with ENOBUFS; a bundled one ends on
    /// the arrival that took the last buffer.
    #[test]
    fn a_multishot_source_stops_with_enobufs_when_the_ring_runs_dry() {
        let ex = crate::LocalExecutorBuilder::new(crate::Placement::Unbound)
            .recv_buffers(crate::RecvBuffers::exact(2, 16))
            .make()
            .unwrap();
        ex.run(async {
            let (mut writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
            use std::os::fd::AsRawFd as _;
            let reactor = crate::executor().reactor();
            let source = reactor.recv_multi(reader.as_raw_fd(), usize::MAX).unwrap();
            settle(&reactor);

            for msg in [&b"aaaa"[..], b"bbbb", b"cccc"] {
                std::io::Write::write_all(&mut writer, msg).unwrap();
                settle(&reactor);
            }
            let (ready, armed) = arrivals(&source);
            assert!(!armed, "two buffers took three arrivals");
            assert_eq!(ready.concat(), b"aaaabbbb");
            match source.result().unwrap() {
                Err(err) => assert_eq!(err.raw_os_error(), Some(libc::ENOBUFS)),
                Ok(n) => assert_eq!(n, 4, "a bundled request ends with its last arrival"),
            }

            drop(source);
            let again = reactor.recv_multi(reader.as_raw_fd(), usize::MAX).unwrap();
            settle(&reactor);
            let (ready, armed) = arrivals(&again);
            assert!(armed);
            assert_eq!(ready.concat(), b"cccc");
        });
    }

    /// On a kernel that takes the flags, an executor with receive buffers
    /// has a socket ring and it defers its work; one without buffers has no
    /// such ring at all, and sleeps as it always did.
    #[test]
    fn the_socket_ring_exists_with_buffers_and_defers_its_task_work() {
        let ex = crate::LocalExecutorBuilder::new(crate::Placement::Unbound)
            .recv_buffers(crate::RecvBuffers::exact(8, 64))
            .make()
            .unwrap();
        ex.run(async {
            assert!(crate::executor().reactor().sys.socket_ring_is_cooperative());
        });
        let ex = crate::LocalExecutorBuilder::new(crate::Placement::Unbound)
            .make()
            .unwrap();
        ex.run(async {
            assert!(!crate::executor().reactor().sys.socket_ring_is_cooperative());
            crate::timer::Timer::new(Duration::from_millis(5)).await;
        });
    }

    /// An arrival from another thread that the kernel has not posted yet is
    /// still taken by an inline reap: the reap enters once between passes
    /// to run the deferred work.
    #[test]
    fn an_inline_reap_runs_pending_task_work() {
        let ex = crate::LocalExecutorBuilder::new(crate::Placement::Unbound)
            .recv_buffers(crate::RecvBuffers::exact(8, 64))
            .preempt_timer(Duration::from_secs(10))
            .make()
            .unwrap();
        ex.run(async {
            use std::os::fd::AsRawFd as _;
            let (mut writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
            let reactor = crate::executor().reactor();
            let source = reactor.recv_multi(reader.as_raw_fd(), usize::MAX).unwrap();
            settle(&reactor);

            let peer = std::thread::spawn(move || {
                std::io::Write::write_all(&mut writer, b"from afar").unwrap();
                std::thread::sleep(Duration::from_millis(100));
            });
            let start = Instant::now();
            loop {
                reactor.reap_socket_completions().unwrap();
                if !arrivals(&source).0.is_empty() {
                    break;
                }
                assert!(
                    start.elapsed() < Duration::from_millis(200),
                    "the reap never ran the pending task work"
                );
                let mut woke = 0;
                reactor.sys.poll_io(&mut woke).unwrap();
            }
            assert_eq!(arrivals(&source).0.concat(), b"from afar");
            peer.join().unwrap();
        });
    }

    /// A task that never enters the kernel still sees the preempt timer,
    /// which is why the timer stays on the plain latency ring and not on the
    /// ring that defers its work.
    #[test]
    fn a_spinning_task_is_still_preempted_beside_a_deferred_ring() {
        let ex = crate::LocalExecutorBuilder::new(crate::Placement::Unbound)
            .preempt_timer(Duration::from_millis(2))
            .make()
            .unwrap();
        ex.run(async {
            crate::timer::Timer::new(Duration::from_millis(1)).await;
            let start = Instant::now();
            while !crate::executor().need_preempt() {
                assert!(
                    start.elapsed() < Duration::from_millis(500),
                    "no preemption in 500ms with a 2ms preempt timer"
                );
            }
            assert!(start.elapsed() < Duration::from_millis(100));
        });
    }

    /// A write larger than one buffer comes back whole over several, with
    /// every buffer accounted for. How many completions the kernel splits
    /// it into is its business: this kernel takes three full buffers in one
    /// and the 8-byte tail in another, so a bundle is an economy, not a
    /// promise.
    #[test]
    fn a_bundled_arrival_spans_buffers() {
        let ex = crate::LocalExecutorBuilder::new(crate::Placement::Unbound)
            .recv_buffers(crate::RecvBuffers::exact(8, 64))
            .make()
            .unwrap();
        ex.run(async {
            use std::os::fd::AsRawFd as _;
            let (mut writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
            let reactor = crate::executor().reactor();
            let source = reactor.recv_multi(reader.as_raw_fd(), usize::MAX).unwrap();
            settle(&reactor);
            let message: Vec<u8> = (0..200u8).collect();
            std::io::Write::write_all(&mut writer, &message).unwrap();
            settle(&reactor);

            let (ready, armed) = arrivals(&source);
            assert!(armed);
            assert_eq!(ready.concat(), message);
            let buffers = reactor.sys.recv_buffers().unwrap();
            assert_eq!(
                buffers.borrow().held(),
                4,
                "200 bytes take four 64-byte buffers"
            );
            drop(source);
            settle(&reactor);
            assert_eq!(
                buffers.borrow().held(),
                0,
                "the cancellation's completion did not return the queued buffers"
            );
        });
    }

    /// Every buffer handed back lands in its own slot with its own address.
    #[test]
    fn provided_buffers_are_distinct() {
        let buffers;
        let ring = IoUring::new(8).unwrap();
        buffers = ProvidedBuffers::register(&ring, 8, 1024).unwrap();
        let addresses: std::collections::HashSet<usize> =
            (0..8).map(|bid| buffers.address(bid)).collect();
        assert_eq!(addresses.len(), 8);
        let sorted = {
            let mut v: Vec<usize> = addresses.into_iter().collect();
            v.sort();
            v
        };
        assert!(sorted.windows(2).all(|w| w[1] - w[0] == 1024));
    }

    #[test]
    fn missing_opcodes_are_named_in_the_error() {
        let err = UringUnsupported::MissingOps(vec!["STATX", "CLOSE"]).to_string();
        assert!(err.contains("IORING_OP_STATX"), "{err}");
        assert!(err.contains("IORING_OP_CLOSE"), "{err}");
    }

    #[test]
    fn eperm_points_at_whatever_is_restricting_io_uring() {
        let err =
            UringUnsupported::SetupFailed(io::Error::from_raw_os_error(libc::EPERM)).to_string();
        let expected = match io_uring_disabled() {
            Some(1) => "kernel.io_uring_disabled=1",
            Some(2) => "kernel.io_uring_disabled=2",
            _ => "seccomp",
        };
        assert!(err.contains(expected), "{err}");
    }

    /// Dropping the in-flight `slow` timer mid-test cancels it.
    #[test]
    fn timeout_smoke_test() {
        let notifier = sys::new_sleep_notifier().unwrap();
        let pool = BlockingThreadPool::new(PoolPlacement::Unbound(1), notifier.clone()).unwrap();
        let reactor = Reactor::new(notifier, 0, 128, pool, crate::RecvBuffers::none()).unwrap();

        fn timeout_source(millis: u64) -> (Source, UringOpDescriptor) {
            let source = Source::new(
                IoRequirements::default(),
                -1,
                SourceType::Timeout(
                    TimeSpec64::try_from(Duration::from_millis(millis)).unwrap(),
                    0,
                ),
                None,
                None,
            );
            let op = match &*source.source_type() {
                SourceType::Timeout(ts, events) => {
                    UringOpDescriptor::Timeout(&ts.raw as *const _, *events)
                }
                _ => unreachable!(),
            };
            (source, op)
        }

        let (fast, op) = timeout_source(50);
        queue_request_into_ring(
            &mut *reactor.ring_for_source(&fast),
            &fast,
            op,
            &mut reactor.source_map.borrow_mut(),
        );

        let (slow, op) = timeout_source(150);
        queue_request_into_ring(
            &mut *reactor.ring_for_source(&slow),
            &slow,
            op,
            &mut reactor.source_map.borrow_mut(),
        );

        let (lethargic, op) = timeout_source(300);
        queue_request_into_ring(
            &mut *reactor.ring_for_source(&lethargic),
            &lethargic,
            op,
            &mut reactor.source_map.borrow_mut(),
        );

        let start = Instant::now();
        reactor.wait(|| None, None, 0, || 0).unwrap();
        let elapsed_ms = start.elapsed().as_millis();
        assert!((50..100).contains(&elapsed_ms));

        drop(slow);

        reactor.wait(|| None, None, 0, || 0).unwrap();
        let elapsed_ms = start.elapsed().as_millis();
        assert!((300..350).contains(&elapsed_ms));
    }

    #[test]
    fn allocator() {
        let l = Layout::from_size_align(10 << 20, 4 << 10).unwrap();
        let (data, mut allocator) = unsafe {
            let data = alloc::alloc::alloc(l);
            assert_eq!(data as usize & 4095, 0);
            let data = std::ptr::NonNull::new(data).unwrap();
            (
                data,
                BuddyAlloc::new(BuddyAllocParam::new(data.as_ptr(), l.size(), l.align())),
            )
        };
        let x = allocator.malloc(4096);
        assert_eq!(x as usize & 4095, 0);
        let x = allocator.malloc(1024);
        assert_eq!(x as usize & 4095, 0);
        let x = allocator.malloc(1);
        assert_eq!(x as usize & 4095, 0);
        unsafe { alloc::alloc::dealloc(data.as_ptr(), l) }
    }

    /// The allocator fails with a single page, because it needs extra metadata
    /// space
    #[test]
    fn allocator_exhaustion() {
        let al = Rc::new(UringBufferAllocator::new(8192));
        al.activate_registered_buffers(1234);
        let x = al.new_buffer(4096).unwrap();
        let y = al.new_buffer(4096).unwrap();

        if y.uring_buffer_id().is_some() {
            panic!("Expected non-uring buffer")
        }

        if y.uring_buffer_id().is_some() {
            unreachable!("Expected non-uring buffer")
        }
        drop(x);
        drop(y);

        let x = al.new_buffer(4096).unwrap();
        match x.uring_buffer_id() {
            Some(x) => assert_eq!(x, 1234),
            None => unreachable!("Expected uring buffer"),
        }
        drop(x);
        let x = al.new_buffer(40960).unwrap();
        if x.uring_buffer_id().is_some() {
            unreachable!("Expected non-uring buffer")
        }
    }

    /// A close still queued on any of the three rings is made when the
    /// reactor drops.
    #[test]
    fn dropped_reactor_closes_queued_descriptors_on_every_ring() {
        let dir = crate::test_utils::make_tmp_test_directory(
            "dropped_reactor_closes_queued_descriptors_on_every_ring",
        );
        let notifier = sys::new_sleep_notifier().unwrap();
        let pool = BlockingThreadPool::new(PoolPlacement::Unbound(1), notifier.clone()).unwrap();
        let mut reactor = Reactor::new(notifier, 0, 128, pool, crate::RecvBuffers::none()).unwrap();

        let queues = [
            ("main", reactor.main_ring.get_mut().submission_queue()),
            ("latency", reactor.latency_ring.get_mut().submission_queue()),
            ("poll", reactor.poll_ring.get_mut().submission_queue()),
        ];
        let paths: Vec<_> = queues
            .into_iter()
            .map(|(ring, queue)| {
                let path = dir.path.join(ring);
                let fd = std::os::fd::IntoRawFd::into_raw_fd(std::fs::File::create(&path).unwrap());
                queue.borrow_mut().submissions.push_back(UringDescriptor {
                    args: UringOpDescriptor::Close,
                    fd,
                    flags: squeue::Flags::empty(),
                    user_data: 0,
                });
                std::fs::canonicalize(path).unwrap()
            })
            .collect();
        drop(reactor);

        let open: Vec<_> = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|f| std::fs::canonicalize(f.unwrap().path()).ok())
            .filter(|f| paths.contains(f))
            .collect();
        assert!(open.is_empty(), "still open: {open:?}");
    }

    /// Enqueues three nops. The second is soft-linked to the third.
    #[test]
    fn sqe_link_chain() {
        let allocator = Rc::new(UringBufferAllocator::new(65536));
        let source_map = Rc::new(RefCell::new(SourceMap::default()));
        let mut ring = SleepableRing::new(4, "main", allocator, source_map, false).unwrap();
        let q = ring.submission_queue();
        let mut queue = q.borrow_mut();

        for i in 0..3 {
            queue.submissions.push_back(UringDescriptor {
                args: UringOpDescriptor::Nop,
                fd: -1,
                flags: if i == 1 {
                    squeue::Flags::IO_LINK
                } else {
                    squeue::Flags::empty()
                },
                user_data: 0,
            });
        }

        ring.submit_one_event(&mut queue.submissions);
        assert_eq!(1, ring.submit_sqes().unwrap());
        assert_eq!(1, ring.consume_completion_queue(&mut 0));

        ring.submit_one_event(&mut queue.submissions);
        assert_eq!(2, ring.submit_sqes().unwrap());
        assert_eq!(2, ring.consume_completion_queue(&mut 0));
    }

    #[test]
    #[should_panic(expected = "Unterminated SQE link chain")]
    /// If the link chain points outside of the queue, we panic
    fn unterminated_sqe_link_chain() {
        let allocator = Rc::new(UringBufferAllocator::new(65536));
        let source_map = Rc::new(RefCell::new(SourceMap::default()));
        let mut ring = SleepableRing::new(2, "main", allocator, source_map, false).unwrap();
        let q = ring.submission_queue();
        let mut queue = q.borrow_mut();

        queue.submissions.push_back(UringDescriptor {
            args: UringOpDescriptor::Close,
            fd: -1,
            flags: squeue::Flags::IO_LINK,
            user_data: 0,
        });

        ring.submit_one_event(&mut queue.submissions);
    }

    #[test]
    #[should_panic(expected = "Unterminated SQE link chain or submission queue overflow")]
    /// If the link chain is longer than the io_uring submission queue, we panic
    fn sqe_link_chain_overflow() {
        let allocator = Rc::new(UringBufferAllocator::new(65536));
        let source_map = Rc::new(RefCell::new(SourceMap::default()));
        let mut ring = SleepableRing::new(2, "main", allocator, source_map, false).unwrap();
        let q = ring.submission_queue();
        let mut queue = q.borrow_mut();

        for _ in 0..2 {
            queue.submissions.push_back(UringDescriptor {
                args: UringOpDescriptor::Close,
                fd: -1,
                flags: squeue::Flags::IO_LINK,
                user_data: 0,
            });
        }

        queue.submissions.push_back(UringDescriptor {
            args: UringOpDescriptor::Close,
            fd: -1,
            flags: squeue::Flags::empty(),
            user_data: 0,
        });

        ring.submit_one_event(&mut queue.submissions);
    }
}
