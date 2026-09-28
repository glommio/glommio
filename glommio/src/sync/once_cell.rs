//! A cell initialised at most once, possibly by an async initialiser.
//!
//! [`std::cell::OnceCell`] cannot help when the value must be produced by
//! something that awaits -- opening a file, resolving a name, asking a peer.
//! This one can, and it guarantees the initialiser runs once even when several
//! tasks reach the cell while initialisation is still in flight: the later
//! arrivals wait for the first rather than starting their own.
//!
//! # Examples
//!
//! ```
//! use glommio::{sync::OnceCell, LocalExecutor};
//!
//! let ex = LocalExecutor::default();
//! ex.run(async {
//!     let cell = OnceCell::new();
//!     let value = cell.get_or_init(|| async { 42 }).await;
//!     assert_eq!(*value, 42);
//! });
//! ```

use super::Semaphore;
use std::{
    cell::{Cell, UnsafeCell},
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// A cell holding a value that is initialised at most once.
pub struct OnceCell<T> {
    value: UnsafeCell<Option<T>>,
    /// Held for the duration of an initialisation, so a second caller waits
    /// for the first instead of running its own initialiser.
    initialising: Semaphore,
    /// True only while the initialiser is *being polled*, which on a
    /// single-threaded executor is exactly the window in which no other task
    /// can be running. A call arriving while this is set therefore comes from
    /// the initialiser's own call stack, and waiting for the permit would be
    /// waiting for itself.
    ///
    /// It is deliberately not set while the initialiser is merely *suspended*:
    /// another task reaching the cell then is the case this type exists to
    /// serve, and it must queue rather than panic.
    in_initialiser: Cell<bool>,
}

/// Polls an initialiser while marking its cell as "on the call stack".
///
/// The flag is raised for the duration of each `poll` and lowered on the way
/// out, including when the initialiser panics, so an abandoned initialisation
/// cannot leave the cell permanently un-enterable.
struct Guarded<'a, Fut> {
    inner: Fut,
    flag: &'a Cell<bool>,
}

impl<Fut: Future> Future for Guarded<'_, Fut> {
    type Output = Fut::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Fut::Output> {
        // Safety: `inner` is pinned structurally and never moved out of, and
        // `flag` is a shared reference that needs no pinning.
        let this = unsafe { self.get_unchecked_mut() };
        let flag = this.flag;
        let inner = unsafe { Pin::new_unchecked(&mut this.inner) };

        flag.set(true);
        let _lower = scopeguard::guard((), |()| flag.set(false));
        inner.poll(cx)
    }
}

/// Polls `inner` while refusing to proceed from inside this cell's own
/// initialiser.
///
/// Checked on every poll rather than once on entry. A future can be created
/// outside the initialiser, where the check passes because the flag is still
/// clear, and then handed in and awaited inside it.
struct RefusingReentry<'a, Fut> {
    inner: Fut,
    flag: &'a Cell<bool>,
}

impl<Fut: Future> Future for RefusingReentry<'_, Fut> {
    type Output = Fut::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Fut::Output> {
        // Safety: as in `Guarded`.
        let this = unsafe { self.get_unchecked_mut() };
        assert!(
            !this.flag.get(),
            "OnceCell initialiser re-entered its own cell; it would wait for itself forever"
        );
        unsafe { Pin::new_unchecked(&mut this.inner) }.poll(cx)
    }
}

impl<T> OnceCell<T> {
    /// Creates an empty cell.
    pub fn new() -> Self {
        OnceCell {
            value: UnsafeCell::new(None),
            initialising: Semaphore::new(1),
            in_initialiser: Cell::new(false),
        }
    }

    /// Returns the value, or `None` if the cell is still empty.
    pub fn get(&self) -> Option<&T> {
        // Safety: the value is written at most once and is never replaced, so
        // a shared reference handed out here cannot be invalidated while it
        // lives. `take` does remove it, but it needs `&mut self`, which the
        // borrow checker will not hand out while the reference returned here
        // is alive. Every writer checks the cell is empty
        // and writes without awaiting in between, which on a single-threaded
        // executor is what makes "at most once" true.
        unsafe { (*self.value.get()).as_ref() }
    }

