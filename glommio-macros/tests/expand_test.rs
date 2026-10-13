//! `#[test]` composes with the standard harness because the macro emits a
//! plain `#[test] fn`. These tests prove that, including the cases where the
//! harness attributes have to survive: Result returns and should_panic.

extern crate self as glommio;

pub struct LocalExecutorBuilder;
pub struct LocalExecutor;

#[derive(Debug, PartialEq)]
pub enum Placement {
    Unbound,
    Fixed(usize),
}

impl LocalExecutorBuilder {
    pub fn new(_placement: Placement) -> Self {
        LocalExecutorBuilder
    }

    pub fn name(self, _name: &str) -> Self {
        self
    }

    #[allow(clippy::result_unit_err)]
    pub fn make(self) -> Result<LocalExecutor, ()> {
        Ok(LocalExecutor)
    }
}

impl LocalExecutor {
    pub fn run<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        futures_lite::future::block_on(future)
    }
}

#[glommio_macros::test]
async fn plain() {
    assert_eq!(1 + 1, 2);
}

#[glommio_macros::test]
async fn returning_result() -> Result<(), std::io::Error> {
    Ok(())
}

#[glommio_macros::test]
#[should_panic(expected = "boom")]
async fn panicking() {
    panic!("boom");
}

#[glommio_macros::test(placement = Fixed(0))]
async fn pinned() {}

#[glommio_macros::test]
#[ignore]
async fn ignored() {
    unreachable!("this test is ignored and must not run");
}
