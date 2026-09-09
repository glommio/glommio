//! Task cancellation must not reenter borrowed reactor registries.

use std::{
    cell::{Cell, RefCell},
    future::{pending, Future},
    mem::ManuallyDrop,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    rc::Rc,
    sync::Arc,
    task::{Context, Wake, Waker},
    thread::{self, ThreadId},
    time::Duration,
};

use glommio::{
    channels::shared_channel::{self, ConnectedReceiver, ConnectedSender},
    task::JoinHandle,
    timer::Timer,
    LocalExecutorBuilder, PoolPlacement,
};

type HandleSlot = Rc<RefCell<Option<JoinHandle<()>>>>;

/// The reactor's stored waker owns the callback that releases the last handle.
struct CancelOnDrop {
    owner: ThreadId,
    slot: ManuallyDrop<HandleSlot>,
}

impl CancelOnDrop {
    fn waker(slot: &HandleSlot) -> Waker {
        Waker::from(Arc::new(Self {
            owner: thread::current().id(),
            slot: ManuallyDrop::new(slot.clone()),
        }))
    }
}

/// SAFETY: Only Drop accesses the Rc, after checking the originating thread.
/// ManuallyDrop prevents a foreign-thread drop from destroying local state.
unsafe impl Send for CancelOnDrop {}

/// SAFETY: Shared access through Wake never reads or modifies the local slot.
/// Arc manages waker references atomically; Drop requires exclusive access.
unsafe impl Sync for CancelOnDrop {}

#[expect(
    clippy::manual_noop_waker,
    reason = "Its destructor releases the task handle when the reactor releases its waker."
)]
impl Wake for CancelOnDrop {
    fn wake(self: Arc<Self>) {}
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        assert_eq!(
            thread::current().id(),
            self.owner,
            "cancellation waker dropped outside its owning test thread",
        );
        let slot = unsafe { ManuallyDrop::take(&mut self.slot) };
        drop(slot.take());
    }
}

struct DropTimer {
    timer: Option<Timer>,
    succeeded: Rc<Cell<Option<bool>>>,
}

impl Drop for DropTimer {
    /// Catch inside the future's destructor so a borrow failure fails the test
    /// instead of triggering the task runtime's abort-on-panic guard.
    fn drop(&mut self) {
        let timer = self.timer.take();
        self.succeeded
            .set(Some(catch_unwind(AssertUnwindSafe(|| drop(timer))).is_ok()));
    }
}

fn executor() -> glommio::LocalExecutor {
    LocalExecutorBuilder::default()
        .io_memory(0)
        .blocking_thread_pool_placement(PoolPlacement::Unbound(1))
        .make()
        .unwrap()
}