    /// Returns whether the cell holds a value.
    pub fn is_initialized(&self) -> bool {
        self.get().is_some()
    }

    /// Sets the value if the cell is empty and nothing is initialising it.
    ///
    /// # Errors
    ///
    /// Hands `value` back in two cases: the cell already holds one, or a
    /// [`get_or_init`](Self::get_or_init) is in flight and suspended in its
    /// initialiser. Overwriting the second would leave that caller waiting for
    /// a computation whose result is then thrown away, so it is reported
    /// instead. `tokio::sync::OnceCell` draws the same line, with a richer
    /// error that names which of the two happened.
    ///
    /// The two are not distinguished here because [`std::cell::OnceCell::set`]
    /// returns `Result<(), T>` and this keeps that shape. Ask
    /// [`is_initialized`](Self::is_initialized) if the difference matters.
    pub fn set(&self, value: T) -> Result<(), T> {
        // The permit is what makes this safe against an initialiser, and
        // taking it fails in exactly the two cases where the value is not this
        // caller's to set: another task holds it, so a `get_or_init` is
        // suspended mid-await and will publish when it wakes; or the semaphore
        // is closed, so a value already landed.
        let Ok(_permit) = self.initialising.try_acquire_permit(1) else {
            return Err(value);
        };

        // Safety: as in `get`. The permit is held, so no initialiser can be
        // between its emptiness check and its write.
        unsafe { *self.value.get() = Some(value) };
        self.initialising.close();
        Ok(())
    }

    /// Returns a mutable reference to the value, or `None` if the cell is
    /// still empty.
    ///
    /// Taking `&mut self` proves no shared reference from [`get`] is alive, so
    /// this needs no `unsafe`.
    ///
    /// [`get`]: Self::get
    pub fn get_mut(&mut self) -> Option<&mut T> {
        self.value.get_mut().as_mut()
    }

    /// Takes the value out, leaving the cell empty and reusable.
    ///
    /// Returns `None` if the cell was empty. As with [`get_mut`], `&mut self`
    /// is what makes removing the value sound.
    ///
    /// [`get_mut`]: Self::get_mut
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::{sync::OnceCell, LocalExecutor};
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let mut cell = OnceCell::new();
    ///     cell.set(1).unwrap();
    ///     assert_eq!(cell.take(), Some(1));
    ///     assert_eq!(cell.take(), None);
    ///     // Empty again, so it can be initialised afresh.
    ///     assert_eq!(*cell.get_or_init(|| async { 2 }).await, 2);
    /// });
    /// ```
    pub fn take(&mut self) -> Option<T> {
        // Swapped for a fresh cell rather than emptied in place: the semaphore
        // was closed when the value landed, and an emptied cell has to be
        // initialisable again.
        std::mem::take(self).value.into_inner()
    }

    /// Consumes the cell and returns the value it holds, if any.
    pub fn into_inner(self) -> Option<T> {
        self.value.into_inner()
    }

    /// Returns the value, running a fallible `init` to produce it if the cell
    /// is empty.
    ///
    /// **A failed initialiser does not poison the cell.** Lazily-initialised
    /// resources are usually fallible -- connecting to a catalog, opening a
    /// file -- and a transient failure should leave the next caller free to
    /// try again. A caller queued behind one that failed runs its own
    /// initialiser rather than inheriting the error.
    ///
    /// # Examples
    ///
    /// ```
    /// use glommio::{sync::OnceCell, LocalExecutor};
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let cell = OnceCell::new();
    ///     let value = cell
    ///         .get_or_try_init(|| async { Ok::<_, std::io::Error>(42) })
    ///         .await
    ///         .unwrap();
    ///     assert_eq!(*value, 42);
    /// });
    /// ```
    pub async fn get_or_try_init<F, Fut, E>(&self, init: F) -> Result<&T, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        if let Some(value) = self.get() {
            return Ok(value);
        }

