//! Additions to `glommio::sync` that a dependent crate drives from outside.

use glommio::sync::Semaphore;
use std::rc::Rc;

#[test]
fn a_semaphore_tells_whether_it_is_closed() {
    let sem = Rc::new(Semaphore::new(1));
    assert!(!sem.is_closed());
    let permit = sem.try_acquire_static_permit(1).unwrap();
    permit.close();
    assert!(sem.is_closed());
    assert!(sem.try_acquire(1).is_err());
}

#[test]
fn a_gate_has_a_default() {
    fn make<T: Default>() -> T {
        T::default()
    }
    let gate: glommio::sync::Gate = make();
    assert!(gate.is_open());
}
