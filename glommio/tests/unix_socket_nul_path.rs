//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! A Unix socket path holding an interior NUL, as a dependent crate sees it.
//!
//! The kernel ends a socket path at its first NUL, so a bind of `lis\0ten`
//! used to create `lis`. Every entry point that takes a socket path refuses
//! such a path with `InvalidInput`, as the filesystem operations do, and a
//! leading NUL still names a Linux abstract address.

use glommio::{
    net::{UnixDatagram, UnixListener, UnixStream},
    LocalExecutor,
};
use std::{
    io::{self, ErrorKind},
    os::linux::net::SocketAddrExt,
    path::PathBuf,
};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glommio-unixnul-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

fn kind<T>(result: glommio::Result<T, ()>) -> ErrorKind {
    match result {
        Ok(_) => panic!("a path with an interior NUL was accepted"),
        Err(err) => io::Error::from(err).kind(),
    }
}

#[test]
fn every_unix_socket_path_entry_point_refuses_an_interior_nul() {
    let dir = scratch("refused");
    LocalExecutor::default().run({
        let dir = dir.clone();
        async move {
            let stream_target = dir.join("lis");
            let dgram_target = dir.join("dg");

            assert_eq!(
                kind(UnixListener::bind(dir.join("lis\0ten"))),
                ErrorKind::InvalidInput,
                "UnixListener::bind"
            );
            assert_eq!(
                kind(UnixDatagram::bind(dir.join("dg\0ram"))),
                ErrorKind::InvalidInput,
                "UnixDatagram::bind"
            );
            assert!(
                !stream_target.exists(),
                "a listener bound the truncated path"
            );
            assert!(
                !dgram_target.exists(),
                "a datagram bound the truncated path"
            );

            let _listener = UnixListener::bind(&stream_target).unwrap();
            let _receiver = UnixDatagram::bind(&dgram_target).unwrap();
            let sender = UnixDatagram::unbound().unwrap();

            assert_eq!(
                kind(UnixStream::connect(dir.join("lis\0ten")).await),
                ErrorKind::InvalidInput,
                "UnixStream::connect"
            );
            assert_eq!(
                kind(sender.connect(dir.join("dg\0ram")).await),
                ErrorKind::InvalidInput,
                "UnixDatagram::connect"
            );
            assert_eq!(
                kind(sender.send_to(b"x", dir.join("dg\0ram")).await),
                ErrorKind::InvalidInput,
                "UnixDatagram::send_to"
            );
        }
    });
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_leading_nul_binds_an_abstract_address() {
    LocalExecutor::default().run(async {
        let name = format!("glommio-unixnul-abstract-{}", std::process::id());

        let listener = UnixListener::bind(format!("\0{name}-stream")).unwrap();
        assert_eq!(
            listener.local_addr().unwrap().as_abstract_name(),
            Some(format!("{name}-stream").as_bytes())
        );

        let socket = UnixDatagram::bind(format!("\0{name}-dgram")).unwrap();
        assert_eq!(
            socket.local_addr().unwrap().as_abstract_name(),
            Some(format!("{name}-dgram").as_bytes())
        );
    });
}