        let Ok(_permit) = (RefusingReentry {
            inner: self.initialising.acquire_permit(1),
            flag: &self.in_initialiser,
        })
        .await
        else {
            // Closed means a previous initialiser published and released every
            // waiter at once, rather than handing the permit down the queue.
            return Ok(self
                .get()
                .expect("the semaphore is closed only after a value lands"));
        };

        // Re-checked under the permit: whoever held it before may have
        // succeeded, in which case there is nothing to do -- or failed, in
        // which case the cell is still empty and this caller tries.
        if self.get().is_none() {
            // Armed across the call to `init`, not only across the future it
            // hands back: a closure is free to do its re-entering before
            // returning anything there is to poll.
            let building = {
                self.in_initialiser.set(true);
                let _armed = scopeguard::guard((), |()| self.in_initialiser.set(false));
                init()
            };
            let value = Guarded {
                inner: building,
                flag: &self.in_initialiser,
            }
            .await?;
            // Safety: as in `get`. Nothing else can publish while this
            // permit is held, which is what `set` taking it too buys.
            unsafe { *self.value.get() = Some(value) };
            // Releases everyone queued behind this initialisation in one pass.
            // A failed initialiser does not get here, so the next caller still
            // takes the permit and runs its own.
            self.initialising.close();
        }

        Ok(self.get().expect("the cell was just initialised"))
    }

    /// Returns the value, running `init` to produce it if the cell is empty.
    ///
    /// If another task is already initialising the cell, this waits for that
    /// one to finish rather than running `init`, so however many callers
    /// arrive only one initialiser runs at a time. That is not the same as
    /// running once: an initialiser that panics or is dropped part-way leaves
    /// the cell empty, and the next caller runs its own.
    ///
    /// # Panics
    ///
    /// If called from inside this cell's own initialiser. See
    /// [`get_or_try_init`](Self::get_or_try_init).
    ///
    /// Reaching the cell from a task the initialiser *spawned* is not that
    /// case and is not detected: the initialiser is suspended awaiting the
    /// spawned task, which looks like any other caller queueing behind it, and
    /// both then wait forever.
    ///
    /// ```no_run
    /// # use glommio::{sync::OnceCell, LocalExecutor};
    /// # use std::rc::Rc;
    /// # let ex = LocalExecutor::default();
    /// # ex.run(async {
    /// let cell: Rc<OnceCell<u32>> = Rc::new(OnceCell::new());
    /// let interior = cell.clone();
    /// // Deadlocks: the spawned task queues behind an initialiser that is
    /// // itself waiting for that task.
    /// cell.get_or_init(|| async move {
    ///     glommio::spawn_local(async move { *interior.get_or_init(|| async { 42 }).await })
    ///         .await
    /// })
    /// .await;
    /// # });
    /// ```
    pub async fn get_or_init<F, Fut>(&self, init: F) -> &T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        // The infallible case is the fallible one whose initialiser cannot
        // fail, which is how `std` builds it too: one path to audit rather
        // than two that have to agree.
        self.get_or_try_init(|| async { Ok::<T, std::convert::Infallible>(init().await) })
            .await
            .unwrap()
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for OnceCell<T> {
    /// Prints the value the cell holds, not the machinery that guards its
    /// initialisation.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut tuple = f.debug_tuple("OnceCell");
        match self.get() {
            Some(value) => tuple.field(value),
            None => tuple.field(&format_args!("<uninit>")),
        };
        tuple.finish()
    }
}

impl<T: Clone> Clone for OnceCell<T> {
    /// Clones the value, if there is one, into a fresh cell.
    ///
    /// The semaphore is not shared: the clone gets its own, so an
    /// initialisation in flight on the original does not block the copy.
    ///
    /// `&self` is enough to clone, so this can run while an initialiser is
    /// suspended. An unpublished value is not in the cell yet, so the clone
    /// comes out empty and is free to initialise itself. Only a published
    /// value is copied.
    fn clone(&self) -> Self {
        match self.get() {
            Some(value) => OnceCell::from(value.clone()),
            None => OnceCell::new(),
        }
    }
}

