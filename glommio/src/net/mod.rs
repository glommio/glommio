//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
//! This module provides glommio's networking support.
use crate::{reactor::Reactor, sys};
use nix::sys::socket::{self, MsgFlags, SockFlag, SockaddrLike};
use std::{
    cell::Cell,
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

/// How many accepts in a row a listener completes without the ring before it
/// goes through the ring once.
///
/// Measured under a Unix connection flood beside a task sleeping 1ms in a
/// loop, median of 10 interleaved rounds: 16 keeps 60% of the unbounded
/// accept rate, 32 keeps 90% and 64 keeps 93%, while the sleeper's 90th
/// percentile wait grows from 1.1ms to 1.2ms to 1.4ms. Accepting only
/// through the ring keeps 43%. Past 32 the rate barely moves and the wait
/// keeps growing.
const IMMEDIATE_ACCEPT_BATCH: u32 = 32;

/// The accepts a listener has completed without the ring since it last went
/// through it.
///
/// An accept without the ring never yields. A listener that always has a
/// connection queued, or fails every accept with `EMFILE`, would be ready on
/// every poll, and an accept loop would starve the tasks that could drain it
/// or free a descriptor. So every [`IMMEDIATE_ACCEPT_BATCH`] accepts, and
/// whenever a preemption is due, the caller takes the ring's accept, which
/// waits for a reactor pass.
#[derive(Debug, Default)]
struct ImmediateAccepts(Cell<u32>);

impl ImmediateAccepts {
    /// Accepts a connection that is already waiting, without the ring. Closed
    /// on exec, as the ring's accept asks for too. `None` when the caller
    /// must take the ring's accept instead.
    fn accept(&self, reactor: &Reactor, fd: BorrowedFd<'_>) -> Option<io::Result<RawFd>> {
        let taken = self.0.get();
        if taken >= IMMEDIATE_ACCEPT_BATCH || reactor.need_preempt() {
            self.0.set(0);
            return None;
        }
        debug_assert!(
            {
                let flags = nix::fcntl::fcntl(fd, nix::fcntl::F_GETFL).unwrap();
                nix::fcntl::OFlag::from_bits_truncate(flags).contains(nix::fcntl::OFlag::O_NONBLOCK)
            },
            "listener is blocking; accept would park the executor"
        );
        let res = unless_would_block(socket::accept4(fd.as_raw_fd(), SockFlag::SOCK_CLOEXEC));
        self.0.set(if res.is_some() { taken + 1 } else { 0 });
        res
    }
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
    use super::{TcpListener, UnixListener, UnixStream, IMMEDIATE_ACCEPT_BATCH};
    use crate::{
        test_utils::make_tmp_test_directory, timer::Timer, GlommioError, LocalExecutor,
        LocalExecutorBuilder, Placement,
    };
    use futures_lite::io::{AsyncWrite, AsyncWriteExt};
    use rusty_fork::rusty_fork_test;
    use std::{
        cell::Cell,
        future::Future,
        io::{self, IoSlice},
        rc::Rc,
        time::{Duration, Instant},
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

    /// An executor whose preempt timer never fires, so that only the bound
    /// on immediate accepts can make an accept loop yield.
    fn never_preempted() -> LocalExecutor {
        LocalExecutorBuilder::new(Placement::Unbound)
            .preempt_timer(Duration::MAX)
            .make()
            .unwrap()
    }

    fn set_descriptor_limit(limit: &libc::rlimit) {
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, limit) }, 0);
    }

    /// Lowers the descriptor limit to the lowest free descriptor, so that
    /// accepting a connection already queued fails with `EMFILE`, and has
    /// another task put the limit back 20ms later. `accept` is retried on
    /// every error, as an accept loop does, so it can only succeed once it
    /// has let that task run.
    async fn accept_once_a_descriptor_is_freed<T, F>(accept: impl Fn() -> F)
    where
        F: Future<Output = crate::Result<T>>,
    {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        let lowest_free = std::fs::File::open("/dev/null").unwrap();
        set_descriptor_limit(&libc::rlimit {
            rlim_cur: std::os::fd::AsRawFd::as_raw_fd(&lowest_free) as libc::rlim_t,
            ..limit
        });
        drop(lowest_free);

        let freed = Rc::new(Cell::new(false));
        let freer = crate::spawn_local({
            let freed = freed.clone();
            async move {
                Timer::new(Duration::from_millis(20)).await;
                set_descriptor_limit(&limit);
                freed.set(true);
            }
        })
        .detach();

        let start = Instant::now();
        let mut failures = 0u64;
        while start.elapsed() < Duration::from_secs(5) {
            match accept().await {
                Ok(_) => break,
                Err(GlommioError::IoError(err)) if err.raw_os_error() == Some(libc::EMFILE) => {
                    failures += 1;
                }
                Err(err) => panic!("{err}"),
            }
        }
        let elapsed = start.elapsed();
        set_descriptor_limit(&limit);
        drop(freer);
        assert!(failures > 0, "the accept never ran into the limit");
        assert!(
            freed.get(),
            "the accept loop failed {failures} times in {elapsed:?} and never let the other task run"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "the other task waited {elapsed:?} for a 20ms timer, over {failures} failures"
        );
    }

    /// Once a preemption is due, an accept goes through the ring although a
    /// connection is queued and the batch of immediate accepts is not used
    /// up.
    #[test]
    fn a_tcp_accept_with_a_preemption_due_goes_through_the_ring() {
        let ex = LocalExecutorBuilder::new(Placement::Unbound)
            .preempt_timer(Duration::from_millis(2))
            .make()
            .unwrap();
        ex.run(async {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let _client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            Timer::new(Duration::from_millis(1)).await;
            let start = Instant::now();
            while !crate::executor().need_preempt() {
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "no preemption in 5s with a 2ms preempt timer"
                );
            }
            assert!(
                futures_lite::future::poll_once(listener.accept())
                    .await
                    .is_none(),
                "the accept did not wait for the ring with a preemption due"
            );
            listener.accept().await.unwrap();
        });
    }

    /// An accept loop on a listener that never runs dry still lets other
    /// tasks run, within one batch of immediate accepts. The loop queues each
    /// connection itself before accepting it, so every accept finds one
    /// waiting. The other task yields once first: a spawned task is polled at
    /// once, and would otherwise be done before the loop starts.
    #[test]
    fn a_unix_accept_loop_that_never_runs_dry_lets_other_tasks_run() {
        never_preempted().run(async {
            let td = make_tmp_test_directory("uds-accept-flood");
            let path = td.path.join("listener");
            let listener = UnixListener::bind(&path).unwrap();
            let ran = Rc::new(Cell::new(false));
            let other = crate::spawn_local({
                let ran = ran.clone();
                async move {
                    futures_lite::future::yield_now().await;
                    ran.set(true);
                }
            })
            .detach();
            let start = Instant::now();
            let mut accepted = 0u64;
            while !ran.get() && start.elapsed() < Duration::from_secs(5) {
                let _client = std::os::unix::net::UnixStream::connect(&path).unwrap();
                drop(listener.accept().await.unwrap());
                accepted += 1;
            }
            drop(other);
            assert!(accepted > 0, "the other task ran before the loop");
            assert!(
                ran.get(),
                "{accepted} accepts in {:?} and the other task never ran",
                start.elapsed()
            );
            assert!(
                accepted <= u64::from(IMMEDIATE_ACCEPT_BATCH) + 1,
                "the other task waited for {accepted} accepts"
            );
        });
    }

    fn assert_a_connection_is_queued(listener: &UnixListener) {
        let mut pfd = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(listener),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut pfd, 1, 0) },
            1,
            "no connection is queued"
        );
    }

    /// A listener that always has a connection queued goes through the ring
    /// once after every full batch of immediate accepts, and then starts a
    /// new batch rather than staying on the ring.
    #[test]
    fn a_unix_listener_takes_the_ring_once_per_batch() {
        never_preempted().run(async {
            let td = make_tmp_test_directory("uds-accept-batches");
            let path = td.path.join("listener");
            let listener = UnixListener::bind(&path).unwrap();
            let mut through_the_ring = Vec::new();
            for i in 0..200 {
                let _client = std::os::unix::net::UnixStream::connect(&path).unwrap();
                assert_a_connection_is_queued(&listener);
                let accept = listener.accept();
                futures_lite::pin!(accept);
                match futures_lite::future::poll_once(&mut accept).await {
                    Some(stream) => drop(stream.unwrap()),
                    None => {
                        through_the_ring.push(i);
                        accept.await.unwrap();
                    }
                }
            }
            let batch = IMMEDIATE_ACCEPT_BATCH as usize;
            let expected: Vec<_> = (batch..200).step_by(batch + 1).collect();
            assert_eq!(through_the_ring, expected);
        });
    }

    /// An accept that finds nothing queued goes through the ring and starts
    /// a new batch, so a full batch of immediate accepts follows it.
    #[test]
    fn a_unix_accept_that_would_block_starts_a_new_batch() {
        never_preempted().run(async {
            let td = make_tmp_test_directory("uds-accept-would-block");
            let path = td.path.join("listener");
            let listener = UnixListener::bind(&path).unwrap();
            let accept_immediately = |i: u32| {
                let _client = std::os::unix::net::UnixStream::connect(&path).unwrap();
                assert_a_connection_is_queued(&listener);
                let accept = futures_lite::future::poll_once(listener.accept());
                async move {
                    assert!(
                        accept.await.is_some(),
                        "accept {i} of the batch went through the ring"
                    );
                }
            };
            for i in 0..IMMEDIATE_ACCEPT_BATCH - 1 {
                accept_immediately(i).await;
            }

            let accept = listener.accept();
            futures_lite::pin!(accept);
            assert!(
                futures_lite::future::poll_once(&mut accept).await.is_none(),
                "an accept with nothing queued completed"
            );
            let _client = std::os::unix::net::UnixStream::connect(&path).unwrap();
            accept.await.unwrap();

            for i in 0..IMMEDIATE_ACCEPT_BATCH {
                accept_immediately(i).await;
            }
        });
    }

    rusty_fork_test! {
        /// An accept loop that keeps failing with `EMFILE` still lets the
        /// task that would free a descriptor run, although each accept fails
        /// at once without the ring.
        #[test]
        fn a_tcp_accept_loop_out_of_descriptors_lets_other_tasks_run() {
            never_preempted().run(async {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let _client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
                accept_once_a_descriptor_is_freed(|| listener.accept()).await;
            });
        }

        /// The Unix listener accepts without the ring as well.
        #[test]
        fn a_unix_accept_loop_out_of_descriptors_lets_other_tasks_run() {
            never_preempted().run(async {
                let td = make_tmp_test_directory("uds-accept-emfile");
                let path = td.path.join("listener");
                let listener = UnixListener::bind(&path).unwrap();
                let _client = std::os::unix::net::UnixStream::connect(&path).unwrap();
                accept_once_a_descriptor_is_freed(|| listener.accept()).await;
            });
        }

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
