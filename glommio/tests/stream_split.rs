//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! `TcpStream::split` and `UnixStream::split` as a dependent crate drives them:
//! one task reads while another writes on the same connection, with no lock
//! between them.

use futures_lite::{
    future::zip,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
};
use glommio::{
    net::{TcpListener, TcpReadHalf, TcpStream, TcpWriteHalf, UnixStream},
    timer::sleep,
    LocalExecutor,
};
use std::{os::fd::AsRawFd, time::Duration};

/// A connected pair on loopback: the accepted side and the connecting side.
async fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (server, client) = zip(listener.accept(), TcpStream::connect(addr)).await;
    (server.unwrap(), client.unwrap())
}

#[test]
fn reads_and_writes_run_at_the_same_time() {
    LocalExecutor::default().run(async {
        let (server, client) = tcp_pair().await;
        let (mut client_rx, mut client_tx) = client.split();

        // The peer echoes, so the client's reader cannot make progress until
        // its writer has sent, and its writer never waits for the reader.
        let echo = glommio::spawn_local(async move {
            let (mut rx, mut tx) = server.split();
            let mut buf = [0u8; 64];
            loop {
                let n = rx.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                tx.write_all(&buf[..n]).await.unwrap();
            }
        });

        let writer = glommio::spawn_local(async move {
            for i in 0..100u8 {
                client_tx.write_all(&[i; 32]).await.unwrap();
            }
            client_tx.close().await.unwrap();
        });

        let mut received = Vec::new();
        client_rx.read_to_end(&mut received).await.unwrap();
        assert_eq!(received.len(), 100 * 32);
        for (i, chunk) in received.chunks(32).enumerate() {
            assert!(chunk.iter().all(|&b| b == i as u8));
        }

        writer.await;
        echo.await;
    });
}

#[test]
fn halves_share_one_descriptor() {
    LocalExecutor::default().run(async {
        let (server, _client) = tcp_pair().await;
        let fd = server.as_raw_fd();
        let (rx, tx) = server.split();
        assert_eq!(rx.as_raw_fd(), fd);
        assert_eq!(tx.as_raw_fd(), fd);
    });
}

#[test]
fn dropping_the_write_half_keeps_the_read_half_open() {
    LocalExecutor::default().run(async {
        let (server, mut client) = tcp_pair().await;
        let (mut rx, tx) = server.split();
        drop(tx);

        client.write_all(b"still here").await.unwrap();
        let mut buf = [0u8; 10];
        rx.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"still here");
    });
}

#[test]
fn dropping_both_halves_closes_the_connection() {
    LocalExecutor::default().run(async {
        let (server, mut client) = tcp_pair().await;
        let (rx, tx) = server.split();
        drop(rx);
        drop(tx);

        let mut buf = [0u8; 1];
        assert_eq!(client.read(&mut buf).await.unwrap(), 0);
    });
}

#[test]
fn reunite_gives_the_stream_back() {
    LocalExecutor::default().run(async {
        let (server, mut client) = tcp_pair().await;
        let peer = server.peer_addr().unwrap();
        let (rx, tx) = server.split();
        let mut server = TcpStream::reunite(rx, tx).unwrap();
        assert_eq!(server.peer_addr().unwrap(), peer);

        client.write_all(b"after").await.unwrap();
        let mut buf = [0u8; 5];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"after");
    });
}

#[test]
fn reunite_refuses_halves_of_different_streams() {
    LocalExecutor::default().run(async {
        let (a, b) = tcp_pair().await;
        let (a_rx, a_tx) = a.split();
        let (b_rx, b_tx) = b.split();
        let (a_rx, b_tx): (TcpReadHalf, TcpWriteHalf) = TcpStream::reunite(a_rx, b_tx).unwrap_err();
        TcpStream::reunite(a_rx, a_tx).unwrap();
        TcpStream::reunite(b_rx, b_tx).unwrap();
    });
}

#[test]
fn buffered_read_half_keeps_its_buffer() {
    LocalExecutor::default().run(async {
        let (server, mut client) = tcp_pair().await;
        let (mut rx, _tx) = server.buffered().split();

        client.write_all(b"line one\nline two\n").await.unwrap();
        let mut line = String::new();
        rx.read_line(&mut line).await.unwrap();
        assert_eq!(line, "line one\n");
        line.clear();
        rx.read_line(&mut line).await.unwrap();
        assert_eq!(line, "line two\n");
    });
}

#[test]
fn read_timeout_lives_on_the_read_half() {
    LocalExecutor::default().run(async {
        let (server, _client) = tcp_pair().await;
        let (mut rx, _tx) = server.split();
        rx.set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        assert_eq!(rx.read_timeout(), Some(Duration::from_millis(20)));

        let mut buf = [0u8; 1];
        let err = rx.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    });
}

#[test]
fn write_half_shutdown_reaches_the_peer() {
    LocalExecutor::default().run(async {
        let (server, mut client) = tcp_pair().await;
        let (_rx, tx) = server.split();
        tx.shutdown(std::net::Shutdown::Write).await.unwrap();

        let mut buf = [0u8; 1];
        assert_eq!(client.read(&mut buf).await.unwrap(), 0);
    });
}

#[test]
fn peek_on_the_read_half_does_not_consume() {
    LocalExecutor::default().run(async {
        let (server, mut client) = tcp_pair().await;
        let (mut rx, _tx) = server.split();
        client.write_all(b"peekaboo").await.unwrap();
        // Let the bytes land before peeking.
        sleep(Duration::from_millis(5)).await;

        let mut buf = [0u8; 4];
        assert_eq!(rx.peek(&mut buf).await.unwrap(), 4);
        assert_eq!(&buf, b"peek");
        let mut all = [0u8; 8];
        rx.read_exact(&mut all).await.unwrap();
        assert_eq!(&all, b"peekaboo");
    });
}

#[test]
fn unix_streams_split_the_same_way() {
    LocalExecutor::default().run(async {
        let (a, mut b) = UnixStream::pair().unwrap();
        let (mut rx, mut tx) = a.split();

        let pump = glommio::spawn_local(async move {
            tx.write_all(b"ping").await.unwrap();
            tx
        });
        let mut buf = [0u8; 4];
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        b.write_all(b"pong").await.unwrap();
        rx.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
        let tx = pump.await;

        let a = UnixStream::reunite(rx, tx).unwrap();
        drop(a);
    });
}
