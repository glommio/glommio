//! Executor-local callbacks invoked at the boundary before parking.

use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::Rc,
};

use glommio_dense_registry::{DenseRegistry, Registration};
use scopeguard::defer;

type Worker = Box<dyn FnMut()>;

/// Registration failed because all 256 idle callback slots are occupied.
/// Removals during a batch release their slots after the batch finishes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BeforeParkCapacityError;

impl fmt::Display for BeforeParkCapacityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("before-park callback capacity exhausted")
    }
}

impl std::error::Error for BeforeParkCapacityError {}

/// Keeps a callback registered with [`IdleWorkers::register`].
///
/// Dropping the handle unregisters the callback. If a batch is running, the
/// callback remains in that batch and is removed afterward. The handle is
/// local to its executor's thread and may safely outlive the executor.
#[must_use = "dropping the handle unregisters the callback"]
pub struct BeforeParkHandle {
    _registration: Option<Registration<Worker, u8>>,
}

impl fmt::Debug for BeforeParkHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BeforeParkHandle").finish_non_exhaustive()
    }
}

/// Separates callback lifetime from reactor lifetime, including during shutdown.
pub struct IdleWorkers {
    registry: RefCell<Option<Rc<DenseRegistry<Worker, u8>>>>,
    running: Cell<bool>,
}

impl Default for IdleWorkers {
    fn default() -> Self {
        Self {
            registry: RefCell::new(Some(Rc::default())),
            running: Cell::new(false),
        }
    }
}

impl IdleWorkers {
    /// Registers a callback, returning an error when all 256 slots are in use.
    /// A rejected callback is dropped before returning. During shutdown, the
    /// callback is immediately dropped and the returned handle is inert.
    pub fn register(
        &self,
        worker: impl FnMut() + 'static,
    ) -> Result<BeforeParkHandle, BeforeParkCapacityError> {
        let registry = self.registry.borrow().clone();
        let Some(registry) = registry else {
            drop(worker);
            return Ok(BeforeParkHandle {
                _registration: None,
            });
        };
        let registration = registry
            .insert(Box::new(worker))
            .map_err(|_| BeforeParkCapacityError)?;
        Ok(BeforeParkHandle {
            _registration: Some(registration),
        })
    }

    /// Runs the complete batch before allowing spawned futures to be polled.
    /// Panics on recursive iteration of an open registry.
    pub fn run(&self) {
        let Some(registry) = self.registry.borrow().clone() else {
            return;
        };
        self.run_batch(
            || registry.for_each(|worker| worker()),
            || registry.shuffle_first(),
        );
    }

    fn run_batch(&self, visit: impl FnOnce(), shuffle: impl FnOnce()) {
        assert!(!self.is_running(), "recursive idle callback iteration");
        self.running.set(true);
        defer!(self.running.set(false));
        defer!(shuffle());
        visit();
    }

    /// Whether local spawns must defer their initial poll until the batch ends.
    #[inline]
    pub fn is_running(&self) -> bool {
        self.running.get()
    }

    /// Schedules a spawn during an idle batch; otherwise starts it immediately.
    /// Only the selected operation receives the input and is invoked.
    #[inline]
    pub fn dispatch_spawn<T, R>(
        &self,
        input: T,
        schedule: impl FnOnce(T) -> R,
        run: impl FnOnce(T) -> R,
    ) -> R {
        if self.is_running() {
            schedule(input)
        } else {
            run(input)
        }
    }

    /// Releases callbacks. Call while the owning executor still supplies the
    /// cleanup and I/O context required by their destructors.
    pub fn shutdown(&self) {
        close_registry(&self.registry);
    }
}

fn close_registry<T>(registry: &RefCell<Option<T>>) {
    let removed = registry.borrow_mut().take();
    drop(removed);
}

/// Runs the complete idle batch, then parks only if no runnable work or I/O
/// progress is found. I/O is polled only when no runnable work is already known.
pub fn park_if_idle(
    run_batch: impl FnOnce(),
    has_work: impl FnOnce() -> bool,
    poll_io: impl FnOnce() -> bool,
    park: impl FnOnce(),
) {
    run_batch();
    if !has_work() && !poll_io() {
        park();
    }
}

#[cfg(kani)]
mod proofs {
    //! Bounded controller proofs use symbolic registry visitation and scheduler
    //! endpoints. The real executor integration is exercised by Glommio's tests.

    use std::{
        cell::{Cell, RefCell},
        future::Future,
        pin::Pin,
        rc::{Rc, Weak},
        task::{Context, Poll, Waker},
    };

    use super::{close_registry, park_if_idle, IdleWorkers};

    fn closed_workers() -> IdleWorkers {
        IdleWorkers {
            registry: RefCell::new(None),
            running: Cell::new(false),
        }
    }

    fn order() -> [u8; 3] {
        let order: [u8; 3] = kani::any();
        for index in 0..3 {
            kani::assume(order[index] < 3);
            for other in index + 1..3 {
                kani::assume(order[index] != order[other]);
            }
        }
        order
    }

