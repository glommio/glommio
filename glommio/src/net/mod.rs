//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
//! This module provides glommio's networking support.
use crate::sys;
use nix::sys::socket::{self, MsgFlags, SockFlag, SockaddrLike};
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

/// Accepts a connection that is already waiting, without the ring. Closed on
/// exec, as the ring's accept asks for too.
fn yolo_accept(fd: BorrowedFd<'_>) -> Option<io::Result<RawFd>> {
    debug_assert!(
        {
            let flags = nix::fcntl::fcntl(fd, nix::fcntl::F_GETFL).unwrap();
            nix::fcntl::OFlag::from_bits_truncate(flags).contains(nix::fcntl::OFlag::O_NONBLOCK)
        },
        "listener is blocking; accept would park the executor"
    );
    unless_would_block(socket::accept4(fd.as_raw_fd(), SockFlag::SOCK_CLOEXEC))
}

/// The flags every send on a socket passes. `MSG_NOSIGNAL` turns a write to
/// a closed peer into `EPIPE`; without it the kernel also raises `SIGPIPE`,
/// which kills a process that leaves the signal at its default.
pub(crate) const SEND_FLAGS: MsgFlags = MsgFlags::MSG_NOSIGNAL;

fn yolo_send(fd: RawFd, buf: &[u8]) -> Option<io::Result<usize>> {
    unless_would_block(socket::send(fd, buf, SEND_FLAGS | MsgFlags::MSG_DONTWAIT))
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
        SEND_FLAGS | MsgFlags::MSG_DONTWAIT,
        None,
    ))
}

fn yolo_sendmsg(fd: RawFd, buf: &[u8], addr: &impl SockaddrLike) -> Option<io::Result<usize>> {
    unless_would_block(socket::sendmsg(
        fd,
        &[IoSlice::new(buf)],
        &[],
        SEND_FLAGS | MsgFlags::MSG_DONTWAIT,
        Some(addr),
    ))
}

#[cfg(test)]
mod completion_read_tests;
mod datagram;
#[cfg(test)]
mod multishot_tests;
mod stream;
mod tcp_socket;
mod udp_socket;
mod unix;
pub use self::{
    stream::{Buffered, NonBuffered, OwnedRxBuf, Preallocated, RxBuf},
    tcp_socket::{
        AcceptedTcpStream, TcpListener, TcpListenerBuilder, TcpReadHalf, TcpStream, TcpWriteHalf,
    },
    udp_socket::UdpSocket,
    unix::{
        AcceptedUnixStream, UnixDatagram, UnixListener, UnixReadHalf, UnixStream, UnixWriteHalf,
    },
};

#[cfg(test)]
mod test {
    use super::{TcpListener, UnixStream};
    use crate::timer::Timer;
    use futures_lite::io::{AsyncWrite, AsyncWriteExt};
    use rusty_fork::rusty_fork_test;
    use std::{
        io::{self, IoSlice},
        time::Duration,
    };

    /// Puts `SIGPIPE` back to the default that kills the process, as a host
    /// that is not a Rust binary can leave it.
    fn restore_default_sigpipe() {
        assert_ne!(
            unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) },
            libc::SIG_ERR
        );
    }

    /// Writes to a stream whose peer is gone until the write reports the
    /// broken pipe. A reset may come first, and a write may still be
    /// accepted before the peer's close arrives.
    async fn write_until_broken(stream: &mut (impl AsyncWrite + Unpin), vectored: bool) {
        for _ in 0..100 {
            let res = if vectored {
                stream.write_vectored(&[IoSlice::new(b"x")]).await
            } else {
                stream.write(b"x").await
            };
            match res {
                Ok(_) => {
                    Timer::new(Duration::from_millis(1)).await;
                }
                Err(err) if err.kind() == io::ErrorKind::ConnectionReset => {}
                Err(err) => {
                    assert_eq!(err.kind(), io::ErrorKind::BrokenPipe, "{err}");
                    return;
                }
            }
        }
        panic!("the closed peer was never reported");
    }

    async fn tcp_stream_with_a_closed_peer() -> super::TcpStream {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let stream = listener.accept().await.unwrap();
        drop(peer);
        stream
    }

    rusty_fork_test! {
        /// A write to a TCP peer that has gone returns `EPIPE` and leaves
        /// the process alive. Forked, since the signal disposition is per
        /// process.
        #[test]
        fn a_write_to_a_closed_tcp_peer_is_an_error_not_a_signal() {
            restore_default_sigpipe();
            test_executor!(async move {
                let mut stream = tcp_stream_with_a_closed_peer().await;
                write_until_broken(&mut stream, false).await;
            });
        }

        /// The vectored write takes its own syscall, and must not raise
        /// `SIGPIPE` either.
        #[test]
        fn a_vectored_write_to_a_closed_tcp_peer_is_an_error_not_a_signal() {
            restore_default_sigpipe();
            test_executor!(async move {
                let mut stream = tcp_stream_with_a_closed_peer().await;
                write_until_broken(&mut stream, true).await;
            });
        }

        /// A Unix stream reports a closed peer on the first write, with
        /// `SIGPIPE` unless the send says otherwise.
        #[test]
        fn a_write_to_a_closed_unix_peer_is_an_error_not_a_signal() {
            restore_default_sigpipe();
            test_executor!(async move {
                let (mut stream, peer) = UnixStream::pair().unwrap();
                drop(peer);
                write_until_broken(&mut stream, false).await;
            });
        }
    }
}
