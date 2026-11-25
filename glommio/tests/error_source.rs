//! A `GlommioError` from a failed operation exposes the `io::Error` it wraps
//! through `Error::source()`, so error reporters that walk the chain reach the
//! errno.

use glommio::{io::BufferedFile, LocalExecutorBuilder};
use std::{error::Error, io};

#[test]
fn a_failed_open_reports_its_io_error_as_the_source() {
    let missing = std::env::temp_dir().join(format!(
        "glommio-error-source-{}/absent",
        std::process::id()
    ));

    LocalExecutorBuilder::default()
        .spawn(move || async move {
            let err = BufferedFile::open(&missing)
                .await
                .expect_err("opening a file in a missing directory must fail");
            let source = err
                .source()
                .expect("the error has no source")
                .downcast_ref::<io::Error>()
                .expect("the source is not an io::Error");
            assert_eq!(source.kind(), io::ErrorKind::NotFound);
        })
        .unwrap()
        .join()
        .unwrap();
}