    #[derive(Clone, Copy)]
    struct ProbeFuture<'a> {
        visited: &'a Cell<u8>,
        polls: &'a Cell<u8>,
    }

    impl Future for ProbeFuture<'_> {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            assert_eq!(self.visited.get(), 3, "future interrupted the idle batch");
            self.polls.set(self.polls.get() + 1);
            Poll::Ready(())
        }
    }

    fn poll(mut future: ProbeFuture<'_>) {
        let mut context = Context::from_waker(Waker::noop());
        assert!(Pin::new(&mut future).poll(&mut context).is_ready());
    }

    #[kani::proof]
    #[kani::unwind(5)]
    fn batches_defer_all_future_polls_and_restore_eager_spawning() {
        let workers = closed_workers();
        let order = order();
        let spawn: [bool; 3] = kani::any();
        let visited = Cell::new(0);
        let polls = Cell::new(0);
        let queued = RefCell::new([None; 3]);
        let shuffled = Cell::new(false);
        workers.run_batch(
            || {
                for index in order {
                    assert!(workers.is_running());
                    visited.set(visited.get() + 1);
                    if spawn[usize::from(index)] {
                        workers.dispatch_spawn(
                            ProbeFuture {
                                visited: &visited,
                                polls: &polls,
                            },
                            |future| queued.borrow_mut()[usize::from(index)] = Some(future),
                            poll,
                        );
                    }
                    assert_eq!(polls.get(), 0);
                }
            },
            || {
                assert!(workers.is_running());
                assert_eq!(visited.get(), 3);
                shuffled.set(true);
            },
        );
        assert!(!workers.is_running());
        assert!(shuffled.get());
        for (index, future) in queued.into_inner().into_iter().enumerate() {
            assert_eq!(future.is_some(), spawn[index]);
            if let Some(future) = future {
                workers.dispatch_spawn(future, |_| panic!("spawn remained deferred"), poll);
            }
        }
        assert_eq!(
            polls.get(),
            spawn.into_iter().filter(|spawn| *spawn).count() as u8
        );
        workers.dispatch_spawn(
            ProbeFuture {
                visited: &visited,
                polls: &polls,
            },
            |_| panic!("spawn outside the batch was deferred"),
            poll,
        );
        assert_eq!(
            polls.get(),
            spawn.into_iter().filter(|spawn| *spawn).count() as u8 + 1
        );
        kani::cover!(
            spawn.into_iter().all(|spawn| spawn),
            "all callbacks spawn work"
        );
        kani::cover!(
            spawn.into_iter().all(|spawn| !spawn),
            "callbacks create no work"
        );
    }

    struct SpawnOnDrop<'a> {
        workers: &'a IdleWorkers,
        visited: &'a Cell<u8>,
        queued: &'a Cell<bool>,
    }

    impl Drop for SpawnOnDrop<'_> {
        fn drop(&mut self) {
            assert!(self.workers.is_running());
            assert_eq!(self.visited.get(), 3);
            self.workers.dispatch_spawn(
                (),
                |_| self.queued.set(true),
                |_| panic!("eager destructor spawn"),
            );
        }
    }

    #[kani::proof]
    #[kani::unwind(5)]
    fn deferred_destructors_and_shuffle_remain_inside_the_batch() {
        let workers = closed_workers();
        let visited = Cell::new(0);
        let queued = Cell::new(false);
        let shuffled = Cell::new(false);
        workers.run_batch(
            || {
                let _removed = SpawnOnDrop {
                    workers: &workers,
                    visited: &visited,
                    queued: &queued,
                };
                for _ in order() {
                    visited.set(visited.get() + 1);
                }
            },
            || {
                assert!(workers.is_running());
                assert!(queued.get());
                shuffled.set(true);
            },
        );
        assert!(queued.get() && shuffled.get());
        assert!(!workers.is_running());
    }

    #[kani::proof]
    #[kani::unwind(5)]
    fn parking_rechecks_work_after_every_callback_finishes() {
        let actions: [bool; 3] = kani::any();
        let io_progress: bool = kani::any();
        let visited = Cell::new(0);
        let work = Cell::new(false);
        let checked_work = Cell::new(false);
        let polled_io = Cell::new(false);
        let parked = Cell::new(false);
        park_if_idle(
            || {
                for index in order() {
                    visited.set(visited.get() + 1);
                    if actions[usize::from(index)] {
                        work.set(true);
                    }
                }
            },
            || {
                assert_eq!(visited.get(), 3);
                checked_work.set(true);
                work.get()
            },
            || {
                assert!(checked_work.get());
                assert!(!work.get());
                polled_io.set(true);
                io_progress
            },
            || {
                assert_eq!(visited.get(), 3);
                assert!(!work.get() && !io_progress);
                assert!(polled_io.get());
                parked.set(true);
            },
        );
        assert_eq!(visited.get(), 3);
        assert!(checked_work.get());
        let any_work = actions.into_iter().any(|work| work);
        assert_eq!(polled_io.get(), !any_work);
        assert_eq!(parked.get(), !any_work && !io_progress);
        kani::cover!(parked.get(), "idle reactor may park");
        kani::cover!(any_work, "callback work vetoes parking");
        kani::cover!(!any_work && io_progress, "I/O progress vetoes parking");
    }

    #[kani::proof]
    #[kani::unwind(4)]
    fn shutdown_is_an_absorbing_state() {
        let workers = closed_workers();
        for _ in 0..3 {
            let action: u8 = kani::any();
            kani::assume(action < 3);
            match action {
                0 => workers.shutdown(),
                1 => workers.run(),
                2 => {
                    let handle = workers.register(|| panic!("closed callback ran")).unwrap();
                    assert!(handle._registration.is_none());
                }
                _ => unreachable!(),
            }
            assert!(workers.registry.borrow().is_none());
            assert!(!workers.is_running());
        }
        kani::cover!(true, "closed controller accepts inert lifecycle operations");
    }

    struct ReenterOnDrop {
        workers: Rc<IdleWorkers>,
        drops: Rc<Cell<u8>>,
    }

    struct RegistryPayload {
        owner: Weak<RefCell<Option<RegistryPayload>>>,
        drops: Rc<Cell<u8>>,
    }

    impl Drop for RegistryPayload {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
            let owner = self.owner.upgrade().unwrap();
            assert!(owner.borrow().is_none());
            close_registry(&owner);
        }
    }

    #[kani::proof]
    #[kani::unwind(3)]
    fn shutdown_clears_and_releases_the_registry_before_dropping_payloads() {
        let registry = Rc::new(RefCell::new(None));
        let drops = Rc::new(Cell::new(0));
        *registry.borrow_mut() = Some(RegistryPayload {
            owner: Rc::downgrade(&registry),
            drops: drops.clone(),
        });
        close_registry(&registry);
        assert!(registry.borrow().is_none());
        assert_eq!(drops.get(), 1);
        close_registry(&registry);
        assert_eq!(drops.get(), 1);
        drop(registry);
        assert_eq!(drops.get(), 1);
    }

    impl Drop for ReenterOnDrop {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
            self.workers.shutdown();
            let handle = self
                .workers
                .register(|| panic!("resurrected callback ran"))
                .unwrap();
            assert!(handle._registration.is_none());
        }
    }

    #[kani::proof]
    #[kani::unwind(3)]
    fn registration_after_shutdown_drops_payload_without_resurrection() {
        let workers = Rc::new(closed_workers());
        let drops = Rc::new(Cell::new(0));
        let guard = ReenterOnDrop {
            workers: workers.clone(),
            drops: drops.clone(),
        };
        let handle = workers
            .register(move || {
                let _ = &guard;
                panic!("closed callback ran");
            })
            .unwrap();
        assert!(handle._registration.is_none());
        assert_eq!(drops.get(), 1);
        workers.run();
        assert!(workers.registry.borrow().is_none());
        drop(handle);
        assert_eq!(drops.get(), 1);
    }
}

