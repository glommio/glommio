//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! The TCP listener options as a dependent crate reaches them.
//!
//! `TcpListener::bind` sets `SO_REUSEPORT`, so any socket of the same user
//! can join its port. `TcpListenerBuilder` can turn that off, and since a
//! listener without `SO_REUSEPORT` needs `SO_REUSEADDR` to be restarted over
//! its own `TIME_WAIT` connections, it sets that too, as `std` does.

use futures_lite::io::AsyncWriteExt;
use glommio::{
    net::{TcpListener, TcpListenerBuilder},
    timer::sleep,
    GlommioError, LocalExecutor,
};
use socket2::SockRef;
use std::{
    io::{ErrorKind, Read},
    net::SocketAddr,
    process::Command,
    time::{Duration, Instant},
};

fn kind<T>(result: glommio::Result<T>) -> ErrorKind {
    match result {
        Ok(_) => panic!("the bind succeeded"),
        Err(GlommioError::IoError(err)) => err.kind(),
        Err(err) => panic!("not an I/O error: {err}"),
    }
}

/// The kernel caps a listen backlog at `net.core.somaxconn`.
fn somaxconn() -> u32 {
    std::fs::read_to_string("/proc/sys/net/core/somaxconn")
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// The accept queue limit the kernel holds for the listener on `port`, which
/// `ss` prints as the Send-Q of a listening socket.
fn listen_backlog(port: u16) -> u32 {
    let out = Command::new("ss")
        .args(["-H", "-l", "-t", "-n", &format!("sport = :{port}")])
        .output()
        .expect("ss from iproute2 is needed to read a listen backlog");
    assert!(out.status.success(), "ss failed: {out:?}");
    let out = String::from_utf8(out.stdout).unwrap();
    let mut lines = out.lines();
    let line = lines.next().expect("ss listed no listener");
    assert!(lines.next().is_none(), "ss listed several listeners: {out}");
    line.split_whitespace().nth(2).unwrap().parse().unwrap()
}

/// Whether a socket bound to local `port` on IPv4 is in `TIME_WAIT`.
fn time_wait_on(port: u16) -> bool {
    let table = std::fs::read_to_string("/proc/net/tcp").unwrap();
    table.lines().skip(1).any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let local_port = fields[1].rsplit(':').next().unwrap();
        u16::from_str_radix(local_port, 16).unwrap() == port && fields[3] == "06"
    })
}

#[test]
fn a_listener_without_reuse_port_keeps_its_port_to_itself() {
    LocalExecutor::default().run(async {
        let owner = TcpListenerBuilder::new()
            .reuse_port(false)
            .bind("127.0.0.1:0")
            .unwrap();
        assert!(!SockRef::from(&owner).reuse_port().unwrap());
        let addr = owner.local_addr().unwrap();

        assert_eq!(kind(TcpListener::bind(addr)), ErrorKind::AddrInUse);
        assert_eq!(
            kind(TcpListenerBuilder::new().reuse_port(false).bind(addr)),
            ErrorKind::AddrInUse
        );
    });
}

#[test]
fn default_listeners_share_a_port_with_reuse_address_set() {
    LocalExecutor::default().run(async {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = first.local_addr().unwrap();
        let second = TcpListener::bind(addr).unwrap();
        let third = TcpListenerBuilder::new().bind(addr).unwrap();

        for listener in [&first, &second, &third] {
            let sock = SockRef::from(listener);
            assert!(sock.reuse_port().unwrap());
            assert!(sock.reuse_address().unwrap());
            assert_eq!(listener.local_addr().unwrap(), addr);
        }
    });
}

#[test]
fn a_listener_without_reuse_port_rebinds_over_its_time_wait() {
    LocalExecutor::default().run(async {
        let exclusive = TcpListenerBuilder::new().reuse_port(false);
        let listener = exclusive.bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();

        let mut client = std::net::TcpStream::connect(addr).unwrap();
        let mut server = listener.accept().await.unwrap();
        server.close().await.unwrap();
        drop(server);
        assert_eq!(client.read(&mut [0u8; 8]).unwrap(), 0);
        drop(client);
        drop(listener);

        let deadline = Instant::now() + Duration::from_secs(5);
        while !time_wait_on(addr.port()) {
            assert!(
                Instant::now() < deadline,
                "the server side never reached TIME_WAIT"
            );
            sleep(Duration::from_millis(5)).await;
        }

        assert_eq!(
            kind(exclusive.reuse_address(false).bind(addr)),
            ErrorKind::AddrInUse,
            "TIME_WAIT did not hold the port, so this test shows nothing"
        );
        let again = exclusive.bind(addr).unwrap();
        assert_eq!(again.local_addr().unwrap(), addr);
    });
}

#[test]
fn the_backlog_reaches_listen() {
    LocalExecutor::default().run(async {
        let default = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = default.local_addr().unwrap().port();
        assert_eq!(listen_backlog(port), 1024.min(somaxconn()));

        let built = TcpListenerBuilder::new().bind("127.0.0.1:0").unwrap();
        let port = built.local_addr().unwrap().port();
        assert_eq!(listen_backlog(port), 1024.min(somaxconn()));

        let small = TcpListenerBuilder::new()
            .backlog(37)
            .bind("127.0.0.1:0")
            .unwrap();
        let port = small.local_addr().unwrap().port();
        assert_eq!(listen_backlog(port), 37);

        let huge = TcpListenerBuilder::new()
            .backlog(u32::MAX)
            .bind("127.0.0.1:0")
            .unwrap();
        let port = huge.local_addr().unwrap().port();
        assert_eq!(listen_backlog(port), somaxconn());
    });
}
