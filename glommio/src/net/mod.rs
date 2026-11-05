//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
//! This module provides glommio's networking support.
use crate::sys;
use nix::sys::socket::{self, MsgFlags, SockaddrLike};
use std::{
    io::{self, IoSlice},
    os::{
        fd::AsRawFd,
        unix::io::{BorrowedFd, RawFd},
    },
};

/// `None` when the call would block, so the caller falls back to the ring.
fn unless_would_block<T>(res: nix::Result<T>) -> Option<io::Result<T>> {
    match res {
        Err(nix::errno::Errno::EAGAIN) => None,
        res => Some(res.map_err(io::Error::from)),
    }
}

fn yolo_accept(fd: BorrowedFd<'_>) -> Option<io::Result<RawFd>> {
    debug_assert!(
        {
            let flags = nix::fcntl::fcntl(fd, nix::fcntl::F_GETFL).unwrap();
            nix::fcntl::OFlag::from_bits_truncate(flags).contains(nix::fcntl::OFlag::O_NONBLOCK)
        },
        "listener is blocking; accept would park the executor"
    );
    unless_would_block(socket::accept(fd.as_raw_fd()))
}

fn yolo_send(fd: RawFd, buf: &[u8]) -> Option<io::Result<usize>> {
    unless_would_block(socket::send(fd, buf, MsgFlags::MSG_DONTWAIT))
}

fn yolo_peek(fd: RawFd, buf: &mut [u8]) -> Option<io::Result<usize>> {
    unless_would_block(socket::recv(
        fd,
        buf,
        MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_PEEK,
    ))
}

fn yolo_recv(fd: RawFd, buf: &mut [u8]) -> Option<io::Result<usize>> {
    unless_would_block(socket::recv(fd, buf, MsgFlags::MSG_DONTWAIT))
}

fn yolo_recvmsg<T: SockaddrLike>(
    fd: RawFd,
    buf: &mut [u8],
    flags: MsgFlags,
) -> Option<io::Result<(usize, T)>> {
    match sys::recvmsg_syscall(
        fd,
        buf.as_mut_ptr(),
        buf.len(),
        (flags | MsgFlags::MSG_DONTWAIT).bits(),
    ) {
        Ok(x) => Some(Ok(x)),
        Err(err) => match err.kind() {
            io::ErrorKind::WouldBlock => None,
            _ => Some(Err(err)),
        },
    }
}

/// The vectored counterpart of [`yolo_send`].
///
/// Anything beyond `UIO_MAXIOV` slices is truncated, which the caller already
/// handles: a prefix write is a short write.
fn yolo_sendv(fd: RawFd, bufs: &[IoSlice<'_>]) -> Option<io::Result<usize>> {
    let len = std::cmp::min(bufs.len(), libc::UIO_MAXIOV as usize);
    unless_would_block(socket::sendmsg::<()>(
        fd,
        &bufs[..len],
        &[],
        MsgFlags::MSG_DONTWAIT,
        None,
    ))
}

fn yolo_sendmsg(fd: RawFd, buf: &[u8], addr: &impl SockaddrLike) -> Option<io::Result<usize>> {
    unless_would_block(socket::sendmsg(
        fd,
        &[IoSlice::new(buf)],
        &[],
        MsgFlags::MSG_DONTWAIT,
        Some(addr),
    ))
}

mod datagram;
mod stream;
mod tcp_socket;
mod udp_socket;
mod unix;
pub use self::{
    stream::{Buffered, Preallocated},
    tcp_socket::{AcceptedTcpStream, TcpListener, TcpStream},
    udp_socket::UdpSocket,
    unix::{AcceptedUnixStream, UnixDatagram, UnixListener, UnixStream},
};