impl<T: PartialEq> PartialEq for OnceCell<T> {
    /// Compares the values, so two empty cells are equal and an empty cell
    /// never equals a full one.
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl<T: Eq> Eq for OnceCell<T> {}

impl<T> From<T> for OnceCell<T> {
    /// Builds a cell already holding `value`.
    fn from(value: T) -> Self {
        let cell = OnceCell {
            value: UnsafeCell::new(Some(value)),
            initialising: Semaphore::new(1),
            in_initialiser: Cell::new(false),
        };
        // Closed because the cell arrives initialised: an open semaphore would
        // let `set` take a permit and overwrite the value it was built with.
        cell.initialising.close();
        cell
    }
}

impl<T> Default for OnceCell<T> {
    fn default() -> Self {
        OnceCell::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{timer::Timer, LocalExecutor};
    use std::{cell::RefCell, rc::Rc, time::Duration};

    #[test]
    fn an_empty_cell_has_nothing_in_it() {
        let cell: OnceCell<u32> = OnceCell::new();
        assert!(cell.get().is_none());
        assert!(!cell.is_initialized());
    }

    #[test]
    fn set_stores_a_value_once() {
        let cell = OnceCell::new();
        assert!(cell.set(1).is_ok());
        assert_eq!(cell.get(), Some(&1));
        assert!(cell.is_initialized());
        assert_eq!(
            cell.set(2),
            Err(2),
            "a second set should hand the value back"
        );
        assert_eq!(cell.get(), Some(&1), "the first value must survive");
    }

    #[test]
    fn get_or_init_runs_the_initialiser_once() {
        LocalExecutor::default().run(async {
            let cell = OnceCell::new();
            let runs = Rc::new(RefCell::new(0));

            for _ in 0..3 {
                let value = cell
                    .get_or_init(|| {
                        let runs = runs.clone();
                        async move {
                            *runs.borrow_mut() += 1;
                            41
                        }
                    })
                    .await;
                assert_eq!(*value, 41);
            }

            assert_eq!(*runs.borrow(), 1, "the initialiser ran more than once");
        });
    }

    #[test]
    fn get_or_try_init_stores_a_successful_value() {
        LocalExecutor::default().run(async {
            let cell = OnceCell::new();
            let value = cell
                .get_or_try_init(|| async { Ok::<_, String>(3) })
                .await
                .unwrap();
            assert_eq!(*value, 3);
            assert!(cell.is_initialized());
        });
    }

    #[test]
    fn a_failed_get_or_try_init_leaves_the_cell_empty() {
        LocalExecutor::default().run(async {
            let cell: OnceCell<u32> = OnceCell::new();
            let err = cell
                .get_or_try_init(|| async { Err::<u32, _>("catalog unreachable") })
                .await
                .unwrap_err();

            assert_eq!(err, "catalog unreachable");
            assert!(
                !cell.is_initialized(),
                "a failed initialiser must not poison the cell"
            );
            assert!(cell.get().is_none());
        });
    }

    #[test]
    fn a_cell_can_be_retried_after_a_failure() {
        LocalExecutor::default().run(async {
            let cell = OnceCell::new();
            let attempts = Rc::new(RefCell::new(0));

            for _ in 0..2 {
                let _ = cell
                    .get_or_try_init(|| {
                        let attempts = attempts.clone();
                        async move {
                            *attempts.borrow_mut() += 1;
                            Err::<u32, _>("still down")
                        }
                    })
                    .await;
            }

            let value = cell
                .get_or_try_init(|| async { Ok::<_, &str>(7) })
                .await
                .unwrap();

            assert_eq!(*value, 7);
            assert_eq!(*attempts.borrow(), 2, "each failure should have retried");
        });
    }

    #[test]
    fn a_caller_waiting_behind_a_failed_init_runs_its_own() {
        LocalExecutor::default().run(async {
            let cell = Rc::new(OnceCell::new());
            let order = Rc::new(RefCell::new(Vec::new()));

            // The first initialiser suspends, then fails, so the second is
            // queued behind it and must not inherit the failure.
            let failing = crate::spawn_local({
                let cell = cell.clone();
                let order = order.clone();
                async move {
                    cell.get_or_try_init(|| {
                        let order = order.clone();
                        async move {
                            Timer::new(Duration::from_millis(20)).await;
                            order.borrow_mut().push("first failed");
                            Err::<u32, _>("no")
                        }
                    })
                    .await
                    .is_err()
                }
            })
            .detach();

            Timer::new(Duration::from_millis(5)).await;

            let second = cell
                .get_or_try_init(|| {
                    let order = order.clone();
                    async move {
                        order.borrow_mut().push("second ran");
                        Ok::<_, &str>(11)
                    }
                })
                .await;

            assert!(failing.await.unwrap());
            assert_eq!(*second.unwrap(), 11);
            assert_eq!(*order.borrow(), vec!["first failed", "second ran"]);
        });
    }

    #[test]
    fn a_concurrent_get_or_init_waits_rather_than_initialising_again() {
        LocalExecutor::default().run(async {
            let cell = Rc::new(OnceCell::new());
            let runs = Rc::new(RefCell::new(0));

            // The first initialiser suspends, so the second arrives while
            // initialisation is still in flight.
            let slow = crate::spawn_local({
                let cell = cell.clone();
                let runs = runs.clone();
                async move {
                    *cell
                        .get_or_init(|| {
                            let runs = runs.clone();
                            async move {
                                Timer::new(Duration::from_millis(20)).await;
                                *runs.borrow_mut() += 1;
                                7
                            }
                        })
                        .await
                }
            })
            .detach();

            Timer::new(Duration::from_millis(5)).await;

            let second = *cell
                .get_or_init(|| {
                    let runs = runs.clone();
                    async move {
                        *runs.borrow_mut() += 1;
                        99
                    }
                })
                .await;

            assert_eq!(slow.await.unwrap(), 7);
            assert_eq!(second, 7, "the second caller should see the first value");
            assert_eq!(*runs.borrow(), 1, "the initialiser ran more than once");
        });
    }
}

#[cfg(test)]
mod write_once {
    use super::*;
    use crate::{timer::sleep, LocalExecutor};
    use std::{rc::Rc, time::Duration};

