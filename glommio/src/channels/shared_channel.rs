//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
//!
use crate::{
    channels::spsc_queue::{make, BufferHalf, Consumer, Producer},
    enclose,
    reactor::Reactor,
    sys::{self, SleepNotifier},
    GlommioError, ResourceType,
};
use futures_lite::{future, stream::Stream};
use scopeguard::ScopeGuard;
use std::{
    fmt,
    future::Future,
    pin::Pin,
    rc::{Rc, Weak},
    sync::Arc,
    task::{Context, Poll},
};

type Result<T, V> = crate::Result<T, V>;

/// The `SharedReceiver` is the receiving end of the Shared Channel.
/// It implements [`Send`] so it can be passed to any thread. However,
/// it doesn't implement any method: before it is used it must be changed
/// into a [`ConnectedReceiver`], which then makes sure it will be used by
/// at most one thread.
///
/// It is technically possible to share this among multiple threads inside
/// a lock, although such design is discouraged and beats the purpose of a
/// spsc channel.
///
/// [`ConnectedReceiver`]: struct.ConnectedReceiver.html
/// [`Send`]: https://doc.rust-lang.org/std/marker/trait.Send.html
pub struct SharedReceiver<T: Send + Sized> {
    state: Option<Arc<ReceiverState<T>>>,
}

/// The `SharedSender` is the sending end of the Shared Channel.
/// It implements [`Send`] so it can be passed to any thread. However,
/// it doesn't implement any method: before it is used it must be changed
/// into a [`ConnectedSender`], which then makes sure it will be used by
/// at most one thread.
///
/// It is technically possible to share this among multiple threads inside
/// a lock, although such design is discouraged and beats the purpose of a
/// spsc channel.
///
/// [`ConnectedSender`]: struct.ConnectedSender.html
/// [`Send`]: https://doc.rust-lang.org/std/marker/trait.Send.html
pub struct SharedSender<T: Send + Sized> {
    state: Option<Arc<SenderState<T>>>,
}

impl<T: Send + Sized> fmt::Debug for SharedSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.state {
            Some(s) => write!(f, "Unbound SharedSender {:?}", s.buffer),
            None => write!(f, "Bound SharedSender"),
        }
    }
}

impl<T: Send + Sized> fmt::Debug for SharedReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.state {
            Some(s) => write!(f, "Unbound SharedReceiver: {:?}", s.buffer),
            None => write!(f, "Bound SharedReceiver"),
        }
    }
}

/// The `ConnectedReceiver` is the receiving end of the Shared Channel.
pub struct ConnectedReceiver<T: Send + Sized> {
    id: u64,
    state: Arc<ReceiverState<T>>,
    reactor: Weak<Reactor>,
    notifier: Arc<SleepNotifier>,
}

/// The `ConnectedSender` is the sending end of the Shared Channel.
pub struct ConnectedSender<T: Send + Sized> {
    id: u64,
    state: Arc<SenderState<T>>,
    reactor: Weak<Reactor>,
    notifier: Arc<SleepNotifier>,
}

impl<T: Send + Sized> fmt::Debug for ConnectedReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Connected Receiver {}: {:?}", self.id, self.state.buffer)
    }
}

impl<T: Send + Sized> fmt::Debug for ConnectedSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Connected Sender {} : {:?}", self.id, self.state.buffer)
    }
}

#[derive(Debug)]
struct SenderState<V: Send + Sized> {
    buffer: Producer<V>,
}

#[derive(Debug)]
struct ReceiverState<V: Send + Sized> {
    buffer: Consumer<V>,
}

/// Waits for the peer half to connect, and returns its executor's notifier.
///
/// The half that connects second wakes the one waiting: it has published
/// its id by then, so a notify on the waiting peer's executor finds the id
/// when the reactor next checks `peer_connected`.
struct Connector<T: BufferHalf> {
    id: u64,
    buffer: T,
    peer_connected: Rc<dyn Fn() -> bool>,
    reactor: Weak<Reactor>,
}

impl<T: BufferHalf> Connector<T> {
    fn new(
        id: u64,
        buffer: T,
        peer_connected: Rc<dyn Fn() -> bool>,
        reactor: Weak<Reactor>,
    ) -> Self {
        Self {
            id,
            buffer,
            peer_connected,
            reactor,
        }
    }
}

impl<T: BufferHalf> Future for Connector<T> {
    type Output = Arc<SleepNotifier>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let reactor = self.reactor.upgrade().unwrap();

        match self.buffer.peer_id() {
            0 => {
                reactor.wait_for_shared_channel_peer(self.id, &self.peer_connected, cx.waker());
                Poll::Pending
            }
            // usize::MAX (the disconnected) always has a placeholder notifier that never
            // returns its fd. So if the other side disconnected it will unblock us here
            id => Poll::Ready(sys::get_sleep_notifier_for(id).unwrap_or_else(|| {
                // The id outlives the notifier: the peer's executor drops it on
                // its own schedule, while the id stays until the peer half is.
                // An executor that is gone cannot be woken, so it is disconnected.
                self.buffer.disconnect_peer();
                sys::get_sleep_notifier_for(usize::MAX)
                    .expect("the disconnected notifier is a static and always exists")
            })),
        }
        .map(|notifier| {
            reactor.stop_waiting_for_shared_channel_peer(self.id);
            notifier.notify(false);
            notifier
        })
    }
}

