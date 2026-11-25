//! Unless explicitly stated otherwise all files in this repository are licensed
//! under the MIT/Apache-2.0 License, at your convenience
//!
//! This product includes software developed at [Datadog](https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//!
use crate::{
    io::{dma_file::DmaFile, glommio_file::GlommioFile, open_options::OpenOptions},
    sys, GlommioError,
};
use std::{
    cell::Ref,
    io,
    os::unix::io::{AsRawFd, FromRawFd, RawFd},
    path::Path,
};

use crate::Result;

#[derive(Debug)]
/// A directory representation where asynchronous operations can be issued
pub struct Directory {
    file: GlommioFile,
}

impl AsRawFd for Directory {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl Directory {
    /// Try creating a clone of this Directory.
    ///
    /// The new object has a different file descriptor and has to be
    /// closed separately.
    pub fn try_clone(&self) -> Result<Directory> {
        let fd = sys::duplicate_file(self.file.as_raw_fd()).map_err(|source| {
            GlommioError::create_enhanced(
                source,
                "Cloning directory",
                self.file.path.borrow().as_ref(),
                Some(self.file.as_raw_fd()),
            )
        })?;
        let file =
            unsafe { GlommioFile::from_raw_fd(fd as _) }.with_path(self.file.path.borrow().clone());
        Ok(Directory { file })
    }

    /// Asynchronously open the directory at path
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<Directory> {
        let path = path.as_ref().to_owned();
        let flags = libc::O_DIRECTORY | libc::O_CLOEXEC;
        let reactor = crate::executor().reactor();
        let native = sys::source::NativePath::try_from(&*path).map_err(|source| {
            GlommioError::create_enhanced(source, "Opening directory", Some(&path), None)
        })?;
        let source = reactor.open_at(libc::AT_FDCWD, native, flags, 0o755);
        let fd = source.collect_fd().await.map_err(|source| {
            GlommioError::create_enhanced(source, "Opening directory", Some(&path), None)
        })?;
        let file = unsafe { GlommioFile::from_raw_fd(fd as _) }.with_path(Some(path));
        Ok(Directory { file })
    }

    /// Opens a file under this directory, returns a DMA file
    ///
    /// NOTE: Path must not contain directories and just be a file name
    pub async fn open_file<P: AsRef<Path>>(&self, path: P) -> Result<DmaFile> {
        if contains_dir(path.as_ref()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Path cannot contain directories",
            )
            .into());
        };

        DmaFile::open_with_options(
            Some(&self.file),
            path.as_ref(),
            "Opening",
            OpenOptions::new().read(true),
        )
        .await
    }

    /// Creates a directory at `path` and opens it, similar to
    /// [`std::fs::create_dir`] but returning a [`Directory`]. A directory that
    /// already exists is opened rather than reported as an error. Missing
    /// parent directories are not created.
    pub async fn create<P: AsRef<Path>>(path: P) -> Result<Directory> {
        let path = path.as_ref().to_owned();
        let source = crate::executor()
            .reactor()
            .create_dir(crate::sys::source::NativePath::try_from(&*path)?, 0o777)
            .await;

        enhanced_try!(
            match source.collect_rw().await {
                Ok(_) => Ok(()),
                Err(x) => {
                    match x.kind() {
                        std::io::ErrorKind::AlreadyExists => Ok(()),
                        _ => Err(x),
                    }
                }
            },
            "Synchronously creating directory",
            Some(&path),
            None
        )?;
        Self::open(&path).await
    }

    /// Creates a file under this directory, returns a DMA file
    ///
    /// NOTE: Path must not contain directories and just be a file name
    pub async fn create_file<P: AsRef<Path>>(&self, path: P) -> Result<DmaFile> {
        if contains_dir(path.as_ref()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Path cannot contain directories",
            )
            .into());
        }

        DmaFile::open_with_options(
            Some(&self.file),
            path.as_ref(),
            "Creating",
            OpenOptions::new().write(true).create(true).truncate(true),
        )
        .await
    }

    /// Returns an iterator to the contents of this directory
    pub fn sync_read_dir(&self) -> Result<std::fs::ReadDir> {
        let path = self.file.path_required("read directory")?;
        enhanced_try!(std::fs::read_dir(&*path), "Reading a directory", self.file)
    }

    /// Issues fdatasync into the underlying file.
    pub async fn sync(&self) -> Result<()> {
        let source = self
            .file
            .reactor
            .upgrade()
            .unwrap()
            .fdatasync(self.as_raw_fd());
        source.collect_rw().await?;
        Ok(())
    }

    /// Closes this directory.
    pub async fn close(self) -> Result<()> {
        self.file.close().await
    }

    /// Returns an `Option` containing the path associated with this open
    /// directory, or `None` if there isn't one.
    pub fn path(&self) -> Option<Ref<'_, Path>> {
        self.file.path()
    }
}