    /// A `set` that wins the race is what every later caller sees.
    ///
    /// The mirror of the test below: the permit makes the two orderings
    /// symmetrical, so whichever gets there first is the value, and the loser
    /// is told rather than silently discarded.
    #[test]
    fn an_initialiser_arriving_after_set_returns_the_value_that_is_there() {
        LocalExecutor::default().run(async {
            let cell: OnceCell<u32> = OnceCell::new();

            assert_eq!(cell.set(1), Ok(()));

            let mut ran = false;
            let value = cell
                .get_or_init(|| {
                    ran = true;
                    async { 2 }
                })
                .await;

            assert_eq!(*value, 1, "the value that was set stands");
            assert!(!ran, "and the initialiser was never run");
        });
    }

    /// `set` is refused while an initialiser holds the permit.
    ///
    /// It used to succeed, and the initialiser then threw its own value away
    /// when it woke: the caller of `get_or_init` waited for a computation
    /// whose result was discarded, which is a surprising thing to do quietly.
    /// `tokio::sync::OnceCell` reports this instead, and so does this now.
    ///
    /// The permit is also what makes the write safe. `get` hands out `&T`, so
    /// a second publication would have to either replace a value a reader
    /// already holds a reference to, or be dropped; holding the permit across
    /// the await means neither can arise.
    #[test]
    fn set_is_refused_while_an_initialiser_holds_the_permit() {
        LocalExecutor::default().run(async {
            let cell: Rc<OnceCell<u32>> = Rc::new(OnceCell::new());

            let initialiser = crate::spawn_local({
                let cell = cell.clone();
                async move {
                    *cell
                        .get_or_init(|| async {
                            sleep(Duration::from_millis(20)).await;
                            2
                        })
                        .await
                }
            });

            // Let the initialiser take the permit and park inside `init`.
            sleep(Duration::from_millis(5)).await;

            assert_eq!(
                cell.set(1),
                Err(1),
                "an initialiser is mid-flight, so the value is not ours to set"
            );
            assert_eq!(cell.get(), None, "and nothing was published");

            assert_eq!(initialiser.await, 2, "the initialiser's own value stands");
            assert_eq!(cell.get(), Some(&2));

            assert_eq!(
                cell.set(3),
                Err(3),
                "and a set after the fact is refused as well"
            );
        });
    }