/// Unregisters a half when dropped, so a `connect` cancelled while waiting
/// for its peer does not leave the registration behind.
fn unregister_on_drop(
    reactor: Weak<Reactor>,
    id: u64,
) -> ScopeGuard<Weak<Reactor>, impl FnOnce(Weak<Reactor>)> {
    scopeguard::guard(reactor, move |reactor| {
        if let Some(r) = reactor.upgrade() {
            r.unregister_shared_channel(id);
        }
    })
}

/// Creates a new `shared_channel` returning its sender and receiver
/// endpoints.
///
/// All shared channels must be bounded.
pub fn new_bounded<T: Send + Sized>(size: usize) -> (SharedSender<T>, SharedReceiver<T>) {
    let (producer, consumer) = make(size);
    (
        SharedSender {
            state: Some(Arc::new(SenderState { buffer: producer })),
        },
        SharedReceiver {
            state: Some(Arc::new(ReceiverState { buffer: consumer })),
        },
    )
}

impl<T: 'static + Send + Sized> SharedSender<T> {
    /// Connects this sender, returning a [`ConnectedSender`] that can be used
    /// to send data into this channel
    ///
    /// Dropping the returned future before it completes disconnects this
    /// sender, as dropping it unconnected would.
    ///
    /// [`ConnectedSender`]: struct.ConnectedSender.html
    pub async fn connect(mut self) -> ConnectedSender<T> {
        let state = self.state.clone().unwrap();
        let reactor = crate::executor().reactor();
        state.buffer.connect(reactor.id());
        let id = reactor.register_shared_channel(Box::new(enclose! {(state) move || {
            if state.buffer.consumer_disconnected() {
                state.buffer.capacity()
            } else {
                state.buffer.free_space()
            }
        }}));

        let peer_connected: Rc<dyn Fn() -> bool> =
            Rc::new(enclose! {(state) move || state.buffer.peer_id() != 0});
        let reactor = Rc::downgrade(&reactor);
        let registration = unregister_on_drop(reactor.clone(), id);
        let peer = Connector::new(
            id,
            state.buffer.clone_internal(),
            peer_connected,
            reactor.clone(),
        );
        let notifier = peer.await;
        ScopeGuard::into_inner(registration);
        self.state = None;
        ConnectedSender {
            id,
            state,
            reactor,
            notifier,
        }
    }
}

impl<T: Send + Sized> ConnectedSender<T> {
    /// Sends data into this channel.
    ///
    /// It returns a [`GlommioError::Closed`] if the receiver is destroyed.
    /// It returns a [`GlommioError::WouldBlock`] if this is a bounded channel
    /// that has no more capacity
    ///
    /// # Examples
    /// ```
    /// use futures_lite::StreamExt;
    /// use glommio::{channels::shared_channel, prelude::*};
    ///
    /// let (sender, receiver) = shared_channel::new_bounded(1);
    /// let producer = LocalExecutorBuilder::default()
    ///     .name("producer")
    ///     .spawn(move || async move {
    ///         let sender = sender.connect().await;
    ///         sender.try_send(0);
    ///     })
    ///     .unwrap();
    /// let receiver = LocalExecutorBuilder::default()
    ///     .name("receiver")
    ///     .spawn(move || async move {
    ///         let mut receiver = receiver.connect().await;
    ///         receiver.next().await.unwrap();
    ///     })
    ///     .unwrap();
    /// producer.join().unwrap();
    /// receiver.join().unwrap();
    /// ```
    ///
    /// [`BrokenPipe`]: https://doc.rust-lang.org/std/io/enum.ErrorKind.html#variant.BrokenPipe
    /// [`WouldBlock`]: https://doc.rust-lang.org/std/io/enum.ErrorKind.html#variant.WouldBlock
    /// [`Other`]: https://doc.rust-lang.org/std/io/enum.ErrorKind.html#variant.Other
    /// [`GlommioError`]: ../../struct.GlommioError.html
    pub fn try_send(&self, item: T) -> Result<(), T> {
        // This is a shared channel so state can change under our noses.
        // We test if the buffer is disconnected before sending to avoid
        // sending a value that will not be received (otherwise we would only
        // receive WouldBlock when the buffer capacity fills).
        //
        // However after we try_push(), we can still fail because the buffer
        // disconnected between now and then. That's okay as all we're trying to
        // do here is prevent unnecessary sends.
        //
        // Note that we check `producer_disconnected` because:
        // (1) senders can be referenced by multiple tasks simultaneously
        // (2) senders can be closed at any time using `Self::close` (which does not
        // automatically cause the receiver / consumer to drop)
        // (3) `Self::try_send` is used by async `Self::send`, which will not
        // unblock correctly if the channel was closed in another task
        if self.state.buffer.consumer_disconnected()
            || self.state.buffer.buffer.producer_disconnected()
        {
            return Err(GlommioError::Closed(ResourceType::Channel(item)));
        }
        match self.state.buffer.buffer.try_push(item) {
            None => {
                self.notifier.notify(false);
                Ok(())
            }
            Some(item) => {
                let res = if self.state.buffer.consumer_disconnected()
                    || self.state.buffer.buffer.producer_disconnected()
                {
                    GlommioError::Closed(ResourceType::Channel(item))
                } else {
                    GlommioError::WouldBlock(ResourceType::Channel(item))
                };
                Err(res)
            }
        }
    }