/// Cancelling another executor's task must not reenter the active reactor.
#[test]
fn cross_executor_cancellation_during_timer_waker_replacement() {
    let first = executor();
    let second = executor();
    let succeeded = Rc::new(Cell::new(None));
    let guard = first.run(async {
        let mut timer = Timer::new(Duration::from_secs(60));
        assert!(Pin::new(&mut timer)
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        DropTimer {
            timer: Some(timer),
            succeeded: succeeded.clone(),
        }
    });
    let handle = second
        .spawn(async move {
            pending::<()>().await;
            drop(guard);
        })
        .detach();
    let handle_to_drop = Rc::new(RefCell::new(Some(handle)));

    first.run(async {
        let mut trigger = Timer::new(Duration::from_secs(60));
        let waker = CancelOnDrop::waker(&handle_to_drop);
        assert!(Pin::new(&mut trigger)
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        drop(waker);
        assert!(Pin::new(&mut trigger)
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
    });
    second.run(async {});
    assert_eq!(
        succeeded.get(),
        Some(true),
        "task cleanup must drop its timer without reentering a borrowed registry",
    );
}

/// Polling a timer outside run() must not make synchronous cleanup reentrant.
#[test]
fn idle_owner_cancellation_during_timer_waker_replacement() {
    let owner = executor();
    let succeeded = Rc::new(Cell::new(None));
    let (guard, mut trigger) = owner.run(async {
        let mut timer = Timer::new(Duration::from_secs(60));
        assert!(Pin::new(&mut timer)
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        (
            DropTimer {
                timer: Some(timer),
                succeeded: succeeded.clone(),
            },
            Timer::new(Duration::from_secs(60)),
        )
    });
    let handle = owner
        .spawn(async move {
            pending::<()>().await;
            drop(guard);
        })
        .detach();
    let handle_to_drop = Rc::new(RefCell::new(Some(handle)));

    let waker = CancelOnDrop::waker(&handle_to_drop);
    assert!(Pin::new(&mut trigger)
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    drop(waker);
    assert!(Pin::new(&mut trigger)
        .poll(&mut Context::from_waker(Waker::noop()))
        .is_pending());

    owner.run(async {});
    assert_eq!(
        succeeded.get(),
        Some(true),
        "task cleanup must drop its timer without reentering a borrowed registry",
    );
}

/// Expiring timers must release the registry before callbacks can cancel tasks.
#[test]
fn cross_executor_cancellation_during_timer_expiry() {
    let first = executor();
    let second = executor();
    let succeeded = Rc::new(Cell::new(None));
    let guard = first.run(async {
        let mut timer = Timer::new(Duration::from_secs(60));
        assert!(Pin::new(&mut timer)
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending());
        DropTimer {
            timer: Some(timer),
            succeeded: succeeded.clone(),
        }
    });
    let handle = second
        .spawn(async move {
            pending::<()>().await;
            drop(guard);
        })
        .detach();
    let handle_to_drop = Rc::new(RefCell::new(Some(handle)));

    first.run(async {
        let mut trigger = Timer::new(Duration::from_secs(60));
        let waker = CancelOnDrop::waker(&handle_to_drop);
        assert!(Pin::new(&mut trigger)
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        drop(waker);
        trigger.reset(Duration::ZERO);
        while handle_to_drop.borrow().is_some() {
            Timer::new(Duration::from_millis(10)).await;
        }
        drop(trigger);
    });
    second.run(async {});
    assert_eq!(
        succeeded.get(),
        Some(true),
        "timer expiry must allow task cleanup to drop another timer",
    );
}

struct DropReceiver {
    receiver: Option<ConnectedReceiver<()>>,
    succeeded: Rc<Cell<Option<bool>>>,
}

impl Drop for DropReceiver {
    /// Catch registry borrow failures before the task's abort-on-panic guard.
    fn drop(&mut self) {
        let receiver = self.receiver.take();
        self.succeeded.set(Some(
            catch_unwind(AssertUnwindSafe(|| drop(receiver))).is_ok(),
        ));
    }
}

fn connected_channel(
    owner: &glommio::LocalExecutor,
) -> (ConnectedSender<()>, ConnectedReceiver<()>) {
    owner.run(async {
        let (sender, receiver) = shared_channel::new_bounded::<()>(1);
        let receiver = glommio::spawn_local(receiver.connect()).detach();
        let sender = sender.connect().await;
        (
            sender,
            receiver
                .await
                .expect("receiver connection task was canceled"),
        )
    })
}

fn shared_channel_callback_cleanup(close: bool) {
    let owner = executor();
    let succeeded = Rc::new(Cell::new(None));
    let (sender, receiver) = connected_channel(&owner);
    sender.try_send(()).expect("failed to fill channel");
    let guard = DropReceiver {
        receiver: Some(receiver),
        succeeded: succeeded.clone(),
    };
    let handle = owner
        .spawn(async move {
            pending::<()>().await;
            drop(guard);
        })
        .detach();
    let handle_to_drop = Rc::new(RefCell::new(Some(handle)));

    let waker = CancelOnDrop::waker(&handle_to_drop);
    let mut send = Box::pin(sender.send(()));
    assert!(send
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    drop(waker);
    if close {
        sender.close();
        assert!(
            handle_to_drop.borrow().is_none(),
            "closing the sender did not invoke its pending waker",
        );
    }
    drop(send);
    drop(sender);
    owner.run(async {});
    assert_eq!(
        succeeded.get(),
        Some(true),
        "receiver cleanup reentered the shared-channel registry",
    );
}

/// Closing a sender must permit callbacks to destroy another channel endpoint.
#[test]
fn idle_cancellation_during_shared_channel_close() {
    shared_channel_callback_cleanup(true);
}

/// Removing a sender must release the registry before dropping stored wakers.
#[test]
fn idle_cancellation_during_shared_channel_unregister() {
    shared_channel_callback_cleanup(false);
}

/// Readiness callbacks must permit cleanup to remove another channel endpoint.
#[test]
fn cross_executor_cancellation_during_shared_channel_readiness() {
    let first = executor();
    let second = executor();
    let succeeded = Rc::new(Cell::new(None));
    let (sender, receiver) = connected_channel(&first);
    sender.try_send(()).expect("failed to fill channel");

    let handle_to_drop = Rc::new(RefCell::new(None));
    let waker = CancelOnDrop::waker(&handle_to_drop);
    let mut send = Box::pin(sender.send(()));
    assert!(send
        .as_mut()
        .poll(&mut Context::from_waker(&waker))
        .is_pending());
    assert_eq!(
        Box::pin(receiver.recv())
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        std::task::Poll::Ready(Some(())),
    );

    let guard = DropReceiver {
        receiver: Some(receiver),
        succeeded: succeeded.clone(),
    };
    let handle = second
        .spawn(async move {
            pending::<()>().await;
            drop(guard);
        })
        .detach();
    assert!(handle_to_drop.replace(Some(handle)).is_none());
    drop(waker);

    first.run(async {
        while handle_to_drop.borrow().is_some() {
            Timer::new(Duration::from_millis(10)).await;
        }
    });
    assert!(
        handle_to_drop.borrow().is_none(),
        "channel readiness did not invoke its pending waker",
    );
    assert_eq!(
        succeeded.get(),
        Some(true),
        "readiness callbacks must allow cleanup to unregister a receiver",
    );
    drop(send);
    drop(sender);
    second.run(async {});
}

/// Connection callbacks must release the registry before task cancellation.
#[test]
fn cross_executor_cancellation_during_shared_channel_connection() {
    let first = executor();
    let second = executor();
    let succeeded = Rc::new(Cell::new(None));
    let (sender, receiver) = connected_channel(&first);
    let guard = DropReceiver {
        receiver: Some(receiver),
        succeeded: succeeded.clone(),
    };
    let handle = second
        .spawn(async move {
            pending::<()>().await;
            drop(guard);
        })
        .detach();
    let handle_to_drop = Rc::new(RefCell::new(Some(handle)));

    first.run(async {
        let (trigger_sender, trigger_receiver) = shared_channel::new_bounded::<()>(1);
        let mut connection = Box::pin(trigger_sender.connect());
        let waker = CancelOnDrop::waker(&handle_to_drop);
        assert!(connection
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending());
        drop(waker);
        while handle_to_drop.borrow().is_some() {
            Timer::new(Duration::from_millis(10)).await;
        }
        assert!(
            handle_to_drop.borrow().is_none(),
            "connection dispatch did not invoke its pending waker",
        );
        let trigger_receiver = trigger_receiver.connect().await;
        let trigger_sender = connection.await;
        drop((trigger_sender, trigger_receiver));
    });
    assert_eq!(
        succeeded.get(),
        Some(true),
        "connection callbacks must allow cleanup to unregister a receiver",
    );
    drop(sender);
    second.run(async {});
}