    #[test]
    fn an_initialiser_still_wins_when_nothing_races_it() {
        LocalExecutor::default().run(async {
            let cell: OnceCell<u32> = OnceCell::new();
            assert_eq!(*cell.get_or_init(|| async { 7 }).await, 7);
            assert_eq!(cell.set(9), Err(9), "already initialised");
            assert_eq!(cell.get(), Some(&7));
        });
    }
}

#[cfg(test)]
mod parity_tests {
    use super::*;
    use crate::LocalExecutor;

    #[test]
    fn get_mut_reaches_the_value() {
        let mut cell = OnceCell::new();
        assert!(cell.get_mut().is_none(), "empty cell has nothing to borrow");
        cell.set(1u32).unwrap();
        *cell.get_mut().unwrap() += 1;
        assert_eq!(cell.get(), Some(&2));
    }

    /// `take` is the one method that removes a value, which is what the safety
    /// argument in `get` now has to account for. It is sound because `&mut
    /// self` excludes any live shared reference.
    #[test]
    fn take_empties_the_cell_and_leaves_it_reusable() {
        LocalExecutor::default().run(async {
            let mut cell = OnceCell::new();
            cell.set(1u32).unwrap();

            assert_eq!(cell.take(), Some(1));
            assert_eq!(cell.take(), None, "taking twice yields nothing");
            assert!(!cell.is_initialized());

            assert_eq!(
                *cell.get_or_init(|| async { 2 }).await,
                2,
                "an emptied cell initialises again"
            );
        });
    }

    #[test]
    fn into_inner_yields_the_value() {
        let cell = OnceCell::new();
        cell.set(9u32).unwrap();
        assert_eq!(cell.into_inner(), Some(9));
        assert_eq!(OnceCell::<u32>::new().into_inner(), None);
    }

    #[test]
    fn from_starts_initialised() {
        let cell = OnceCell::from(4u32);
        assert!(cell.is_initialized());
        assert_eq!(cell.get(), Some(&4));
        assert_eq!(
            cell.set(5),
            Err(5),
            "an initialised cell refuses a second value"
        );
    }

    /// Taking the value while an initialiser is suspended is impossible by
    /// construction: `get_or_init` borrows the cell shared, so no `&mut self`
    /// can exist at the same time. This records that the emptied cell's
    /// machinery is left in a usable state rather than a half-held one.
    #[test]
    fn a_taken_cell_still_serialises_its_initialisers() {
        LocalExecutor::default().run(async {
            let mut cell = OnceCell::new();
            cell.set(1u32).unwrap();
            cell.take();

            let runs = std::rc::Rc::new(Cell::new(0));
            for _ in 0..3 {
                let r = runs.clone();
                cell.get_or_init(|| async move {
                    r.set(r.get() + 1);
                    7u32
                })
                .await;
            }
            assert_eq!(runs.get(), 1, "the initialiser still runs exactly once");
            assert_eq!(cell.get(), Some(&7));
        });
    }
}

#[cfg(test)]
mod std_trait_tests {
    use super::*;
    use crate::{timer::sleep, LocalExecutor};
    use std::{rc::Rc, time::Duration};