    /// Sends data into this channel when it is ready to receive it
    ///
    /// Room is judged from the queue's size, which the receiver's thread
    /// updates; should a push still find the slot taken, this waits again
    /// rather than failing.
    ///
    /// # Examples
    /// ```
    /// use glommio::{channels::shared_channel, prelude::*};
    ///
    /// let (sender, receiver) = shared_channel::new_bounded(1);
    /// let producer = LocalExecutorBuilder::default()
    ///     .name("producer")
    ///     .spawn(move || async move {
    ///         let sender = sender.connect().await;
    ///         sender.send(0).await;
    ///     })
    ///     .unwrap();
    /// let receiver = LocalExecutorBuilder::default()
    ///     .name("receiver")
    ///     .spawn(move || async move {
    ///         let mut receiver = receiver.connect().await;
    ///         receiver.recv().await.unwrap();
    ///     })
    ///     .unwrap();
    /// producer.join().unwrap();
    /// receiver.join().unwrap();
    /// ```
    pub async fn send(&self, mut item: T) -> Result<(), T> {
        loop {
            future::poll_fn(|cx| self.wait_for_room(cx)).await;
            match self.try_send(item) {
                Err(GlommioError::WouldBlock(ResourceType::Channel(back))) => item = back,
                res => return res,
            }
        }
    }

    fn wait_for_room(&self, cx: &Context<'_>) -> Poll<()> {
        match self.state.buffer.free_space() > 0
            || self.state.buffer.producer_disconnected()
            || self.state.buffer.consumer_disconnected()
        {
            true => Poll::Ready(()),
            false => {
                self.reactor
                    .upgrade()
                    .unwrap()
                    .add_shared_channel_waker(self.id, cx.waker());
                Poll::Pending
            }
        }
    }

    /// Close the sender
    pub fn close(&self) {
        if !self.state.buffer.disconnect() {
            if let Some(r) = self.reactor.upgrade() {
                self.notifier.notify(false);
                // wake other tasks `awaiting` the same sender letting them know sender is
                // closed; we don't `unregister_shared_channel` here because
                // another task could still be `await`ing this sender, in which
                // case we need to be able to `wake` it
                r.process_shared_channels_by_id(self.id);
            }
        }
    }
}

impl<T: 'static + Send + Sized> SharedReceiver<T> {
    /// Connects this receiver, returning a [`ConnectedReceiver`] that can be
    /// used to send data into this channel
    ///
    /// Dropping the returned future before it completes disconnects this
    /// receiver, as dropping it unconnected would.
    ///
    /// [`ConnectedReceiver`]: struct.ConnectedReceiver.html
    pub async fn connect(mut self) -> ConnectedReceiver<T> {
        let reactor = crate::executor().reactor();
        let state = self.state.clone().unwrap();
        state.buffer.connect(reactor.id());
        let id = reactor.register_shared_channel(Box::new(enclose! { (state) move || {
            if state.buffer.producer_disconnected() {
                state.buffer.capacity()
            } else {
                state.buffer.size()
            }
        }}));

        let peer_connected: Rc<dyn Fn() -> bool> =
            Rc::new(enclose! {(state) move || state.buffer.peer_id() != 0});
        let reactor = Rc::downgrade(&reactor);
        let registration = unregister_on_drop(reactor.clone(), id);
        let peer = Connector::new(
            id,
            state.buffer.clone_internal(),
            peer_connected,
            reactor.clone(),
        );
        let notifier = peer.await;
        ScopeGuard::into_inner(registration);
        self.state = None;
        ConnectedReceiver {
            id,
            state,
            reactor,
            notifier,
        }
    }
}

impl<T: Send + Sized> ConnectedReceiver<T> {
    /// Receives data from this channel
    ///
    /// If the sender is no longer available it returns [`None`]. Otherwise,
    /// blocks until an item is available and returns it wrapped in [`Some`].
    ///
    /// Notice that this is also available as a Stream. Whether to consume from
    /// a stream or `recv` is up to the application. The biggest difference
    /// is that [`StreamExt`]'s [`next`] method takes a mutable reference to
    /// self. If the LocalReceiver is, say, behind an [`Rc`] it may be more
    /// ergonomic to `recv`.
    ///
    /// # Examples
    /// ```
    /// use glommio::{channels::shared_channel, prelude::*};
    ///
    /// let (sender, receiver) = shared_channel::new_bounded(1);
    /// let producer = LocalExecutorBuilder::default()
    ///     .name("producer")
    ///     .spawn(move || async move {
    ///         let sender = sender.connect().await;
    ///         sender.try_send(0u32);
    ///     })
    ///     .unwrap();
    /// let receiver = LocalExecutorBuilder::default()
    ///     .name("receiver")
    ///     .spawn(move || async move {
    ///         let mut receiver = receiver.connect().await;
    ///         let x = receiver.recv().await.unwrap();
    ///         assert_eq!(x, 0);
    ///     })
    ///     .unwrap();
    /// producer.join().unwrap();
    /// receiver.join().unwrap();
    /// ```
    ///
    /// [`None`]: https://doc.rust-lang.org/std/option/enum.Option.html#variant.None
    /// [`Some`]: https://doc.rust-lang.org/std/option/enum.Option.html#variant.Some
    /// [`StreamExt`]: https://docs.rs/futures-lite/2.6.0/futures_lite/stream/index.htmll
    /// [`next`]: https://docs.rs/futures-lite/2.6.0/futures_lite/stream/trait.StreamExt.html#method.next
    /// [`Rc`]: https://doc.rust-lang.org/std/rc/struct.Rc.html
    pub async fn recv(&self) -> Option<T> {
        let waiter = future::poll_fn(|cx| self.recv_one(cx));
        waiter.await
    }

