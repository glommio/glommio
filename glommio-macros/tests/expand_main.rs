//! The expansion names `::glommio` paths, so these tests provide a stub crate
//! of that name rather than depending on the real runtime — which would make
//! the dependency circular.

extern crate self as glommio;

pub struct LocalExecutorBuilder(Placement);
pub struct LocalExecutor;

#[derive(Debug, PartialEq)]
pub enum Placement {
    Unbound,
    Fixed(usize),
}

impl LocalExecutorBuilder {
    pub fn new(placement: Placement) -> Self {
        NAME.with(|n| *n.borrow_mut() = None);
        LocalExecutorBuilder(placement)
    }

    pub fn name(self, name: &str) -> Self {
        NAME.with(|n| *n.borrow_mut() = Some(name.to_string()));
        self
    }

    #[allow(clippy::result_unit_err)]
    pub fn make(self) -> Result<LocalExecutor, ()> {
        PLACEMENT.with(|p| *p.borrow_mut() = Some(self.0));
        Ok(LocalExecutor)
    }
}

impl LocalExecutor {
    pub fn run<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        futures_lite::future::block_on(future)
    }
}

thread_local! {
    static PLACEMENT: std::cell::RefCell<Option<Placement>> =
        const { std::cell::RefCell::new(None) };
    static NAME: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

#[glommio_macros::main]
async fn runs_the_body() -> u32 {
    7
}

#[test]
fn main_runs_the_body_and_defaults_to_unbound() {
    assert_eq!(runs_the_body(), 7);
    PLACEMENT.with(|p| assert_eq!(*p.borrow(), Some(Placement::Unbound)));
}

#[glommio_macros::main(placement = Fixed(3))]
async fn pinned() {}

#[glommio_macros::main(placement = Fixed(1))]
async fn pinned_again() {}

#[test]
fn placement_argument_reaches_the_builder() {
    pinned();
    PLACEMENT.with(|p| assert_eq!(*p.borrow(), Some(Placement::Fixed(3))));
    NAME.with(|n| assert_eq!(*n.borrow(), None));
}

#[test]
fn second_placement_reaches_the_builder() {
    pinned_again();
    PLACEMENT.with(|p| assert_eq!(*p.borrow(), Some(Placement::Fixed(1))));
    // The expansion never calls `.name(..)`: `make()` cannot honour it, so the
    // attribute rejects the argument outright rather than dropping it here.
    NAME.with(|n| assert_eq!(*n.borrow(), None));
}
