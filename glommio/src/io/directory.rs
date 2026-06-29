// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
// This product includes software developed at Datadog (https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//
use crate::{
    io::{dma_file::DmaFile, glommio_file::GlommioFile},
    sys::{self, SourceType},
    GlommioError,
};
use std::{
    cell::Ref,
    ffi::{CStr, OsStr},
    io,
    os::unix::{
        ffi::OsStrExt,
        io::{AsRawFd, FromRawFd, RawFd},
    },
    path::Path,
};

type Result<T> = crate::Result<T, ()>;

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

const DIR_ENTRY_MIN_SIZE: usize = 18;

/// Represents a readonly view into a dirent64
///
///     struct linux_dirent64 {
///         ino64_t        d_ino;    /* 64-bit inode number */           off=0
///         off64_t        d_off;    /* Not an offset; see getdents() */ off=8
///         unsigned short d_reclen; /* Size of this dirent */           off=16
///         unsigned char  d_type;   /* File type */                     off=18
///         char           d_name[]; /* Filename (null-terminated) */    off=19
///     };
#[derive(Debug)]
pub struct DirEntry64<'a> {
    buf: &'a [u8],
}

impl<'a> DirEntry64<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        DirEntry64 { buf }
    }

    pub fn d_ino(&self) -> u64 {
        u64::from_ne_bytes(self.buf[..8].try_into().unwrap())
    }

    pub fn d_type(&self) -> u8 {
        self.buf[18]
    }

    pub fn d_reclen(&self) -> usize {
        u16::from_ne_bytes(self.buf[16..18].try_into().unwrap()) as usize
    }

    pub fn d_name(&self) -> std::result::Result<&'a OsStr, std::ffi::FromBytesUntilNulError> {
        // According to the getdents64(2) manual, d_name is null terminated.
        // Thus, we construct an intermediate CStr as a validation step.
        let c_str = CStr::from_bytes_until_nul(&self.buf[19..])?;
        Ok(OsStr::from_bytes(c_str.to_bytes()))
    }
}

#[derive(Debug)]
pub struct DirEntryIter<'a> {
    buf: &'a [u8],
    i: usize,
}

/// An iterator over a list of dentries returned by getdents64
impl<'a> Iterator for DirEntryIter<'a> {
    type Item = DirEntry64<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let dentry_slice = self.buf.get(self.i..self.i + DIR_ENTRY_MIN_SIZE)?;

        let dentry = DirEntry64::new(dentry_slice);

        let reclen = dentry.d_reclen();
        if reclen == 0 {
            panic!("reclen is 0");
        }

        let dentry_slice = self.buf.get(self.i..self.i + reclen)?;

        let dentry = DirEntry64::new(dentry_slice);

        self.i += reclen;

        Some(dentry)
    }
}

impl<'a> DirEntryIter<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, i: 0 }
    }
}

/// Represents the return value of getdents64(2)
#[derive(Debug)]
pub struct GetDents {
    pub buf: Box<[u8]>,

    /// Number of bytes the getdents64 call populated in buf
    pub n: usize,
}

impl GetDents {
    pub fn iter(&self) -> DirEntryIter<'_> {
        DirEntryIter::new(&self.buf[..self.n])
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
        let source = reactor.open_at(-1, &path, flags, 0o755);
        let fd = source.collect_rw().await.map_err(|source| {
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

        let path = self.file.path_required("open file")?.join(path.as_ref());
        DmaFile::open(path).await
    }

    /// Similar to create() in the standard library, but returns a DMA file
    pub async fn create<P: AsRef<Path>>(path: P) -> Result<Directory> {
        let path = path.as_ref().to_owned();
        let source = crate::executor().reactor().create_dir(&*path, 0o777).await;

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

        let path = self.file.path_required("create file")?.join(path.as_ref());
        DmaFile::create(path).await
    }

    /// Read a list of dirent64s into buf, which is then returned in GetDents
    pub async fn get_dents(&self, buf: Box<[u8]>) -> Result<GetDents> {
        let res = crate::executor()
            .reactor()
            .get_dents(self.as_raw_fd(), buf)
            .await;

        let n = res.collect_rw().await?;

        let buf = match res.extract_source_type() {
            SourceType::GetDents(_, Some(buf)) => buf,
            _ => unreachable!(
                "a successful get_dents call should return a SourceType::GetDents(fd, Some(buf))"
            ),
        };

        Ok(GetDents { buf, n })
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

    /// Closes this DMA file.
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
    use std::collections::HashSet;

    use super::*;
    use crate::test_utils::*;

    #[test]
    fn directory_get_dents() {
        test_executor!(async move {
            let test_dir = make_tmp_test_directory("get_dents_testing");
            let dir = Directory::open(test_dir.path.clone()).await.unwrap();
            let buf = vec![0u8; 1024].into_boxed_slice();

            let f1 = dir.create_file("f1").await.unwrap();
            let f2 = dir.create_file("f2").await.unwrap();

            let f1_stat = f1.statx().await.unwrap();
            let f2_stat = f2.statx().await.unwrap();

            let names = HashSet::from([".", "..", "f1", "f2"]);

            let dents = dir.get_dents(buf).await.unwrap();

            let mut actual_names = HashSet::new();
            for dent in dents.iter() {
                actual_names.insert(dent.d_name().unwrap().to_str().unwrap());

                if dent.d_name().unwrap() == "f1" {
                    assert_eq!(f1_stat.stx_ino, dent.d_ino());
                }

                if dent.d_name().unwrap() == "f2" {
                    assert_eq!(f2_stat.stx_ino, dent.d_ino());
                }
            }

            assert_eq!(actual_names, names);
        });
    }

    #[test]
    fn directory_dirent_iter_empty() {
        let buf = vec![0u8; 0].into_boxed_slice();
        let iter = DirEntryIter::new(&buf);
        assert_eq!(iter.count(), 0);
    }

    #[test]
    fn directory_dirent_iter_buffer_too_small_for_reclen() {
        let mut buf = vec![0u8; 24];

        let reclen: u16 = 40;
        buf[16..18].copy_from_slice(&reclen.to_ne_bytes());

        let mut iter = DirEntryIter::new(&buf);
        assert!(iter.next().is_none());
    }

    #[test]
    #[should_panic(expected = "reclen is 0")]
    fn directory_dirent_iter_zero_reclen_panics() {
        let mut buf = vec![0u8; DIR_ENTRY_MIN_SIZE];
        let reclen: u16 = 0;
        buf[16..18].copy_from_slice(&reclen.to_ne_bytes());

        let mut iter = DirEntryIter::new(&buf);
        let _ = iter.next(); // This should trigger the assert_ne!(reclen, 0)
    }

    #[test]
    fn directory_dirent_missing_null_terminator() {
        let mut buf = vec![0u8; 22];

        let reclen: u16 = 22;
        buf[16..18].copy_from_slice(&reclen.to_ne_bytes());

        buf[19] = b'a';
        buf[20] = b'b';
        buf[21] = b'c';

        let mut iter = DirEntryIter::new(&buf);
        let entry = iter
            .next()
            .expect("Should still parse out the entry from the slice");

        let name_result = entry.d_name();
        assert!(name_result.is_err());
    }
}
