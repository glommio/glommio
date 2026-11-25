//! `try_recv` on both channel receivers, driven from a dependent crate.

use glommio::{
    channels::{local_channel, shared_channel, TryRecvError},
    LocalExecutor, LocalExecutorBuilder,
};
use std::sync::mpsc;

#[test]
fn a_local_receiver_tries_without_waiting() {
    LocalExecutor::default().run(async {
        let (sender, receiver) = local_channel::new_bounded(1);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        sender.try_send(3).unwrap();
        drop(sender);
        assert_eq!(receiver.try_recv(), Ok(3));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Closed));
    });
}

#[test]
fn a_connected_receiver_tries_without_waiting() {
    let (sender, receiver) = shared_channel::new_bounded(1);
    let (checked_tx, checked_rx) = mpsc::channel::<()>();
    let (sent_tx, sent_rx) = mpsc::channel::<()>();
    let producer = LocalExecutorBuilder::default()
        .spawn(move || async move {
            let sender = sender.connect().await;
            checked_rx.recv().unwrap();
            sender.try_send(3u32).unwrap();
            drop(sender);
            sent_tx.send(()).unwrap();
        })
        .unwrap();
    let consumer = LocalExecutorBuilder::default()
        .spawn(move || async move {
            let receiver = receiver.connect().await;
            assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
            checked_tx.send(()).unwrap();
            sent_rx.recv().unwrap();
            assert_eq!(receiver.try_recv(), Ok(3));
            assert_eq!(receiver.try_recv(), Err(TryRecvError::Closed));
        })
        .unwrap();
    producer.join().unwrap();
    consumer.join().unwrap();
}

#[test]
fn a_try_recv_error_is_a_copyable_std_error() {
    fn boxed(e: TryRecvError) -> Box<dyn std::error::Error + Send + Sync> {
        Box::new(e)
    }
    let empty = TryRecvError::Empty;
    let copy = empty;
    assert_eq!(empty, copy);
    assert_ne!(TryRecvError::Empty, TryRecvError::Closed);
    assert_eq!(boxed(empty).to_string(), empty.to_string());
    assert!(!format!("{:?}", TryRecvError::Closed).is_empty());
}