    #[test]
    fn debug_shows_the_value() {
        assert_eq!(format!("{:?}", OnceCell::from(42u32)), "OnceCell(42)");
        assert_eq!(
            format!("{:?}", OnceCell::<u32>::new()),
            "OnceCell(<uninit>)"
        );

        let std_cell = std::cell::OnceCell::from(42u32);
        assert_eq!(
            format!("{:?}", OnceCell::from(42u32)),
            format!("{std_cell:?}"),
            "the full case matches std"
        );
    }

    #[test]
    fn clone_copies_a_published_value() {
        let cell = OnceCell::from(String::from("hello"));
        let copy = cell.clone();
        assert_eq!(copy.get().map(String::as_str), Some("hello"));
        assert_eq!(
            OnceCell::<String>::new().clone().get(),
            None,
            "an empty cell clones empty"
        );
    }

    /// `Clone` takes `&self`, so it can run while an initialiser is suspended.
    /// The value is not published yet, so the clone is empty and initialises
    /// itself independently rather than blocking on the original's semaphore.
    #[test]
    fn a_clone_taken_mid_initialisation_is_empty_and_independent() {
        LocalExecutor::default().run(async {
            let cell: Rc<OnceCell<u32>> = Rc::new(OnceCell::new());

            let initialising = crate::spawn_local({
                let cell = cell.clone();
                async move {
                    *cell
                        .get_or_init(|| async {
                            sleep(Duration::from_millis(20)).await;
                            1u32
                        })
                        .await
                }
            });

            let cloner = crate::spawn_local({
                let cell = cell.clone();
                async move {
                    sleep(Duration::from_millis(5)).await;
                    // Explicit: `cell.clone()` would clone the `Rc`, not the cell.
                    let copy = OnceCell::clone(&cell);
                    assert_eq!(
                        copy.get(),
                        None,
                        "nothing published yet, so the clone is empty"
                    );
                    // Its own semaphore, so this does not queue behind the original.
                    *copy.get_or_init(|| async { 2u32 }).await
                }
            });

            assert_eq!(initialising.await, 1);
            assert_eq!(cloner.await, 2, "the clone initialised itself");
            assert_eq!(cell.get(), Some(&1), "the original is unaffected");
        });
    }

    #[test]
    fn eq_compares_values_not_identity() {
        assert_eq!(OnceCell::from(1u32), OnceCell::from(1u32));
        assert_ne!(OnceCell::from(1u32), OnceCell::from(2u32));
        assert_eq!(OnceCell::<u32>::new(), OnceCell::<u32>::new());
        assert_ne!(OnceCell::from(1u32), OnceCell::<u32>::new());
    }
}

#[cfg(test)]
mod reentrancy_tests {
    use super::*;
    use crate::{timer::sleep, LocalExecutor};
    use std::{cell::RefCell, future::poll_fn, rc::Rc, task::Waker, time::Duration};

    /// Re-entering from the initialiser's own call stack is the case that can
    /// never make progress, so it panics rather than waiting for itself.
    #[test]
    #[should_panic(expected = "re-entered its own cell")]
    fn a_reentrant_initialiser_panics() {
        LocalExecutor::default().run(async {
            let cell: Rc<OnceCell<u32>> = Rc::new(OnceCell::new());
            let inner = cell.clone();
            cell.get_or_init(|| async move {
                // Would queue behind the permit this very call already holds.
                *inner.get_or_init(|| async { 1 }).await
            })
            .await;
        });
    }

    /// The case the type exists for: a second task arrives while the
    /// initialiser is suspended. It must wait, not panic, and the initialiser
    /// must still run exactly once.
    #[test]
    fn a_second_task_waits_rather_than_panicking() {
        LocalExecutor::default().run(async {
            let cell: Rc<OnceCell<u32>> = Rc::new(OnceCell::new());
            let runs = Rc::new(Cell::new(0));

            let first = crate::spawn_local({
                let (cell, runs) = (cell.clone(), runs.clone());
                async move {
                    *cell
                        .get_or_init(|| async {
                            runs.set(runs.get() + 1);
                            // Suspends inside the initialiser, which is when
                            // the other task reaches the cell.
                            sleep(Duration::from_millis(20)).await;
                            7u32
                        })
                        .await
                }
            });

            let second = crate::spawn_local({
                let (cell, runs) = (cell.clone(), runs.clone());
                async move {
                    sleep(Duration::from_millis(5)).await;
                    *cell
                        .get_or_init(|| async {
                            runs.set(runs.get() + 1);
                            99u32
                        })
                        .await
                }
            });

            assert_eq!(first.await, 7, "the first task's value wins");
            assert_eq!(second.await, 7, "the second task sees the same value");
            assert_eq!(runs.get(), 1, "the initialiser ran exactly once");
        });
    }

