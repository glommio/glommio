//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
use super::datagram::GlommioDatagram;
use nix::sys::socket::{SockaddrLike, SockaddrStorage};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    cell::Cell,
    io,
    net::{self, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, ToSocketAddrs},
    os::{
        fd::{AsFd, BorrowedFd},
        unix::io::{AsRawFd, FromRawFd, RawFd},
    },
    time::Duration,
};

use crate::Result;

#[derive(Debug)]
/// An Udp Socket.
pub struct UdpSocket {
    socket: GlommioDatagram<net::UdpSocket>,
    /// Set once the socket is known to be connected, so `recv` and `peek`
    /// stop asking the kernel. Never set back: glommio cannot disconnect a
    /// socket, `connect` only takes an IP address, and a failed `connect`
    /// leaves the previous peer in place. An unconnected answer is not kept,
    /// since the socket may be connected later through its descriptor. A
    /// disconnect made through the descriptor (`connect` with `AF_UNSPEC`)
    /// is not seen: `recv` then takes datagrams from any sender.
    connected: Cell<bool>,
}

impl From<socket2::Socket> for UdpSocket {
    fn from(socket: socket2::Socket) -> UdpSocket {
        Self {
            socket: GlommioDatagram::<net::UdpSocket>::from(socket),
            connected: Cell::new(false),
        }
    }
}

impl AsRawFd for UdpSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.socket.as_raw_fd()
    }
}

impl AsFd for UdpSocket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.socket.as_fd()
    }
}

impl FromRawFd for UdpSocket {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        let socket = socket2::Socket::from_raw_fd(fd);
        UdpSocket::from(socket)
    }
}

fn sockaddr_storage_to_std(addr: SockaddrStorage) -> Option<SocketAddr> {
    match addr.family() {
        Some(nix::sys::socket::AddressFamily::Inet) => addr
            .as_sockaddr_in()
            .map(|x| SocketAddr::V4(SocketAddrV4::new(x.ip(), x.port()))),
        Some(nix::sys::socket::AddressFamily::Inet6) => addr.as_sockaddr_in6().map(|x| {
            SocketAddr::V6(SocketAddrV6::new(
                x.ip(),
                x.port(),
                x.flowinfo(),
                x.scope_id(),
            ))
        }),
        _ => None,
    }
}

/// Binds `sk` to `addr` with `SO_REUSEPORT` set, so other executors can bind
/// the same address, without letting a request for any port land on a taken
/// one.
///
/// The kernel's search for a free UDP port counts a port held by another
/// `SO_REUSEPORT` socket of the same user as free, when the searching socket
/// has the option set as well. Two sockets bound to port 0 can then share a
/// port, and a datagram meant for one is delivered to the other. So a bind to
/// port 0 sets the option only once the port is chosen.
fn bind_shared(sk: &Socket, addr: &socket2::SockAddr) -> io::Result<()> {
    if addr.as_socket().is_some_and(|addr| addr.port() == 0) {
        sk.bind(addr)?;
        sk.set_reuse_port(true)
    } else {
        sk.set_reuse_port(true)?;
        sk.bind(addr)
    }
}

impl UdpSocket {
    /// Creates a UDP socket bound to the specified address.
    ///
    /// Binding with port number 0 will request an available port from the OS.
    ///
    /// This sets the ReusePort option on the socket, so if the OS-provided load
    /// balancing is enough, it is possible to just bind to the same address
    /// from multiple executors.
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::{net::UdpSocket, LocalExecutor};
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async move {
    ///     let listener = UdpSocket::bind("127.0.0.1:8000").unwrap();
    ///     println!("Listening on {}", listener.local_addr().unwrap());
    /// });
    /// ```
    pub fn bind<A: ToSocketAddrs>(addr: A) -> Result<UdpSocket> {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::other("empty address"))?;

        let domain = if addr.is_ipv6() {
            Domain::IPV6
        } else {
            Domain::IPV4
        };
        let sk = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
        bind_shared(&sk, &socket2::SockAddr::from(addr))?;
        Ok(Self::from(sk))
    }

