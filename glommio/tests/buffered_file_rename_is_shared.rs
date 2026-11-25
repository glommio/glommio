//! `BufferedFile::rename` takes `&self`, like the other file types, so a file
//! shared through an `Rc` can still be renamed.

use glommio::{io::BufferedFile, LocalExecutorBuilder};
use std::rc::Rc;

#[test]
fn a_buffered_file_behind_an_rc_can_be_renamed() {
    let dir = std::env::temp_dir().join(format!("glommio-bufrename-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch directory");
    let (before, after) = (dir.join("before"), dir.join("after"));

    LocalExecutorBuilder::default()
        .spawn({
            let (before, after) = (before.clone(), after.clone());
            move || async move {
                let file = Rc::new(BufferedFile::create(&before).await.expect("create"));
                let shared: &BufferedFile = &file;
                shared.rename(&after).await.expect("rename");
                assert_eq!(&*file.path().unwrap(), after.as_path());
                Rc::into_inner(file).unwrap().close().await.expect("close");
            }
        })
        .unwrap()
        .join()
        .unwrap();

    assert!(!before.exists(), "the old name is gone");
    assert!(after.exists(), "the new name is there");
    let _ = std::fs::remove_dir_all(&dir);
}