    /// A panicking initialiser must lower the flag on its way out, or the cell
    /// would refuse every later caller.
    #[test]
    fn a_panicking_initialiser_leaves_the_cell_usable() {
        LocalExecutor::default().run(async {
            let cell: OnceCell<u32> = OnceCell::new();

            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                futures_lite::future::block_on(
                    cell.get_or_init(|| async { panic!("initialiser gave up") }),
                );
            }));
            assert!(panicked.is_err(), "the initialiser's panic propagated");

            assert!(
                !cell.in_initialiser.get(),
                "the flag was lowered despite the panic"
            );
            assert_eq!(*cell.get_or_init(|| async { 5 }).await, 5);
        });
    }

    /// The fallible path takes the same guard.
    #[test]
    #[should_panic(expected = "re-entered its own cell")]
    fn a_reentrant_try_initialiser_panics() {
        LocalExecutor::default().run(async {
            let cell: Rc<OnceCell<u32>> = Rc::new(OnceCell::new());
            let inner = cell.clone();
            let _ = cell
                .get_or_try_init(|| async move {
                    inner
                        .get_or_try_init(|| async { Ok::<_, ()>(1) })
                        .await
                        .copied()
                })
                .await;
        });
    }

    /// The flag has to be raised across the call to `init`, not only across
    /// the future it returns: a closure can re-enter synchronously, before
    /// there is anything to poll.
    #[test]
    #[should_panic(expected = "re-entered its own cell")]
    fn an_initialiser_that_reenters_before_returning_a_future_panics() {
        let cell = OnceCell::<u32>::new();
        let mut outer = Box::pin(cell.get_or_try_init(|| {
            let mut inner = Box::pin(cell.get_or_init(|| async { 1 }));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(inner.as_mut().poll(&mut cx).is_pending());
            async move { Ok::<_, ()>(*inner.await) }
        }));

        let mut cx = Context::from_waker(Waker::noop());
        assert!(outer.as_mut().poll(&mut cx).is_pending());
        assert!(cell.get().is_none());
    }

    /// Checking once on entry is not enough. This waiter is created and polled
    /// while the flag is still clear, and only then handed to the initialiser
    /// to await, so the check has to run on every poll.
    #[test]
    #[should_panic(expected = "re-entered its own cell")]
    fn a_waiter_polled_before_entry_panics_when_awaited_inside_it() {
        type Waiting<'a> = Pin<Box<dyn Future<Output = &'a u32> + 'a>>;

        let cell = OnceCell::<u32>::new();
        let slot: RefCell<Option<Waiting<'_>>> = RefCell::new(None);
        let mut outer = Box::pin(cell.get_or_init(|| async {
            let inner = poll_fn(|_| match slot.borrow_mut().take() {
                Some(inner) => Poll::Ready(inner),
                None => Poll::Pending,
            })
            .await;
            *inner.await
        }));

        let mut cx = Context::from_waker(Waker::noop());
        assert!(outer.as_mut().poll(&mut cx).is_pending());

        let mut inner: Waiting<'_> = Box::pin(cell.get_or_init(|| async { 2 }));
        assert!(inner.as_mut().poll(&mut cx).is_pending());
        *slot.borrow_mut() = Some(inner);

        assert!(outer.as_mut().poll(&mut cx).is_pending());
        assert!(cell.get().is_none());
    }
}
