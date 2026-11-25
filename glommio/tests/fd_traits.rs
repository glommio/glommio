//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! The descriptor traits as a dependent crate reaches them.
//!
//! Every socket and file type hands out its descriptor through both `AsRawFd`
//! and `AsFd`, so it can go to `socket2::SockRef` or any other API that takes
//! a borrowed descriptor.

use futures_lite::future::zip;
use glommio::{
    io::{BufferedFile, DmaFile, OwnedDmaFile},
    net::{TcpListener, TcpStream, UdpSocket, UnixDatagram, UnixListener, UnixStream},
    LocalExecutor,
};
use std::{
    os::fd::{AsFd, AsRawFd, RawFd},
    path::PathBuf,
};

/// The descriptor `value` hands out, after checking both traits agree on it.
fn fd_of<T: AsFd + AsRawFd>(value: &T) -> RawFd {
    let raw = value.as_raw_fd();
    assert_eq!(value.as_fd().as_raw_fd(), raw);
    assert!(raw >= 0);
    raw
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("glommio-fdtraits-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

#[test]
fn every_socket_type_lends_its_descriptor() {
    let dir = scratch("sockets");
    let cleanup = dir.clone();
    LocalExecutor::default().run(async move {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        fd_of(&listener);
        let sock = socket2::SockRef::from(&listener);
        assert_eq!(
            sock.local_addr().unwrap().as_socket(),
            Some(listener.local_addr().unwrap()),
            "the listener's descriptor is not the bound socket"
        );
        let addr = listener.local_addr().unwrap();
        let (server, client) = zip(listener.accept(), TcpStream::connect(addr)).await;
        let (server, client) = (server.unwrap(), client.unwrap());
        fd_of(&server);
        let (rx, tx) = client.split();
        assert_eq!(fd_of(&rx), fd_of(&tx));

        fd_of(&UdpSocket::bind("127.0.0.1:0").unwrap());

        let path = dir.join("listener");
        let listener = UnixListener::bind(&path).unwrap();
        fd_of(&listener);
        let (server, client) = zip(listener.accept(), UnixStream::connect(&path)).await;
        let (server, client) = (server.unwrap(), client.unwrap());
        fd_of(&server);
        let (rx, tx) = client.split();
        assert_eq!(fd_of(&rx), fd_of(&tx));

        let (a, _b) = UnixDatagram::pair().unwrap();
        fd_of(&a);
    });
    let _ = std::fs::remove_dir_all(&cleanup);
}

#[test]
fn every_file_type_lends_its_descriptor() {
    let dir = scratch("files");
    let cleanup = dir.clone();
    LocalExecutor::default().run(async move {
        let buffered = BufferedFile::create(dir.join("buffered")).await.unwrap();
        fd_of(&buffered);
        buffered.close().await.unwrap();

        let dma = DmaFile::create(dir.join("dma")).await.unwrap();
        let raw = fd_of(&dma);
        let owned = OwnedDmaFile::from(dma);
        assert_eq!(fd_of(&owned), raw);
        DmaFile::from(owned).close().await.unwrap();
    });
    let _ = std::fs::remove_dir_all(&cleanup);
}