    fn recv_one(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.do_recv_one(cx, false)
    }

    fn do_recv_one(&self, cx: &mut Context<'_>, disconnected: bool) -> Poll<Option<T>> {
        match self.state.buffer.buffer.try_pop() {
            None => {
                if disconnected {
                    Poll::Ready(None)
                } else if self.state.buffer.producer_disconnected() {
                    // Double check in case the producer sent the last message and
                    // disconnected right after a `None` is returned from `try_pop`
                    self.do_recv_one(cx, true)
                } else {
                    self.reactor
                        .upgrade()
                        .unwrap()
                        .add_shared_channel_waker(self.id, cx.waker());
                    Poll::Pending
                }
            }
            res => {
                self.notifier.notify(false);
                Poll::Ready(res)
            }
        }
    }
}

impl<T: Send + Sized> Stream for ConnectedReceiver<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.recv_one(cx)
    }
}

impl<T: Send + Sized> Drop for SharedSender<T> {
    /// Disconnects a half that never connected, and wakes a peer waiting
    /// in `connect()` for it.
    ///
    /// The light barrier orders the disconnect before the look at the
    /// peer's id, and pairs with the heavy one a connecting peer parks
    /// behind: either its park drain sees the disconnect, or this sees its
    /// id and notifies it. Without it both can miss, and the peer's connect
    /// parks for good.
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            // Never connected, we must connect ourselves.
            if !state.buffer.disconnect() {
                sys::light_membarrier();
                let id = state.buffer.peer_id();
                if let Some(notifier) = sys::get_sleep_notifier_for(id) {
                    notifier.notify(false);
                }
            }
        }
    }
}

impl<T: Send + Sized> Drop for SharedReceiver<T> {
    /// Disconnects a half that never connected, and wakes a peer waiting
    /// in `connect()` for it.
    ///
    /// The light barrier orders the disconnect before the look at the
    /// peer's id, and pairs with the heavy one a connecting peer parks
    /// behind: either its park drain sees the disconnect, or this sees its
    /// id and notifies it. Without it both can miss, and the peer's connect
    /// parks for good.
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            // Never connected, we must connect ourselves.
            if !state.buffer.disconnect() {
                sys::light_membarrier();
                let id = state.buffer.peer_id();
                if let Some(notifier) = sys::get_sleep_notifier_for(id) {
                    notifier.notify(false);
                }
            }
        }
    }
}

impl<T: Send + Sized> Drop for ConnectedReceiver<T> {
    fn drop(&mut self) {
        let already_disconnected = self.state.buffer.disconnect();
        if let Some(r) = self.reactor.upgrade() {
            if !already_disconnected {
                self.notifier.notify(false);
            }
            r.unregister_shared_channel(self.id);
        }
    }
}