#[cfg(test)]
mod tests {
    //! Capacity boundaries and panic recovery use ordinary Rust execution.

    use std::{
        cell::{Cell, RefCell},
        panic::{catch_unwind, AssertUnwindSafe},
        rc::Rc,
    };

    use super::{BeforeParkCapacityError, IdleWorkers};

    #[test]
    fn fallible_registration_reserves_removed_slots_until_the_batch_ends() {
        let workers = Rc::new(IdleWorkers::default());
        let handles = Rc::new(RefCell::new(Vec::new()));
        let visited = Rc::new(Cell::new(0));
        for _ in 0..256 {
            let weak = Rc::downgrade(&workers);
            let handles_for_worker = handles.clone();
            let visited = visited.clone();
            let worker = workers
                .register(move || {
                    visited.set(visited.get() + 1);
                    if visited.get() == 1 {
                        drop(handles_for_worker.borrow_mut().pop());
                        assert_eq!(
                            weak.upgrade().unwrap().register(|| {}).unwrap_err(),
                            BeforeParkCapacityError,
                        );
                    }
                })
                .unwrap();
            handles.borrow_mut().push(worker);
        }
        assert_eq!(
            workers.register(|| {}).unwrap_err(),
            BeforeParkCapacityError
        );
        workers.run();
        assert_eq!(visited.get(), 256);
        let replacement = workers.register(|| {}).unwrap();
        assert_eq!(
            workers.register(|| {}).unwrap_err(),
            BeforeParkCapacityError
        );
        drop(replacement);
        handles.borrow_mut().clear();
    }

    #[test]
    fn recursive_run_panics_without_losing_the_batch_or_spawn_state() {
        let workers = Rc::new(IdleWorkers::default());
        let weak = Rc::downgrade(&workers);
        let handle = workers
            .register(move || weak.upgrade().unwrap().run())
            .unwrap();
        let panic = catch_unwind(AssertUnwindSafe(|| workers.run()));
        assert!(panic.is_err());
        assert!(!workers.is_running());
        drop(handle);
        workers.run();
        assert!(!workers.is_running());
    }
}