fn contains_dir(path: &Path) -> bool {
    let mut iter = path.components();
    match iter.next() {
        Some(std::path::Component::Normal(_)) => iter.next().is_some(),
        _ => true,
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// A path relative to the working directory that leads to `target`.
    fn relative_to_cwd(target: &Path) -> std::path::PathBuf {
        let cwd = std::env::current_dir().unwrap();
        let mut relative: std::path::PathBuf =
            (1..cwd.components().count()).map(|_| "..").collect();
        relative.push(target.strip_prefix("/").unwrap());
        relative
    }

    /// A cloned directory is closed on exec, like the one it came from.
    #[test]
    fn a_cloned_directory_is_closed_on_exec() {
        test_executor!(async move {
            let dir = Directory::open(std::env::temp_dir()).await.unwrap();
            let clone = dir.try_clone().unwrap();
            let flags = unsafe { libc::fcntl(clone.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0, "F_GETFD failed on the clone");
            assert_eq!(
                flags & libc::FD_CLOEXEC,
                libc::FD_CLOEXEC,
                "the clone leaks across exec"
            );
        });
    }

    #[test]
    fn open_and_create_take_relative_paths() {
        test_executor!(async move {
            Directory::open(".").await.expect("open(\".\") failed");
            let dir = std::env::temp_dir().join(format!("glommio-rel-dir-{}", std::process::id()));
            let created = Directory::create(relative_to_cwd(&dir)).await;
            let _ = std::fs::remove_dir(&dir);
            created.expect("create() with a relative path failed");
        });
    }

    /// Files are opened and created in the directory the handle holds, not in
    /// whatever its path names by the time they are asked for.
    #[test]
    fn files_go_through_the_directory_handle_after_its_path_is_replaced() {
        test_executor!(async move {
            let base =
                std::env::temp_dir().join(format!("glommio-dir-swap-{}", std::process::id()));
            let (original, moved) = (base.join("original"), base.join("moved"));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&original).unwrap();
            std::fs::write(original.join("existing"), b"opened").unwrap();

            let dir = Directory::open(&original).await.unwrap();
            std::fs::rename(&original, &moved).unwrap();
            std::fs::create_dir(&original).unwrap();
            std::fs::write(original.join("existing"), b"replacement").unwrap();

            let created = dir.create_file("created").await.unwrap();
            created.close().await.unwrap();
            let opened = dir.open_file("existing").await.unwrap();
            let opened_inode = opened.inode();
            opened.close().await.unwrap();

            let created_in_moved = moved.join("created").exists();
            let created_in_replacement = original.join("created").exists();
            let moved_inode = std::os::unix::fs::MetadataExt::ino(
                &std::fs::metadata(moved.join("existing")).unwrap(),
            );
            std::fs::remove_dir_all(&base).unwrap();

            assert!(
                created_in_moved,
                "the file was not created in the opened directory"
            );
            assert!(
                !created_in_replacement,
                "the file was created in the replacement"
            );
            assert_eq!(
                opened_inode, moved_inode,
                "the replacement's file was opened"
            );
        });
    }

    /// A file under a directory opened by a relative path still records an
    /// absolute path, so renaming it later does not depend on the working
    /// directory.
    #[test]
    fn a_file_under_a_relatively_opened_directory_records_an_absolute_path() {
        test_executor!(async move {
            let base =
                std::env::temp_dir().join(format!("glommio-rel-file-{}", std::process::id()));
            std::fs::create_dir_all(&base).unwrap();
            let dir = Directory::open(relative_to_cwd(&base)).await.unwrap();
            let file = dir.create_file("created").await.unwrap();
            let recorded = file.path().map(|p| p.to_path_buf());
            file.close().await.unwrap();
            let names_the_file = recorded.as_ref().is_some_and(|p| p.exists());
            std::fs::remove_dir_all(&base).unwrap();

            let recorded = recorded.expect("the file recorded no path");
            assert!(recorded.is_absolute(), "recorded {recorded:?}");
            assert!(names_the_file, "recorded {recorded:?}");
        });
    }

    /// A path holding an interior NUL cannot name a directory or a file in
    /// one. Opening either fails with `InvalidInput` instead of panicking,
    /// and the kind survives `?` into an `io::Error`.
    #[test]
    fn opening_a_path_with_a_nul_fails_instead_of_panicking() {
        use crate::io::glommio_file::test::kind_seen_through_question_mark;
        use std::io::ErrorKind::InvalidInput;
        test_executor!(async move {
            let opened = Directory::open("/tmp/glommio\0nul").await;
            assert_eq!(kind_seen_through_question_mark(opened), Some(InvalidInput));
            let dir = Directory::open(std::env::temp_dir()).await.unwrap();
            let file = dir.open_file("glommio\0nul").await;
            assert_eq!(kind_seen_through_question_mark(file), Some(InvalidInput));
            let created = dir.create_file("glommio\0nul").await;
            assert_eq!(kind_seen_through_question_mark(created), Some(InvalidInput));
        });
    }
}