impl<T: Send + Sized> Drop for ConnectedSender<T> {
    /// Unregisters even after [`close`](Self::close), which keeps the
    /// registration for tasks still waiting on this sender.
    fn drop(&mut self) {
        let already_disconnected = self.state.buffer.disconnect();
        if let Some(r) = self.reactor.upgrade() {
            if !already_disconnected {
                self.notifier.notify(false);
            }
            r.unregister_shared_channel(self.id);
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn send_to_a_full_channel_fails_once_the_receiver_is_gone() {
        let (tx, rx) = new_bounded::<u32>(1);
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel::<()>();
        let (filled_tx, filled_rx) = std::sync::mpsc::channel::<()>();
        let receiver = crate::LocalExecutorBuilder::default()
            .spawn(move || async move {
                let rx = rx.connect().await;
                filled_rx.recv().unwrap();
                drop(rx);
                dropped_tx.send(()).unwrap();
            })
            .unwrap();
        let sender = crate::LocalExecutorBuilder::default()
            .spawn(move || async move {
                let tx = tx.connect().await;
                tx.try_send(1).unwrap();
                filled_tx.send(()).unwrap();
                dropped_rx.recv().unwrap();
                let sent =
                    crate::timer::timeout(Duration::from_secs(5), async { Ok(tx.send(2).await) })
                        .await
                        .expect("send did not return");
                assert!(matches!(sent, Err(GlommioError::Closed(_))));
            })
            .unwrap();
        receiver.join().unwrap();
        sender.join().unwrap();
    }

    /// Guards against `close()` followed by a drop keeping the buffer, and
    /// anything still in it, alive until the sender's executor exits.
    #[test]
    fn a_closed_sender_releases_the_buffer_when_dropped() {
        let (tx, rx) = new_bounded(4);
        let live = Arc::new(AtomicUsize::new(0));
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel::<()>();
        let (released_tx, released_rx) = std::sync::mpsc::channel::<()>();
        let counted = live.clone();
        let sender = crate::LocalExecutorBuilder::default()
            .spawn(move || async move {
                let tx = tx.connect().await;
                counted.fetch_add(1, Ordering::Relaxed);
                tx.try_send(WithDrop(counted.clone(), 1)).ok().unwrap();
                tx.close();
                drop(tx);
                dropped_tx.send(()).unwrap();
                released_rx.recv().unwrap();
                assert_eq!(
                    counted.load(Ordering::Relaxed),
                    0,
                    "the unreceived item outlived both halves"
                );
            })
            .unwrap();
        let receiver = crate::LocalExecutorBuilder::default()
            .spawn(move || async move {
                let rx = rx.connect().await;
                dropped_rx.recv().unwrap();
                drop(rx);
                released_tx.send(()).unwrap();
            })
            .unwrap();
        receiver.join().unwrap();
        sender.join().unwrap();
        assert_eq!(live.load(Ordering::Relaxed), 0);
    }

    /// Guards against a receiver attaching to a sender whose `connect()` was
    /// cancelled, and then waiting forever for messages that cannot come.
    #[test]
    fn a_receiver_sees_a_cancelled_sender_connect_as_closed() {
        let (tx, rx) = new_bounded::<u32>(4);
        let (cancelled_tx, cancelled_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let sender = crate::LocalExecutorBuilder::default()
            .spawn(move || async move {
                let registered = crate::executor().reactor().registered_shared_channels();
                let connected = crate::timer::timeout(Duration::from_millis(20), async {
                    tx.connect().await;
                    Ok(())
                })
                .await;
                assert_eq!(
                    crate::executor().reactor().registered_shared_channels(),
                    registered,
                    "the cancelled connect left its registration behind"
                );
                assert!(connected.is_err(), "the receiver was not there to connect");
                cancelled_tx.send(()).unwrap();
                done_rx.recv().unwrap();
            })
            .unwrap();
        let receiver = crate::LocalExecutorBuilder::default()
            .spawn(move || async move {
                cancelled_rx.recv().unwrap();
                let received = crate::timer::timeout(Duration::from_secs(5), async {
                    Ok(rx.connect().await.recv().await)
                })
                .await
                .expect("recv waited on a sender that no longer exists");
                assert_eq!(received, None);
                done_tx.send(()).unwrap();
            })
            .unwrap();
        receiver.join().unwrap();
        sender.join().unwrap();
    }

    /// Guards against a sender attaching to a receiver whose `connect()` was
    /// cancelled, and then filling a buffer nobody will drain.
    #[test]
    fn a_sender_sees_a_cancelled_receiver_connect_as_closed() {
        let (tx, rx) = new_bounded::<u32>(1);
        let (cancelled_tx, cancelled_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let receiver = crate::LocalExecutorBuilder::default()
            .spawn(move || async move {
                let registered = crate::executor().reactor().registered_shared_channels();
                let connected = crate::timer::timeout(Duration::from_millis(20), async {
                    rx.connect().await;
                    Ok(())
                })
                .await;
                assert_eq!(
                    crate::executor().reactor().registered_shared_channels(),
                    registered,
                    "the cancelled connect left its registration behind"
                );
                assert!(connected.is_err(), "the sender was not there to connect");
                cancelled_tx.send(()).unwrap();
                done_rx.recv().unwrap();
            })
            .unwrap();
        let sender = crate::LocalExecutorBuilder::default()
            .spawn(move || async move {
                cancelled_rx.recv().unwrap();
                let sent = crate::timer::timeout(Duration::from_secs(5), async {
                    let tx = tx.connect().await;
                    let first = tx.send(1).await;
                    let second = tx.send(2).await;
                    Ok((first, second))
                })
                .await
                .expect("send waited on a receiver that no longer exists");
                assert!(
                    matches!(
                        sent,
                        (Err(GlommioError::Closed(_)), _) | (_, Err(GlommioError::Closed(_)))
                    ),
                    "{sent:?}"
                );
                done_tx.send(()).unwrap();
            })
            .unwrap();
        sender.join().unwrap();
        receiver.join().unwrap();
    }
    use crate::LocalExecutor;
    use crate::{
        timer::{sleep, Timer},
        LocalExecutorBuilder, Placement,
    };
    use futures_lite::{future::poll_fn, FutureExt, StreamExt};
    use std::{
        future::Future,
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc, Arc,
        },
        task::Poll,
        time::Duration,
    };

    #[test]
    fn producer_consumer() {
        let (sender, receiver) = new_bounded(10);

        let ex1 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let sender = sender.connect().await;
                Timer::new(Duration::from_millis(10)).await;
                sender.try_send(100).unwrap();
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let receiver = receiver.connect().await;
                let x = receiver.recv().await;
                assert_eq!(x.unwrap(), 100);
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn producer_stream_consumer() {
        let (sender, receiver) = new_bounded(1);

        let ex1 = LocalExecutorBuilder::new(Placement::Fixed(0))
            .spin_before_park(Duration::from_millis(1000000))
            .spawn(move || async move {
                let sender = sender.connect().await;
                for _ in 0..10 {
                    sender.send(1).await.unwrap();
                    Timer::new(Duration::from_millis(1)).await;
                }
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::new(Placement::Fixed(1))
            .spin_before_park(Duration::from_millis(1000000))
            .spawn(move || async move {
                let receiver = receiver.connect().await;
                let sum = receiver.fold(0, |acc, x| acc + x).await;
                assert_eq!(sum, 10);
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn consumer_sleeps_before_producer_produces() {
        let (sender, receiver) = new_bounded(1);

        let ex1 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                Timer::new(Duration::from_millis(100)).await;
                let sender = sender.connect().await;
                sender.send(1).await.unwrap();
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let receiver = receiver.connect().await;
                let recv = receiver.recv().await.unwrap();
                assert_eq!(recv, 1);
                let sum = receiver.fold(0, |acc, x| acc + x).await;
                assert_eq!(sum, 0);
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn producer_sleeps_before_consumer_consumes() {
        let (sender, receiver) = new_bounded(1);

        let ex1 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let sender = sender.connect().await;
                // This will go right away because the channel fits 1 element
                sender.try_send(1).unwrap();
                // This will sleep. The consumer should unblock us
                sender.send(1).await.unwrap();
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                Timer::new(Duration::from_millis(100)).await;
                let receiver = receiver.connect().await;
                let sum = receiver.fold(0, |acc, x| acc + x).await;
                assert_eq!(sum, 2);
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn producer_never_connects() {
        let (sender, receiver) = new_bounded(1);

        let ex1 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                drop(sender);
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let receiver: ConnectedReceiver<usize> = receiver.connect().await;
                assert!(receiver.recv().await.is_none());
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn destroy_with_pending_wakers() {
        let (sender, receiver) = new_bounded::<u8>(1);

        let ex2 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let receiver = receiver.connect();
                let sender = sender.connect();
                let (receiver, sender) = futures::future::join(receiver, sender).await;

                future::poll_fn(move |cx| {
                    let mut f1 = receiver.recv().boxed_local();
                    assert_eq!(f1.poll(cx), Poll::Pending);
                    assert!(sender.try_send(1).is_ok());
                    let r = receiver.recv_one(cx);
                    assert_eq!(r, Poll::Ready(Some(1)));
                    r
                })
                .await;
                sleep(Duration::from_secs(1)).await;
            })
            .unwrap();

        ex2.join().unwrap();
    }

    #[test]
    fn consumer_never_connects() {
        let (sender, receiver) = new_bounded(1);

        let ex1 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                drop(receiver);
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                Timer::new(Duration::from_millis(100)).await;
                let sender: ConnectedSender<usize> = sender.connect().await;
                match sender.send(0).await {
                    Ok(_) => panic!("Should not have sent"),
                    Err(GlommioError::Closed(ResourceType::Channel(_))) => {
                        // all good
                    }
                    Err(other_err) => {
                        panic!("incorrect error type: '{other_err}' for channel send")
                    }
                }
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn pass_function() {
        let (sender, receiver) = new_bounded(10);

        let ex1 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let sender = sender.connect().await;
                Timer::new(Duration::from_millis(10)).await;
                if sender.send(|| 32).await.is_err() {
                    panic!("send failed");
                }
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let receiver = receiver.connect().await;
                let x = receiver.recv().await.unwrap();
                assert_eq!(32, x());
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn send_to_full_channel() {
        let (sender, receiver) = new_bounded(1);

        let status = Arc::new(AtomicUsize::new(0));
        let s1 = status.clone();

        let ex1 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let sender = sender.connect().await;
                sender.send(0).await.unwrap();
                let x = sender.try_send(1);
                assert!(x.is_err());
                s1.store(1, Ordering::Relaxed);
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let receiver = receiver.connect().await;

                while status.load(Ordering::Relaxed) == 0 {}
                let x = receiver.recv().await.unwrap();
                assert_eq!(0, x);
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn non_copy_shared() {
        let (sender, receiver) = new_bounded(1);

        let ex1 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let sender = sender.connect().await;
                let string1 = "Some string data here..".to_string();
                sender.send(string1).await.unwrap();
                let string2 = "different data..".to_string();
                sender.send(string2).await.unwrap();
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let receiver = receiver.connect().await;
                let x = receiver.recv().await.unwrap();
                assert_eq!(x, "Some string data here..".to_string());
                let y = receiver.recv().await.unwrap();
                assert_eq!(y, "different data..".to_string());
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn copy_shared() {
        let (sender, receiver) = new_bounded(2);

        let ex1 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let sender = sender.connect().await;
                sender.send(100usize).await.unwrap();
                sender.send(200usize).await.unwrap();
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let receiver = receiver.connect().await;
                let x = receiver.recv().await.unwrap();
                let y = receiver.recv().await.unwrap();
                assert_eq!(x, 100usize);
                assert_eq!(y, 200usize);
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[derive(Debug)]
    struct WithDrop(Arc<AtomicUsize>, usize);

    impl Drop for WithDrop {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn shared_drop_gets_called() {
        let (sender, receiver) = new_bounded(1000);

        let original = Arc::new(AtomicUsize::new(0));
        let send_count = original.clone();
        let drop_count = original.clone();

        let ex1 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let sender = sender.connect().await;
                for x in 0..1000 {
                    let val = WithDrop(send_count.clone(), x);
                    drop_count.fetch_add(1, Ordering::Relaxed);
                    let _ = sender.send(val).await;
                }
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let receiver = receiver.connect().await;
                let y = receiver.recv().await.unwrap();
                drop(y);
                Timer::new(Duration::from_secs(1)).await;
                let y = receiver.recv().await.unwrap();
                drop(y);
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();

        // make sure that our total is always 0, to ensure we have dropped all entries,
        // despite differing conditions.
        assert_eq!(original.load(Ordering::Relaxed), 0usize);
    }

    #[test]
    fn shared_drop_gets_called_reversed() {
        let (sender, receiver) = new_bounded(100);

        let original = Arc::new(AtomicUsize::new(0));
        let send_count = original.clone();
        let drop_count = original.clone();

        let ex1 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let sender = sender.connect().await;
                for x in 0..110 {
                    let val = WithDrop(send_count.clone(), x);
                    drop_count.fetch_add(1, Ordering::Relaxed);
                    let _ = sender.send(val).await;
                }
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let receiver = receiver.connect().await;
                let y = receiver.recv().await.unwrap();
                drop(y);
                let y = receiver.recv().await.unwrap();
                drop(y);
            })
            .unwrap();

        ex2.join().unwrap();
        ex1.join().unwrap();

        // make sure that our total is always 0, to ensure we have dropped all entries,
        // despite differing conditions.
        assert_eq!(original.load(Ordering::Relaxed), 0usize);
    }

    #[test]
    fn shared_drop_cascade_drop_executor() {
        let (sender, receiver) = new_bounded(100);

        let original = Arc::new(AtomicUsize::new(0));
        let send_count = original.clone();
        let drop_count = original.clone();

        let ex1 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let sender = sender.connect().await;
                for x in 0..50 {
                    let val = WithDrop(send_count.clone(), x);
                    drop_count.fetch_add(1, Ordering::Relaxed);
                    let _ = sender.send(val).await;
                }
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let receiver = receiver.connect().await;
                let _resp = receiver.recv().await.unwrap();
            })
            .unwrap();
        ex2.join().unwrap();
        ex1.join().unwrap();

        // make sure that our total is always 0, to ensure we have dropped all entries,
        // despite differing conditions.
        assert_eq!(original.load(Ordering::Relaxed), 0usize);
    }

    #[test]
    fn shared_drop_cascade_drop_executor_reverse() {
        let (sender, receiver) = new_bounded(100);
        let (connected_tx, connected_rx) = mpsc::channel();

        let original = Arc::new(AtomicUsize::new(0));
        let send_count = original.clone();
        let drop_count = original.clone();
        let sender_connected = connected_tx.clone();

        let ex1 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let sender = sender.connect().await;
                sender_connected.send(()).unwrap();
                for x in 0..50 {
                    let val = WithDrop(send_count.clone(), x);
                    drop_count.fetch_add(1, Ordering::SeqCst);
                    let _ = sender.send(val).await;
                }
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let receiver = receiver.connect().await;
                connected_tx.send(()).unwrap();
                for x in 0..50 {
                    let resp = receiver.recv().await.unwrap();
                    assert_eq!(x, resp.1);
                }
            })
            .unwrap();

        // Both endpoints must be connected before destroying the sender's
        // executor. Otherwise the receiver can observe its peer ID after the
        // sender's reactor has already removed the corresponding notifier.
        connected_rx.recv().unwrap();
        connected_rx.recv().unwrap();
        drop(ex1);
        ex2.join().unwrap();

        // make sure that our total is always 0, to ensure we have dropped all entries,
        // despite differing conditions.
        assert_eq!(original.load(Ordering::Relaxed), 0usize);
    }

    #[test]
    fn close_sender_while_blocked_on_send() {
        use std::sync::{Condvar, Mutex};

        let (sender, receiver) = new_bounded(10);
        let cv_mtx_1 = Arc::new((Condvar::new(), Mutex::new(0)));
        let cv_mtx_2 = Arc::clone(&cv_mtx_1);
        let cv_mtx_3 = Arc::clone(&cv_mtx_1);

        let ex1 = LocalExecutorBuilder::default()
            .spawn({
                move || async move {
                    let s1 = Rc::new(sender.connect().await);
                    let s2 = Rc::clone(&s1);
                    let t1 = crate::executor().spawn_local(async move {
                        let mut ii = 0;
                        while s1.try_send(ii).is_ok() {
                            ii += 1;
                        }
                        s1.close();
                        *cv_mtx_1.1.lock().unwrap() = 1;
                        cv_mtx_1.0.notify_all();
                    });
                    #[allow(clippy::await_holding_lock)]
                    let t2 = crate::executor().spawn_local(async move {
                        let mut lck = cv_mtx_2
                            .0
                            .wait_while(cv_mtx_2.1.lock().unwrap(), |l| *l < 1)
                            .unwrap();
                        assert!(s2.send(-1).await.is_err());
                        *lck = 2;
                        cv_mtx_2.0.notify_all();
                    });
                    t1.await;
                    t2.await;
                }
            })
            .unwrap();

        let ex2 = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let receiver = receiver.connect().await;
                {
                    let _lck = cv_mtx_3
                        .0
                        .wait_while(cv_mtx_3.1.lock().unwrap(), |l| *l < 2)
                        .unwrap();
                };
                while let Some(v) = receiver.recv().await {
                    assert!(0 <= v);
                }
            })
            .unwrap();

        ex1.join().unwrap();
        ex2.join().unwrap();
    }

    #[test]
    fn peer_connect_after_canceled_connection_and_retained_task_waker() {
        let (sender, receiver) = new_bounded::<u8>(1);
        let executor = LocalExecutor::default();
        let retained_waker = executor.run(async move {
            let mut connection = Box::pin(sender.connect());
            let waker = poll_fn(|cx| {
                assert!(connection.as_mut().poll(cx).is_pending());
                Poll::Ready(cx.waker().clone())
            })
            .await;
            drop(connection);
            waker
        });
        drop(executor);
        // The notifier is released off the executor thread, so the id outlives it.
        std::thread::sleep(Duration::from_millis(100));

        let executor = LocalExecutor::default();
        executor.run(async move {
            drop(receiver.connect().await);
        });
        drop(retained_waker);
    }

    /// CPU time the calling thread has used, user and system.
    fn thread_cpu_time() -> Duration {
        use nix::sys::{
            resource::{getrusage, UsageWho},
            time::TimeValLike,
        };
        let usage = getrusage(UsageWho::RUSAGE_THREAD).expect("getrusage failed");
        let micros = usage.user_time().num_microseconds() + usage.system_time().num_microseconds();
        Duration::from_micros(micros as u64)
    }

    /// Connects the receiver and the sender from two executors, each after
    /// its delay, and returns the wall and CPU time each `connect()` took,
    /// receiver first.
    ///
    /// Neither half is used or dropped until both connects have returned, so
    /// nothing but the connect itself can wake an executor waiting in one.
    fn connect_from_two_executors(
        receiver_delay: Duration,
        sender_delay: Duration,
    ) -> [(Duration, Duration); 2] {
        fn half<H: 'static, F: Future<Output = H>>(
            delay: Duration,
            start: Arc<std::sync::Barrier>,
            connect: impl FnOnce() -> F + Send + 'static,
            done: mpsc::Sender<(Duration, Duration)>,
            release: mpsc::Receiver<()>,
        ) -> crate::ExecutorJoinHandle<()> {
            LocalExecutorBuilder::default()
                .spawn(move || async move {
                    start.wait();
                    sleep(delay).await;
                    let (cpu, wall) = (thread_cpu_time(), std::time::Instant::now());
                    let connected = connect().await;
                    done.send((wall.elapsed(), thread_cpu_time() - cpu))
                        .unwrap();
                    let _ = release.recv();
                    drop(connected);
                })
                .unwrap()
        }

        let (sender, receiver) = new_bounded::<u32>(1);
        let start = Arc::new(std::sync::Barrier::new(2));
        let (receiver_done, receiver_took) = mpsc::channel();
        let (sender_done, sender_took) = mpsc::channel();
        let (release_receiver, receiver_released) = mpsc::channel();
        let (release_sender, sender_released) = mpsc::channel();
        let receiver = half(
            receiver_delay,
            start.clone(),
            move || receiver.connect(),
            receiver_done,
            receiver_released,
        );
        let sender = half(
            sender_delay,
            start,
            move || sender.connect(),
            sender_done,
            sender_released,
        );
        let took = [receiver_took, sender_took].map(|took| {
            took.recv_timeout(Duration::from_secs(5))
                .expect("a connect() never returned")
        });
        drop((release_receiver, release_sender));
        receiver.join().unwrap();
        sender.join().unwrap();
        took
    }

    /// Guards against an executor spinning a core while its `connect()`
    /// waits for the peer, instead of parking until the peer connects.
    #[test]
    fn waiting_for_the_peer_to_connect_burns_no_cpu() {
        const PEER_DELAY: Duration = Duration::from_millis(200);
        let [(wall, cpu), _] = connect_from_two_executors(Duration::ZERO, PEER_DELAY);
        assert!(
            wall >= PEER_DELAY / 2,
            "connect returned after {wall:?}, before the peer could have connected"
        );
        assert!(
            cpu * 10 < wall,
            "waiting {wall:?} for the peer used {cpu:?} of CPU"
        );
    }

    /// Guards against a sender left waiting by a receiver that connects later.
    #[test]
    fn a_sender_connect_returns_once_the_receiver_connects() {
        connect_from_two_executors(Duration::from_millis(100), Duration::ZERO);
    }

    /// Guards against a receiver left waiting by a sender that connects later.
    #[test]
    fn a_receiver_connect_returns_once_the_sender_connects() {
        connect_from_two_executors(Duration::ZERO, Duration::from_millis(100));
    }

    /// Guards against both halves waiting for each other when they connect
    /// at the same moment, each missing the other's arrival.
    #[test]
    fn both_halves_connect_when_they_connect_at_once() {
        for _ in 0..50 {
            connect_from_two_executors(Duration::ZERO, Duration::ZERO);
        }
    }

    /// Guards against a receiver polled again and again on a quiet channel,
    /// as `select!` against a timer does, queueing its waker once per poll.
    #[test]
    fn repolling_a_pending_recv_keeps_one_waker() {
        let (sender, receiver) = new_bounded::<u32>(1);
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let sender = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let sender = sender.connect().await;
                done_rx.recv().unwrap();
                drop(sender);
            })
            .unwrap();
        let receiver = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let receiver = receiver.connect().await;
                for _ in 0..100 {
                    assert!(futures_lite::future::poll_once(receiver.recv())
                        .await
                        .is_none());
                }
                let wakers = crate::executor()
                    .reactor()
                    .shared_channel_wakers(receiver.id);
                done_tx.send(()).unwrap();
                assert_eq!(wakers, 1, "one task queued {wakers} wakers");
            })
            .unwrap();
        receiver.join().unwrap();
        sender.join().unwrap();
    }

    /// Guards against a sender polled again and again on a full channel
    /// queueing its waker once per poll.
    #[test]
    fn repolling_a_pending_send_keeps_one_waker() {
        let (sender, receiver) = new_bounded::<u32>(1);
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let receiver = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let receiver = receiver.connect().await;
                done_rx.recv().unwrap();
                drop(receiver);
            })
            .unwrap();
        let sender = LocalExecutorBuilder::default()
            .spawn(move || async move {
                let sender = sender.connect().await;
                sender.try_send(0).unwrap();
                for i in 1..=100 {
                    assert!(futures_lite::future::poll_once(sender.send(i))
                        .await
                        .is_none());
                }
                let wakers = crate::executor().reactor().shared_channel_wakers(sender.id);
                done_tx.send(()).unwrap();
                assert_eq!(wakers, 1, "one task queued {wakers} wakers");
            })
            .unwrap();
        sender.join().unwrap();
        receiver.join().unwrap();
    }
}
