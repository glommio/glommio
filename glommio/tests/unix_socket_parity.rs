//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! The Unix socket options as a dependent crate reaches them.
//!
//! The Unix stream and datagram types carry the same read and write timeouts
//! as their TCP and UDP twins, `UnixStream` can be shut down from a `poll`
//! function as `TcpStream` can, and an `AcceptedUnixStream` reports its peer
//! as an `AcceptedTcpStream` does.

use futures_lite::{
    future::poll_fn,
    io::{AsyncReadExt, AsyncWriteExt},
};
use glommio::{
    net::{UnixDatagram, UnixListener, UnixStream},
    timer::timeout,
    LocalExecutor,
};
use std::{io::ErrorKind, net::Shutdown, path::PathBuf, time::Duration};

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("glommio-unixparity-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

#[test]
fn a_unix_stream_read_times_out_on_a_silent_peer() {
    LocalExecutor::default().run(async {
        let (mut stream, _silent) = UnixStream::pair().unwrap();
        assert_eq!(stream.read_timeout(), None);
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        assert_eq!(stream.read_timeout(), Some(Duration::from_millis(50)));
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert_eq!(stream.write_timeout(), Some(Duration::from_secs(1)));
        assert!(stream.set_read_timeout(Some(Duration::ZERO)).is_err());

        let read = timeout(Duration::from_secs(5), async {
            Ok(stream.read(&mut [0u8; 8]).await)
        })
        .await
        .expect("the read timeout never fired");
        assert_eq!(read.unwrap_err().kind(), ErrorKind::TimedOut);
    });
}

#[test]
fn the_unix_halves_and_datagram_carry_timeouts() {
    LocalExecutor::default().run(async {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let (rx, tx) = stream.split();
        rx.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        tx.set_write_timeout(Some(Duration::from_millis(60)))
            .unwrap();
        assert_eq!(rx.read_timeout(), Some(Duration::from_millis(50)));
        assert_eq!(tx.write_timeout(), Some(Duration::from_millis(60)));

        let (socket, _silent) = UnixDatagram::pair().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert_eq!(socket.read_timeout(), Some(Duration::from_millis(50)));
        assert_eq!(socket.write_timeout(), Some(Duration::from_secs(1)));
        let recv = timeout(Duration::from_secs(5), async {
            Ok(socket.recv(&mut [0u8; 8]).await)
        })
        .await
        .expect("the recv timeout never fired");
        assert_eq!(
            std::io::Error::from(recv.unwrap_err()).kind(),
            ErrorKind::TimedOut
        );
    });
}

#[test]
fn a_unix_stream_shuts_down_from_a_poll_function() {
    LocalExecutor::default().run(async {
        let (mut stream, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        poll_fn(|cx| stream.poll_shutdown(cx, Shutdown::Write))
            .await
            .unwrap();
        assert_eq!(peer.read(&mut [0u8; 8]).await.unwrap(), 0);
        peer.write_all(b"still open").await.unwrap();
        let mut buf = [0u8; 10];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"still open");
    });
}

#[test]
fn an_accepted_unix_stream_reports_its_peer() {
    let dir = scratch("accepted");
    let cleanup = dir.clone();
    LocalExecutor::default().run(async move {
        let path = dir.join("listener");
        let listener = UnixListener::bind(&path).unwrap();
        let client_path = dir.join("client");
        let client =
            socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None).unwrap();
        client
            .bind(&socket2::SockAddr::unix(&client_path).unwrap())
            .unwrap();
        client
            .connect(&socket2::SockAddr::unix(&path).unwrap())
            .unwrap();

        let accepted = listener.shared_accept().await.unwrap();
        assert_eq!(
            accepted.peer_addr().unwrap().as_pathname(),
            Some(client_path.as_path())
        );
        let stream = accepted.bind_to_executor();
        assert_eq!(
            stream.peer_addr().unwrap().as_pathname(),
            Some(client_path.as_path())
        );
    });
    let _ = std::fs::remove_dir_all(&cleanup);
}