    /// Connects this UDP socket to a remote address, allowing the [`send`] and
    /// [`recv`] methods to be used to send data and also applies filters to
    /// only receive data from the specified address.
    ///
    /// If `addr` yields multiple addresses, connect will be attempted with each
    /// of the addresses until the underlying OS function returns no error.
    /// Note that usually, a successful connect call does not specify that
    /// there is a remote server listening on the port, rather, such an
    /// error would only be detected after the first send. If the OS returns an
    /// error for each of the specified addresses, the error returned from
    /// the last connection attempt (the last address) is returned.
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::{net::UdpSocket, LocalExecutor};
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async move {
    ///     let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
    ///     let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    ///     receiver
    ///         .connect(sender.local_addr().unwrap())
    ///         .await
    ///         .unwrap();
    ///     sender
    ///         .connect(receiver.local_addr().unwrap())
    ///         .await
    ///         .unwrap();
    /// });
    /// ```
    ///
    /// [`send`]: UdpSocket::send
    /// [`recv`]: UdpSocket::recv
    pub async fn connect<A: ToSocketAddrs>(&self, addr: A) -> Result<()> {
        let iter = addr.to_socket_addrs()?;
        let mut err = io::Error::other("No Valid addresses");
        for addr in iter {
            let reactor = self.socket.reactor.upgrade().unwrap();
            let source = reactor.connect(self.socket.as_raw_fd(), SockaddrStorage::from(addr));
            match source.collect_rw().await {
                Ok(_) => {
                    self.connected.set(true);
                    return Ok(());
                }
                Err(x) => {
                    err = x;
                }
            };
        }
        Err(err.into())
    }

    /// Stores a receive buffer size that nothing reads.
    ///
    /// Every receive is sized by the buffer handed to it: [`recv`],
    /// [`recv_from`], [`peek`] and [`peek_from`] read at most `buf.len()`
    /// bytes, whatever was set here. Size that buffer instead.
    ///
    /// [`recv`]: UdpSocket::recv
    /// [`recv_from`]: UdpSocket::recv_from
    /// [`peek`]: UdpSocket::peek
    /// [`peek_from`]: UdpSocket::peek_from
    #[deprecated(
        since = "0.12.1",
        note = "has no effect: receives are sized by the buffer passed to recv, recv_from, peek \
                and peek_from"
    )]
    pub fn set_buffer_size(&mut self, buffer_size: usize) {
        self.socket.rx_buf_size = buffer_size;
    }

    /// Returns the value last given to [`set_buffer_size`], which nothing on
    /// the receive path reads.
    ///
    /// [`set_buffer_size`]: UdpSocket::set_buffer_size
    #[deprecated(
        since = "0.12.1",
        note = "has no effect: receives are sized by the buffer passed to recv, recv_from, peek \
                and peek_from"
    )]
    pub fn buffer_size(&mut self) -> usize {
        self.socket.rx_buf_size
    }

    /// Gets the value of the `SO_BROADCAST` option for this socket.
    ///
    /// For more information about this option, see
    /// [`UdpSocket::set_broadcast`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use glommio::{net::UdpSocket, LocalExecutor};
    /// # let ex = LocalExecutor::default();
    /// # ex.run(async move {
    /// let s = UdpSocket::bind("127.0.0.1:10000").unwrap();
    /// s.set_broadcast(false).expect("set_broadcast call failed");
    /// assert_eq!(s.broadcast().unwrap(), false);
    /// # })
    /// ```
    pub fn broadcast(&self) -> Result<bool> {
        Ok(self.socket.socket.broadcast()?)
    }

    /// Sets the value of the `SO_BROADCAST` option for this socket.
    ///
    /// When enabled, this socket is allowed to send packets to a broadcast
    /// address.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use glommio::{net::UdpSocket, LocalExecutor};
    /// # let ex = LocalExecutor::default();
    /// # ex.run(async move {
    /// let s = UdpSocket::bind("127.0.0.1:10000").unwrap();
    /// s.set_broadcast(false).expect("set_broadcast call failed");
    /// # })
    /// ```
    pub fn set_broadcast(&self, broadcast: bool) -> Result<()> {
        Ok(self.socket.socket.set_broadcast(broadcast)?)
    }

    /// Executes an operation of the `IP_ADD_MEMBERSHIP` type.
    ///
    /// This function specifies a new multicast group for this socket to join.
    /// The address must be a valid multicast address, and `interface` is the
    /// address of the local interface with which the system should join the
    /// multicast group. If it's equal to `INADDR_ANY` then an appropriate
    /// interface is chosen by the system.
    pub fn join_multicast_v4(&self, multiaddr: &Ipv4Addr, interface: &Ipv4Addr) -> Result<()> {
        Ok(self.socket.socket.join_multicast_v4(multiaddr, interface)?)
    }

    /// Executes an operation of the `IPV6_ADD_MEMBERSHIP` type.
    ///
    /// This function specifies a new multicast group for this socket to join.
    /// The address must be a valid multicast address, and `interface` is the
    /// index of the interface to join/leave (or 0 to indicate any interface).
    pub fn join_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> Result<()> {
        Ok(self.socket.socket.join_multicast_v6(multiaddr, interface)?)
    }

    /// Executes an operation of the `IP_DROP_MEMBERSHIP` type.
    ///
    /// For more information about this option, see
    /// [`UdpSocket::join_multicast_v4`].
    pub fn leave_multicast_v4(&self, multiaddr: &Ipv4Addr, interface: &Ipv4Addr) -> Result<()> {
        Ok(self
            .socket
            .socket
            .leave_multicast_v4(multiaddr, interface)?)
    }

    /// Executes an operation of the `IPV6_DROP_MEMBERSHIP` type.
    ///
    /// For more information about this option, see
    /// [`UdpSocket::join_multicast_v6`].
    pub fn leave_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> Result<()> {
        Ok(self
            .socket
            .socket
            .leave_multicast_v6(multiaddr, interface)?)
    }

    /// Gets the value of the `IP_MULTICAST_LOOP` option for this socket.
    ///
    /// For more information about this option, see
    /// [`UdpSocket::set_multicast_loop_v4`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use glommio::{net::UdpSocket, LocalExecutor};
    /// # let ex = LocalExecutor::default();
    /// # ex.run(async move {
    /// let s = UdpSocket::bind("127.0.0.1:10000").unwrap();
    /// s.set_multicast_loop_v4(false)
    ///     .expect("set_multicast_loop_v4 call failed");
    /// assert_eq!(s.multicast_loop_v4().unwrap(), false);
    /// # })
    /// ```
    pub fn multicast_loop_v4(&self) -> Result<bool> {
        Ok(self.socket.socket.multicast_loop_v4()?)
    }

    /// Sets the value of the `IP_MULTICAST_LOOP` option for this socket.
    ///
    /// If enabled, multicast packets will be looped back to the local socket.
    /// Note that this may not have any effect on IPv6 sockets.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use glommio::{net::UdpSocket, LocalExecutor};
    /// # let ex = LocalExecutor::default();
    /// # ex.run(async move {
    /// let s = UdpSocket::bind("127.0.0.1:10000").unwrap();
    /// s.set_multicast_loop_v4(false)
    ///     .expect("set_multicast_loop_v4 call failed");
    /// # })
    /// ```
    pub fn set_multicast_loop_v4(&self, multicast_loop_v4: bool) -> Result<()> {
        Ok(self
            .socket
            .socket
            .set_multicast_loop_v4(multicast_loop_v4)?)
    }

    /// Gets the value of the `IPV6_MULTICAST_LOOP` option for this socket.
    ///
    /// For more information about this option, see
    /// [`UdpSocket::set_multicast_loop_v6`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use glommio::{net::UdpSocket, LocalExecutor};
    /// # let ex = LocalExecutor::default();
    /// # ex.run(async move {
    /// let s = UdpSocket::bind("127.0.0.1:10000").unwrap();
    /// s.set_multicast_loop_v6(false)
    ///     .expect("set_multicast_loop_v6 call failed");
    /// assert_eq!(s.multicast_loop_v6().unwrap(), false);
    /// # })
    /// ```
    pub fn multicast_loop_v6(&self) -> Result<bool> {
        Ok(self.socket.socket.multicast_loop_v6()?)
    }

    /// Sets the value of the `IPV6_MULTICAST_LOOP` option for this socket.
    ///
    /// Controls whether this socket sees the multicast packets it sends itself.
    /// Note that this may not have any affect on IPv4 sockets.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use glommio::{net::UdpSocket, LocalExecutor};
    /// # let ex = LocalExecutor::default();
    /// # ex.run(async move {
    /// let s = UdpSocket::bind("127.0.0.1:10000").unwrap();
    /// s.set_multicast_loop_v6(false)
    ///     .expect("set_multicast_loop_v6 call failed");
    /// # })
    /// ```
    pub fn set_multicast_loop_v6(&self, multicast_loop_v6: bool) -> Result<()> {
        Ok(self
            .socket
            .socket
            .set_multicast_loop_v6(multicast_loop_v6)?)
    }

    /// Gets the value of the `IP_MULTICAST_TTL` option for this socket.
    ///
    /// For more information about this option, see
    /// [`UdpSocket::set_multicast_ttl_v4`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use glommio::{net::UdpSocket, LocalExecutor};
    /// # let ex = LocalExecutor::default();
    /// # ex.run(async move {
    /// let s = UdpSocket::bind("127.0.0.1:10000").unwrap();
    /// s.set_multicast_ttl_v4(42)
    ///     .expect("set_multicast_ttl_v4 call failed");
    /// assert_eq!(s.multicast_ttl_v4().unwrap(), 42);
    /// # })
    /// ```
    pub fn multicast_ttl_v4(&self) -> Result<u32> {
        Ok(self.socket.socket.multicast_ttl_v4()?)
    }

    /// Sets the value of the `IP_MULTICAST_TTL` option for this socket.
    ///
    /// Indicates the time-to-live value of outgoing multicast packets for
    /// this socket. The default value is 1 which means that multicast packets
    /// don't leave the local network unless explicitly requested.
    ///
    /// Note that this may not have any effect on IPv6 sockets.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use glommio::{net::UdpSocket, LocalExecutor};
    /// # let ex = LocalExecutor::default();
    /// # ex.run(async move {
    /// let s = UdpSocket::bind("127.0.0.1:10000").unwrap();
    /// s.set_multicast_ttl_v4(42)
    ///     .expect("set_multicast_ttl_v4 call failed");
    /// # })
    /// ```
    pub fn set_multicast_ttl_v4(&self, multicast_ttl_v4: u32) -> Result<()> {
        Ok(self.socket.socket.set_multicast_ttl_v4(multicast_ttl_v4)?)
    }

    /// Gets the value of the `IP_TTL` option for this socket.
    ///
    /// For more information about this option, see [`UdpSocket::set_ttl`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use glommio::{net::UdpSocket, LocalExecutor};
    /// # let ex = LocalExecutor::default();
    /// # ex.run(async move {
    /// let s = UdpSocket::bind("127.0.0.1:10000").unwrap();
    /// s.set_ttl(42).expect("set_ttl call failed");
    /// assert_eq!(s.ttl().unwrap(), 42);
    /// # })
    /// ```
    pub fn ttl(&self) -> Result<u32> {
        Ok(self.socket.socket.ttl()?)
    }

    /// Sets the value for the `IP_TTL` option on this socket.
    ///
    /// This value sets the time-to-live field that is used in every packet sent
    /// from this socket.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use glommio::{net::UdpSocket, LocalExecutor};
    /// # let ex = LocalExecutor::default();
    /// # ex.run(async move {
    /// let s = UdpSocket::bind("127.0.0.1:10000").unwrap();
    /// s.set_ttl(42).expect("set_ttl call failed");
    /// # })
    /// ```
    pub fn set_ttl(&self, ttl: u32) -> Result<()> {
        Ok(self.socket.socket.set_ttl(ttl)?)
    }

    /// Sets the read timeout to the timeout specified.
    ///
    /// If the value specified is [`None`], then read calls will block
    /// indefinitely. An [`Err`] is returned if the zero [`Duration`] is
    /// passed to this method.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use glommio::{net::UdpSocket, LocalExecutor};
    /// # use std::time::Duration;
    /// # let ex = LocalExecutor::default();
    /// # ex.run(async move {
    /// let s = UdpSocket::bind("127.0.0.1:10000").unwrap();
    /// s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    /// # })
    /// ```
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> Result<()> {
        self.socket.set_read_timeout(dur)
    }

    /// Sets the write timeout to the timeout specified.
    ///
    /// If the value specified is [`None`], then write calls will block
    /// indefinitely. An [`Err`] is returned if the zero [`Duration`] is
    /// passed to this method.
    ///
    /// ```no_run
    /// # use glommio::{net::UdpSocket, LocalExecutor};
    /// # use std::time::Duration;
    /// # let ex = LocalExecutor::default();
    /// # ex.run(async move {
    /// let s = UdpSocket::bind("127.0.0.1:10000").unwrap();
    /// s.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
    /// # })
    /// ```
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> Result<()> {
        self.socket.set_write_timeout(dur)
    }

    /// Returns the read timeout of this socket.
    pub fn read_timeout(&self) -> Option<Duration> {
        self.socket.read_timeout()
    }

    /// Returns the write timeout of this socket.
    pub fn write_timeout(&self) -> Option<Duration> {
        self.socket.write_timeout()
    }

    /// Receives single datagram on the socket from the remote address to which
    /// it is connected, without removing the message from input queue. On
    /// success, returns the number of bytes peeked.
    ///
    /// The function must be called with valid byte array buf of sufficient size
    /// to hold the message bytes. If a message is too long to fit in the
    /// supplied buffer, excess bytes may be discarded.
    ///
    /// To use this function, [`connect`] must have been called
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::{net::UdpSocket, LocalExecutor};
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async move {
    ///     let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
    ///     let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    ///     receiver
    ///         .connect(sender.local_addr().unwrap())
    ///         .await
    ///         .unwrap();
    ///     sender
    ///         .send_to(&[1; 1], receiver.local_addr().unwrap())
    ///         .await
    ///         .unwrap();
    ///     let mut buf = vec![0; 32];
    ///     let sz = receiver.peek(&mut buf).await.unwrap();
    ///     assert_eq!(sz, 1);
    /// })
    /// ```
    ///
    /// [`connect`]: UdpSocket::connect
    pub async fn peek(&self, buf: &mut [u8]) -> Result<usize> {
        self.ensure_connected()?;
        self.socket.peek(buf).await.map_err(Into::into)
    }

    ///Receives a single datagram message on the socket, without removing it
    /// from the queue. On success, returns the number of bytes read and the
    /// origin.
    ///
    /// The function must be called with valid byte array buf of sufficient size
    /// to hold the message bytes. If a message is too long to fit in the
    /// supplied buffer, excess bytes may be discarded.
    pub async fn peek_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        let (sz, addr) = self.socket.peek_from(buf).await?;
        let addr = sockaddr_storage_to_std(addr).expect("invalid socket addr for this family!");
        Ok((sz, addr))
    }

    /// Fails with the error `getpeername` gives unless the socket is
    /// connected, asking the kernel only until it first says so.
    fn ensure_connected(&self) -> Result<()> {
        if !self.connected.get() {
            self.peer_addr()?;
            self.connected.set(true);
        }
        Ok(())
    }

    /// Returns the socket address of the remote peer this socket was connected
    /// to.
    pub fn peer_addr(&self) -> Result<SocketAddr> {
        #[cfg(test)]
        crate::sys::test_support::note_peer_lookup();
        self.socket.socket.peer_addr().map_err(Into::into)
    }

    /// Returns the socket address of the local half of this UDP connection.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.socket.socket.local_addr().map_err(Into::into)
    }

    /// Receives a single datagram message on the socket from the remote address
    /// to which it is connected.
    ///
    /// On success, returns the number of bytes read.  The function must be
    /// called with valid byte array buf of sufficient size to hold the
    /// message bytes. If a message is too long to fit in the supplied
    /// buffer, excess bytes may be discarded.
    ///
    ///
    /// To use this function, [`connect`] must have been called
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::{net::UdpSocket, LocalExecutor};
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async move {
    ///     let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
    ///     let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    ///     receiver
    ///         .connect(sender.local_addr().unwrap())
    ///         .await
    ///         .unwrap();
    ///     sender
    ///         .send_to(&[1; 1], receiver.local_addr().unwrap())
    ///         .await
    ///         .unwrap();
    ///     let mut buf = vec![0; 32];
    ///     let sz = receiver.recv(&mut buf).await.unwrap();
    ///     assert_eq!(sz, 1);
    /// })
    /// ```
    ///
    /// [`connect`]: UdpSocket::connect
    pub async fn recv(&self, buf: &mut [u8]) -> Result<usize> {
        self.ensure_connected()?;
        self.socket.recv(buf).await.map_err(Into::into)
    }

    /// Receives a single datagram message on the socket. On success, returns
    /// the number of bytes read and the origin.
    ///
    /// The function must be called with valid byte array buf of sufficient size
    /// to hold the message bytes. If a message is too long to fit in the
    /// supplied buffer, excess bytes may be discarded.
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::{net::UdpSocket, LocalExecutor};
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async move {
    ///     let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
    ///     let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    ///     sender
    ///         .send_to(&[1; 1], receiver.local_addr().unwrap())
    ///         .await
    ///         .unwrap();
    ///     let mut buf = vec![0; 32];
    ///     let (sz, addr) = receiver.recv_from(&mut buf).await.unwrap();
    ///     assert_eq!(sz, 1);
    ///     assert_eq!(addr, sender.local_addr().unwrap());
    /// })
    /// ```
    pub async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)> {
        let (sz, addr) = self.socket.recv_from(buf).await?;
        let addr = sockaddr_storage_to_std(addr).expect("invalid socket addr for this family!");
        Ok((sz, addr))
    }

    /// Sends data on the socket to the given address. On success, returns the
    /// number of bytes written. Address type can be any implementor of
    /// [`ToSocketAddrs`] trait. See its documentation for concrete examples.
    /// It is possible for `addr` to yield multiple addresses, but send_to will
    /// only send data to the first address yielded by `addr`.
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::{net::UdpSocket, LocalExecutor};
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async move {
    ///     let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
    ///     let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    ///     sender
    ///         .send_to(&[1; 1], receiver.local_addr().unwrap())
    ///         .await
    ///         .unwrap();
    ///     let mut buf = vec![0; 32];
    ///     let (sz, addr) = receiver.recv_from(&mut buf).await.unwrap();
    ///     assert_eq!(sz, 1);
    ///     assert_eq!(addr, sender.local_addr().unwrap());
    /// })
    /// ```
    ///
    /// [`ToSocketAddrs`]: https://doc.rust-lang.org/stable/std/net/trait.ToSocketAddrs.html
    pub async fn send_to<A: ToSocketAddrs>(&self, buf: &[u8], addr: A) -> Result<usize> {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::other("empty address"))?;

        let sockaddr = SockaddrStorage::from(addr);
        self.socket.send_to(buf, sockaddr).await.map_err(Into::into)
    }

    /// Sends data on the socket to the remote address to which it is connected.
    ///
    /// [`UdpSocket::connect`] will connect this socket to a remote address.
    /// This method will fail if the socket is not connected.
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::{net::UdpSocket, LocalExecutor};
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async move {
    ///     let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
    ///     let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    ///     sender
    ///         .connect(receiver.local_addr().unwrap())
    ///         .await
    ///         .unwrap();
    ///     sender.send(&[1; 1]).await.unwrap();
    /// })
    /// ```
    ///
    /// `[UdpSocket::connect`]: UdpSocket::connect
    pub async fn send(&self, buf: &[u8]) -> Result<usize> {
        self.socket.send(buf).await.map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{timer::Timer, LocalExecutorBuilder};
    use nix::sys::socket::{MsgFlags, SockaddrIn};
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::{Duration, Instant},
    };

    macro_rules! connected_pair {
        () => {{
            let s1 = UdpSocket::bind("127.0.0.1:0").unwrap();
            let s2 = UdpSocket::bind("127.0.0.1:0").unwrap();
            s1.connect(s2.local_addr().unwrap()).await.unwrap();
            s2.connect(s1.local_addr().unwrap()).await.unwrap();
            (s1, s2)
        }};
    }

    /// The holder takes a port, and the second socket is then confined to
    /// that one port (`IP_LOCAL_PORT_RANGE`, Linux 6.3), so the kernel's
    /// choice for port 0 is either the held port or nothing. Skips on a
    /// kernel without the option.
    #[test]
    fn binding_port_zero_never_shares_a_port_another_socket_holds() {
        test_executor!(async move {
            const IP_LOCAL_PORT_RANGE: libc::c_int = 51;
            let holder = UdpSocket::bind("127.0.0.1:0").unwrap();
            let port = u32::from(holder.local_addr().unwrap().port());
            let range = port | (port << 16);
            let sk = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
            let set = unsafe {
                libc::setsockopt(
                    sk.as_raw_fd(),
                    libc::IPPROTO_IP,
                    IP_LOCAL_PORT_RANGE,
                    &range as *const u32 as *const libc::c_void,
                    std::mem::size_of::<u32>() as libc::socklen_t,
                )
            };
            if set != 0 {
                assert_eq!(
                    io::Error::last_os_error().raw_os_error(),
                    Some(libc::ENOPROTOOPT)
                );
                return;
            }
            let any = socket2::SockAddr::from(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
            let err = bind_shared(&sk, &any).expect_err("bound a port another socket holds");
            assert_eq!(err.raw_os_error(), Some(libc::EADDRINUSE));
        });
    }

    /// A bind to port 0 still sets `SO_REUSEPORT`, only after the bind, so
    /// other executors can join the port it chose.
    #[test]
    fn executors_can_still_bind_the_port_a_port_zero_bind_chose() {
        test_executor!(async move {
            let first = UdpSocket::bind("127.0.0.1:0").unwrap();
            let second = UdpSocket::bind(first.local_addr().unwrap()).unwrap();
            assert_eq!(first.local_addr().unwrap(), second.local_addr().unwrap());
        });
    }

    #[test]
    fn send_to_through_the_ring_sends_the_whole_datagram() {
        test_executor!(async move {
            let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
            let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
            sender.socket.tx_yolo.set(false);
            let sent = sender
                .send_to(&[7u8; 100], receiver.local_addr().unwrap())
                .await
                .unwrap();
            assert_eq!(sent, 100);
            let mut buf = [0u8; 256];
            let (n, _) = receiver.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], &[7u8; 100]);
        });
    }

    #[should_panic]
    #[test]
    fn udp_unconnected_recv() {
        test_executor!(async move {
            let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut buf = [0u8; 1];
            receiver.recv(&mut buf).await.unwrap();
        });
    }

    #[should_panic]
    #[test]
    fn udp_unconnected_peek() {
        test_executor!(async move {
            let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut buf = [0u8; 1];
            receiver.peek(&mut buf).await.unwrap();
        });
    }

    #[should_panic]
    #[test]
    fn udp_unconnected_send() {
        test_executor!(async move {
            let conn = UdpSocket::bind("127.0.0.1:0").unwrap();
            conn.send(&[1]).await.unwrap();
        });
    }

    #[test]
    /// We can't rely on how the load balancing will happen, so we just send a
    /// bunch. There seems to be affinity, so every time we send we create a
    /// new source address.
    ///
    /// The sending waits until both executors have bound. An executor that
    /// binds after the last datagram has gone waits forever for its own, and
    /// a loaded machine can take longer than any fixed pause to start one.
    fn multi_executor_bind_works() {
        test_executor!(async move {
            let addr_picker = UdpSocket::bind("127.0.0.1:0").unwrap();
            let addr = addr_picker.local_addr().unwrap();
            let bound = Arc::new(AtomicUsize::new(0));

            let ex1_bound = bound.clone();
            let ex1 = LocalExecutorBuilder::default()
                .spawn(move || async move {
                    let socket = UdpSocket::bind(addr).unwrap();
                    ex1_bound.fetch_add(1, Ordering::Release);
                    let mut buf = [0u8; 1];
                    println!("will receive");
                    let (sz, _) = socket.recv_from(&mut buf).await.unwrap();
                    assert_eq!(sz, 1);
                    assert_eq!(buf[0], 65);
                    println!("received1");
                })
                .unwrap();

            let ex2_bound = bound.clone();
            let ex2 = LocalExecutorBuilder::default()
                .spawn(move || async move {
                    let socket = UdpSocket::bind(addr).unwrap();
                    ex2_bound.fetch_add(1, Ordering::Release);
                    let mut buf = [0u8; 1];
                    let (sz, _) = socket.recv_from(&mut buf).await.unwrap();
                    assert_eq!(sz, 1);
                    assert_eq!(buf[0], 65);
                })
                .unwrap();

            let deadline = Instant::now() + Duration::from_secs(30);
            while bound.load(Ordering::Acquire) < 2 {
                assert!(Instant::now() < deadline, "an executor never bound");
                Timer::new(Duration::from_millis(1)).await;
            }

            for _ in 0..1000 {
                let client = UdpSocket::bind("127.0.0.1:0").unwrap();
                client.send_to(&[65; 1], addr).await.unwrap();
            }

            ex1.join().unwrap();
            ex2.join().unwrap();
        });
    }

    #[test]
    fn udp_connect_peers() {
        test_executor!(async move {
            let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
            let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
            receiver
                .connect(sender.local_addr().unwrap())
                .await
                .unwrap();
            sender
                .connect(receiver.local_addr().unwrap())
                .await
                .unwrap();

            assert_eq!(receiver.peer_addr().unwrap(), sender.local_addr().unwrap());
            assert_eq!(sender.peer_addr().unwrap(), receiver.local_addr().unwrap());
        });
    }

    #[test]
    fn udp_connected_ping_pong() {
        test_executor!(async move {
            let (receiver, sender) = connected_pair!();
            sender.send(&[65u8; 1]).await.unwrap();

            let mut buf = [0u8; 1];
            assert_eq!(1, receiver.recv(&mut buf).await.unwrap());
            assert_eq!(buf[0], 65);

            receiver.send(&[64u8; 1]).await.unwrap();
            let mut buf = [0u8; 1];
            assert_eq!(1, sender.recv(&mut buf).await.unwrap());
            assert_eq!(buf[0], 64);
        });
    }

    /// Because we are connected, those messages will never arrive.
    #[test]
    fn udp_connected_recv_filter() {
        test_executor!(async move {
            let (receiver, sender) = connected_pair!();
            let other_sender = UdpSocket::bind("127.0.0.1:0").unwrap();
            for _ in 0..10 {
                sender.send(&[65u8; 1]).await.unwrap();
                other_sender
                    .send_to(&[64u8; 1], receiver.local_addr().unwrap())
                    .await
                    .unwrap();
            }

            for _ in 0..10 {
                let mut buf = [0u8; 1];
                assert_eq!(1, receiver.recv(&mut buf).await.unwrap());
                assert_eq!(buf[0], 65);
            }
        });
    }

    #[test]
    fn zero_sized_send() {
        test_executor!(async move {
            let (receiver, sender) = connected_pair!();

            let recv_handle = crate::spawn_local(async move {
                let mut buf = [0u8; 10];
                let sz = receiver.recv(&mut buf).await.unwrap();
                assert_eq!(sz, 0);
            })
            .detach();

            Timer::new(Duration::from_millis(100)).await;

            sender.send(&[]).await.unwrap();
            recv_handle.await.unwrap();
        });
    }

    #[test]
    fn peek() {
        test_executor!(async move {
            let (receiver, sender) = connected_pair!();

            let receiver_handle = crate::spawn_local(async move {
                for _ in 0..10 {
                    let mut buf = [0u8; 40];
                    let sz = receiver.peek(&mut buf).await.unwrap();
                    assert_eq!(sz, 4);
                    assert_eq!(&buf[0..4], b"msg1");
                }

                let mut buf = [0u8; 40];
                let sz = receiver.recv(&mut buf).await.unwrap();
                assert_eq!(sz, 4);
                assert_eq!(&buf[0..4], b"msg1");
            })
            .detach();

            sender.send(b"msg1").await.unwrap();

            receiver_handle.await.unwrap();
        });
    }

    #[test]
    fn a_connected_udp_peek_times_out_on_a_silent_peer() {
        test_executor!(async move {
            let (receiver, _silent) = connected_pair!();
            receiver
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let peek = crate::timer::timeout(Duration::from_secs(2), async {
                Ok(receiver.peek(&mut [0u8; 8]).await)
            })
            .await
            .expect("the peek timeout never fired");
            assert_eq!(
                io::Error::from(peek.unwrap_err()).kind(),
                io::ErrorKind::TimedOut
            );
        });
    }

    /// Peeks a queued datagram twice, through the direct syscall and then
    /// through the ring, and `recv` still finds it afterwards.
    #[test]
    fn a_udp_peek_leaves_a_queued_datagram_for_recv() {
        test_executor!(async move {
            let (receiver, sender) = connected_pair!();
            receiver
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            sender.send(b"msg1").await.unwrap();
            for yolo in [true, false] {
                receiver.socket.rx_yolo.set(yolo);
                let mut buf = [0u8; 8];
                assert_eq!(receiver.peek(&mut buf).await.unwrap(), 4);
                assert_eq!(&buf[..4], b"msg1");
            }
            let mut buf = [0u8; 8];
            assert_eq!(receiver.recv(&mut buf).await.unwrap(), 4);
            assert_eq!(&buf[..4], b"msg1");
        });
    }

    /// Sends first and then receive, so will hit the nonblocking path.
    #[test]
    fn peekfrom_non_blocking() {
        test_executor!(async move {
            let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
            let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
            let addr = receiver.local_addr().unwrap();

            sender.send_to(&[1], addr).await.unwrap();

            let mut buf = [0u8; 1];
            let (sz, from) = receiver.peek_from(&mut buf).await.unwrap();
            assert_eq!(sz, 1);
            assert_eq!(from, sender.local_addr().unwrap());
        });
    }

    /// Like the previous test, but recvs first so likely hits the io_uring
    /// path.
    #[test]
    fn peekfrom_blocking() {
        test_executor!(async move {
            let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
            let sender = UdpSocket::bind("127.0.0.1:0").unwrap();

            let addr = receiver.local_addr().unwrap();

            let receive_handle = crate::spawn_local(async move {
                let mut buf = [0u8; 1];
                for _ in 0..10 {
                    let (sz, _) = receiver
                        .socket
                        .recv_from_blocking::<SockaddrIn>(&mut buf, MsgFlags::MSG_PEEK)
                        .await
                        .unwrap();
                    assert_eq!(sz, 1);
                }
                let (_, from) = receiver
                    .socket
                    .recv_from_blocking(&mut buf, MsgFlags::MSG_PEEK)
                    .await
                    .unwrap();
                sockaddr_storage_to_std(from).expect("invalid socket addr for this family!")
            })
            .detach();

            Timer::new(Duration::from_millis(100)).await;
            let sender_addr = sender.local_addr().unwrap();
            sender.send_to(&[1], addr).await.unwrap();

            let from = receive_handle.await.unwrap();
            assert_eq!(from, sender_addr);
        });
    }

    /// Sends first and then receive, so will hit the nonblocking path.
    #[test]
    fn recvfrom_non_blocking() {
        test_executor!(async move {
            let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
            let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
            let addr = receiver.local_addr().unwrap();

            sender.send_to(&[1], addr).await.unwrap();

            let mut buf = [0u8; 1];
            let (sz, from) = receiver.recv_from(&mut buf).await.unwrap();
            assert_eq!(sz, 1);
            assert_eq!(from, sender.local_addr().unwrap());
        });
    }

    /// Like the previous test, but recvs first so likely hits the io_uring
    /// path.
    #[test]
    fn recvfrom_blocking() {
        test_executor!(async move {
            let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
            let sender = UdpSocket::bind("127.0.0.1:0").unwrap();

            let addr = receiver.local_addr().unwrap();

            let receive_handle = crate::spawn_local(async move {
                let mut buf = [0u8; 1];
                let (sz, from) = receiver
                    .socket
                    .recv_from_blocking(&mut buf, MsgFlags::empty())
                    .await
                    .unwrap();
                assert_eq!(sz, 1);
                sockaddr_storage_to_std(from).expect("invalid socket addr for this family!")
            })
            .detach();

            Timer::new(Duration::from_millis(100)).await;
            let sender_addr = sender.local_addr().unwrap();
            sender.send_to(&[1], addr).await.unwrap();

            let from = receive_handle.await.unwrap();
            assert_eq!(from, sender_addr);
        });
    }

    #[test]
    fn sendto() {
        test_executor!(async move {
            let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
            let me = UdpSocket::bind("127.0.0.1:0").unwrap();
            receiver.connect(me.local_addr().unwrap()).await.unwrap();

            let addr = receiver.local_addr().unwrap();
            me.send_to(&[65u8; 1], addr).await.unwrap();

            let mut buf = [0u8; 1];
            let sz = receiver.recv(&mut buf).await.unwrap();
            assert_eq!(sz, 1);
            assert_eq!(buf[0], 65u8);
        });
    }

    #[test]
    fn sendto_blocking() {
        test_executor!(async move {
            let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
            let addr = receiver.local_addr().unwrap();

            let sockaddr = SockaddrStorage::from(addr);
            let me = UdpSocket::bind("127.0.0.1:0").unwrap();
            me.socket
                .send_to_blocking(&[65u8; 1], sockaddr)
                .await
                .unwrap();

            receiver.connect(me.local_addr().unwrap()).await.unwrap();
            let mut buf = [0u8; 1];
            let sz = receiver.recv(&mut buf).await.unwrap();
            assert_eq!(sz, 1);
            assert_eq!(buf[0], 65u8);
        });
    }

    /// The receiving buffer is zeroed memory of which only the first page
    /// is ever written.
    #[test]
    fn a_peek_into_a_buffer_of_four_gib_sees_the_datagram() {
        test_executor!(async move {
            let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
            let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
            receiver
                .connect(sender.local_addr().unwrap())
                .await
                .unwrap();
            sender
                .send_to(b"hello", receiver.local_addr().unwrap())
                .await
                .unwrap();
            let mut buf = vec![0; 1 << 32];
            assert_eq!(receiver.peek(&mut buf).await.unwrap(), 5);
            assert_eq!(&buf[..5], b"hello");
        });
    }

    /// Guards against every `recv` and `peek` on a socket connected through
    /// `connect` asking the kernel for its peer again.
    #[test]
    fn recv_and_peek_on_a_connected_socket_do_not_look_up_the_peer() {
        test_executor!(async move {
            let (receiver, sender) = connected_pair!();
            let before = crate::sys::test_support::peer_lookups();
            for _ in 0..4 {
                sender.send(b"ping").await.unwrap();
                let mut buf = [0u8; 8];
                assert_eq!(receiver.peek(&mut buf).await.unwrap(), 4);
                assert_eq!(receiver.recv(&mut buf).await.unwrap(), 4);
            }
            assert_eq!(crate::sys::test_support::peer_lookups() - before, 0);
        });
    }

    /// A socket connected before glommio saw it is found connected by its
    /// first receive, and not asked again.
    #[test]
    fn a_socket_connected_outside_glommio_looks_up_its_peer_once() {
        test_executor!(async move {
            let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
            let std_receiver = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            std_receiver.connect(sender.local_addr().unwrap()).unwrap();
            let receiver = UdpSocket::from(Socket::from(std_receiver));
            let before = crate::sys::test_support::peer_lookups();
            for _ in 0..4 {
                sender
                    .send_to(b"ping", receiver.local_addr().unwrap())
                    .await
                    .unwrap();
                let mut buf = [0u8; 8];
                assert_eq!(receiver.peek(&mut buf).await.unwrap(), 4);
                assert_eq!(receiver.recv(&mut buf).await.unwrap(), 4);
            }
            assert_eq!(crate::sys::test_support::peer_lookups() - before, 1);
        });
    }

    /// An unconnected socket refuses `recv` and `peek` with `ENOTCONN` every
    /// time, and a refusal is not remembered: once the socket is connected,
    /// here behind glommio's back, `recv` works.
    #[test]
    fn an_unconnected_socket_keeps_refusing_until_it_is_connected() {
        test_executor!(async move {
            let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
            receiver
                .set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            let not_connected = |res: Result<usize>| {
                matches!(
                    res,
                    Err(crate::GlommioError::IoError(ref e))
                        if e.raw_os_error() == Some(libc::ENOTCONN)
                )
            };
            let mut buf = [0u8; 8];
            for _ in 0..2 {
                assert!(not_connected(receiver.recv(&mut buf).await));
                assert!(not_connected(receiver.peek(&mut buf).await));
            }
            let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
            let SocketAddr::V4(peer) = sender.local_addr().unwrap() else {
                unreachable!("bound to an IPv4 address")
            };
            nix::sys::socket::connect(receiver.as_raw_fd(), &SockaddrIn::from(peer)).unwrap();
            sender
                .send_to(b"ping", receiver.local_addr().unwrap())
                .await
                .unwrap();
            assert_eq!(receiver.recv(&mut buf).await.unwrap(), 4);
        });
    }

    #[test]
    fn broadcast() {
        test_executor!(async move {
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            s.set_broadcast(false).expect("set_broadcast call failed");
            assert!(!s.broadcast().unwrap());
        });
    }

    #[test]
    fn multicast_v4() {
        test_executor!(async move {
            let multicast_addr = Ipv4Addr::new(239, 0, 0, 0);
            assert!(multicast_addr.is_multicast());
            let interface_addr = Ipv4Addr::new(0, 0, 0, 0);
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            s.join_multicast_v4(&multicast_addr, &interface_addr)
                .unwrap();
            s.leave_multicast_v4(&multicast_addr, &interface_addr)
                .unwrap();
        });
    }

    #[test]
    fn multicast_v6() {
        test_executor!(async move {
            let multicast_addr = Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0);
            assert!(multicast_addr.is_multicast());
            let s = UdpSocket::bind("::1:0").unwrap();
            s.join_multicast_v6(&multicast_addr, 0).unwrap();
            s.leave_multicast_v6(&multicast_addr, 0).unwrap();
        });
    }

    #[test]
    fn set_multicast_loop_v4() {
        test_executor!(async move {
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            s.set_multicast_loop_v4(false)
                .expect("set_multicast_loop_v4 call failed");
            assert!(!s.multicast_loop_v4().unwrap());
        });
    }

    #[test]
    fn set_multicast_loop_v6() {
        test_executor!(async move {
            let s = UdpSocket::bind("::1:0").unwrap();
            s.set_multicast_loop_v6(false)
                .expect("set_multicast_loop_v6 call failed");
            assert!(!s.multicast_loop_v6().unwrap());
        });
    }

    #[test]
    fn set_multicast_ttl_v4() {
        test_executor!(async move {
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            s.set_multicast_ttl_v4(42)
                .expect("set_multicast_ttl_v4 call failed");
            assert_eq!(s.multicast_ttl_v4().unwrap(), 42);
        });
    }

    #[test]
    fn set_ttl() {
        test_executor!(async move {
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            s.set_ttl(42).expect("set_ttl call failed");
            assert_eq!(s.ttl().unwrap(), 42);
        });
    }
}
