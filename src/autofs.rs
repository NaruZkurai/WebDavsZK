//! Robust `DavFileSystem` wrapper around `LocalFs`.
//!
//! Two optional behaviours (both on by default, both independently
//! configurable):
//!
//! 1. **Auto-create missing parents** (`--create-parents`). KIO uploads
//!    directory trees with parallel `MKCOL`+`PUT` and assumes servers create
//!    intermediate collections implicitly (like Nextcloud/SabreDAV). A strict
//!    server answers such a `PUT` with `409 Conflict` because the parent is
//!    missing, which KIO surfaces as *"the file/folder does not exist"* and
//!    the recursive copy stops midway.
//! 2. **Atomic writes** (`--atomic-writes`). `LocalFs` writes a `PUT` body
//!    straight into the destination file, so a concurrent reader sees a
//!    partially-written file and an aborted upload leaves a corrupt file
//!    behind. We stream into a temp file named `{name}.uploading.nzk_webdavs`
//!    in the same directory and atomically `rename()` it to `{name}` when the
//!    upload completes, so readers see either the old or the new file, never a
//!    partial one (and in-progress uploads are clearly visible).

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use bytes::{Buf, Bytes};
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use dav_server::localfs::LocalFs;
use log::debug;
use sha2::{Digest, Sha256};

/// Local filesystem with optional auto-parent creation and atomic writes,
/// mirroring `LocalFs::fspath()` path mapping.
#[derive(Clone)]
pub struct AutoMkcolFs {
    root: PathBuf,
    inner: Box<LocalFs>,
    create_parents: bool,
    atomic_writes: bool,
    fsync: bool,
}

impl AutoMkcolFs {
    pub fn new(
        root: impl AsRef<std::path::Path>,
        create_parents: bool,
        atomic_writes: bool,
        fsync: bool,
    ) -> Self {
        let root = root.as_ref().to_path_buf();
        let inner = LocalFs::new(&root, true, false, false);
        Self {
            root,
            inner,
            create_parents,
            atomic_writes,
            fsync,
        }
    }

    /// Map a DAV path to a filesystem path (same as `LocalFs::fspath`).
    fn fspath(&self, path: &DavPath) -> PathBuf {
        self.root.join(path.as_rel_ospath())
    }

    /// Create the parent directory of `path` (all missing segments).
    async fn ensure_parent(&self, path: &DavPath) -> FsResult<()> {
        if !self.create_parents {
            return Ok(());
        }
        let parent = path.parent();
        let mut p = self.root.clone();
        p.push(parent.as_rel_ospath());
        if p != self.root {
            debug!("auto-creating missing parent dirs for {}", p.display());
            self.inner
                .blocking(move || std::fs::create_dir_all(&p))
                .await
                .map_err(|e| FsError::from(&e))?;
        }
        Ok(())
    }
}

impl DavFileSystem for AutoMkcolFs {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            // Writes (PUT) need a pre-existing parent (unless auto-created).
            if options.write && options.create {
                self.ensure_parent(path).await?;
            }

            // Atomic PUT: stream to a `.uploading.nzk_webdavs` temp file in the
            // same directory and atomically rename it into place on success, so
            // readers never see a partial file and an aborted upload leaves no
            // corrupt file behind. The SHA-256 of the upload is verified
            // against any client-provided OC-Checksum and recorded as an xattr.
            if self.atomic_writes && options.write && options.create && options.truncate {
                let target = self.fspath(path);
                // Mirror `create_new` (If-None-Match: *) semantics: fail if the
                // destination already exists.
                if options.create_new && target.exists() {
                    return Err(FsError::Exists);
                }
                if target.is_dir() {
                    return Err(FsError::Forbidden);
                }
                let dir = target
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| self.root.clone());

                // Fail fast (507) when the size is known and the filesystem
                // can't fit the request.
                if let Some(size) = options.size
                    && let Ok(free) = free_space(&dir)
                    && free < size
                {
                    return Err(FsError::InsufficientStorage);
                }

                let (temp, file) = create_temp(&dir, &target)?;
                let af = AtomicFile::new(
                    temp,
                    target,
                    options.size,
                    options.checksum.clone(),
                    file,
                    self.fsync,
                );
                return Ok(Box::new(af) as Box<dyn DavFile>);
            }

            self.inner.open(path, options).await
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        self.inner.read_dir(path, meta)
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        self.inner.metadata(path)
    }

    fn symlink_metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        self.inner.symlink_metadata(path)
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            // Ensure ancestors exist (like Nextcloud/SabreDAV), then create the
            // collection itself with strict "exists -> error" semantics.
            self.ensure_parent(path).await?;
            self.inner.create_dir(path).await
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        self.inner.remove_dir(path)
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        self.inner.remove_file(path)
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        self.inner.rename(from, to)
    }

    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        self.inner.copy(from, to)
    }
}

/// Run a blocking closure off the async worker, mirroring `LocalFs::blocking`.
async fn blocking<F, R>(func: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    match tokio::runtime::Handle::current().runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(func),
        _ => tokio::task::spawn_blocking(func).await.unwrap(),
    }
}

/// Create a temp file in `dir` for an in-flight upload, named
/// `{name}.uploading.nzk_webdavs`. It lives in the same directory as the
/// target (so the final rename is atomic) and is clearly visible as a
/// "still uploading" file. If that name is already taken (a concurrent
/// upload of the same file, or a stale temp from a crash), a numeric suffix
/// is added so uploads never block each other.
fn create_temp(dir: &Path, target: &Path) -> FsResult<(PathBuf, std::fs::File)> {
    let name = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let base = dir.join(format!("{name}.uploading.nzk_webdavs"));
    for i in 0..16u32 {
        let temp = if i == 0 {
            base.clone()
        } else {
            dir.join(format!("{name}.uploading.nzk_webdavs.{i}"))
        };
        let mut oo = std::fs::OpenOptions::new();
        oo.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            oo.mode(0o644);
        }
        match oo.open(&temp) {
            Ok(f) => return Ok((temp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(FsError::from(&e)),
        }
    }
    Err(FsError::Exists)
}

/// Metadata wrapper for the atomic temp files.
#[derive(Debug, Clone)]
struct LocalMeta(std::fs::Metadata);

impl DavMetaData for LocalMeta {
    fn len(&self) -> u64 {
        self.0.len()
    }
    fn modified(&self) -> FsResult<SystemTime> {
        self.0.modified().map_err(|e| FsError::from(&e))
    }
    fn is_dir(&self) -> bool {
        self.0.is_dir()
    }
    fn is_file(&self) -> bool {
        self.0.is_file()
    }
    fn is_symlink(&self) -> bool {
        self.0.file_type().is_symlink()
    }

    #[cfg(unix)]
    fn accessed(&self) -> FsResult<SystemTime> {
        self.0.accessed().map_err(|e| FsError::from(&e))
    }
    #[cfg(unix)]
    fn created(&self) -> FsResult<SystemTime> {
        self.0.created().map_err(|e| FsError::from(&e))
    }
    #[cfg(unix)]
    fn status_changed(&self) -> FsResult<SystemTime> {
        use std::os::unix::fs::MetadataExt;
        Ok(UNIX_EPOCH + Duration::new(self.0.ctime() as u64, 0))
    }
    #[cfg(unix)]
    fn executable(&self) -> FsResult<bool> {
        use std::os::unix::fs::PermissionsExt;
        if self.0.is_file() {
            Ok((self.0.permissions().mode() & 0o100) > 0)
        } else {
            Err(FsError::NotImplemented)
        }
    }
}

/// Bytes of free space available to us on the filesystem containing `dir`.
///
/// On platforms without `statvfs` this returns `u64::MAX` (skip the pre-check;
/// the reserve call itself still fails if there's no space).
fn free_space(dir: &Path) -> std::io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let cpath = std::ffi::CString::new(dir.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL")
        })?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut st) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let frsize = if st.f_frsize > 0 {
            st.f_frsize
        } else {
            st.f_bsize
        };
        Ok(st.f_bavail as u64 * frsize as u64)
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(u64::MAX)
    }
}

/// A `DavFile` that streams into a temp file and atomically renames it into
/// place on successful completion (`flush()`). If the transfer is aborted the
/// temp file is removed when this is dropped, so no partial file is ever left
/// at the destination.
#[derive(Debug)]
struct AtomicFile {
    file: Option<std::fs::File>,
    temp_path: PathBuf,
    target_path: PathBuf,
    expected_size: Option<u64>,
    written: u64,
    committed: bool,
    fsync: bool,
    hasher: Sha256,
    expected_checksum: Option<String>,
}

impl AtomicFile {
    fn new(
        temp_path: PathBuf,
        target_path: PathBuf,
        expected_size: Option<u64>,
        expected_checksum: Option<String>,
        file: std::fs::File,
        fsync: bool,
    ) -> Self {
        Self {
            file: Some(file),
            temp_path,
            target_path,
            expected_size,
            written: 0,
            committed: false,
            fsync,
            hasher: Sha256::new(),
            expected_checksum,
        }
    }

    /// Sync data and atomically move the temp file into place - but only if
    /// the whole body arrived (or the length was unknown, i.e. chunked).
    async fn commit(&mut self) -> FsResult<()> {
        if self.committed {
            return Ok(());
        }
        let should_commit = self.expected_size.is_none_or(|s| self.written == s);
        let temp = self.temp_path.clone();
        let target = self.target_path.clone();
        let fsync = self.fsync;

        // Hash verification: compare the client-provided OC-Checksum (SHA256)
        // against what we received. On mismatch the upload is rejected and the
        // temp file is removed by `Drop`.
        let digest = self.hasher.clone().finalize();
        if let Some(ck) = self.expected_checksum.as_deref()
            && let Some(expected) = parse_sha256_checksum(ck)
            && expected != digest.as_slice()
        {
            debug!(
                "checksum mismatch for {}; rejecting upload",
                target.display()
            );
            return Err(FsError::GeneralFailure);
        }

        let file = self.file.take();
        let res = blocking(move || -> std::io::Result<()> {
            // Optional per-file fsync for maximum durability. Off by default:
            // it's the dominant cost when moving many small files.
            if fsync && let Some(f) = file.as_ref() {
                f.sync_data()?;
            }
            if should_commit {
                std::fs::rename(&temp, &target)?;
            }
            Ok(())
        })
        .await;
        if res.is_ok() {
            self.committed = should_commit;
            if self.committed {
                set_xattr_sha256(&self.target_path, &hex::encode(digest));
            }
        }
        res.map_err(|e| FsError::from(&e))
    }
}

impl DavFile for AtomicFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        Box::pin(async move {
            // Commit normally happens in flush(); do it here as a fallback so
            // the reported metadata reflects the final file.
            self.commit().await?;
            let p = if self.committed {
                &self.target_path
            } else {
                &self.temp_path
            };
            let meta = std::fs::metadata(p).map_err(|e| FsError::from(&e))?;
            Ok(Box::new(LocalMeta(meta)) as Box<dyn DavMetaData>)
        })
    }

    fn write_bytes(&'_ mut self, buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(async move {
            self.hasher.update(&buf);
            let mut file = self.file.take().ok_or(FsError::GeneralFailure)?;
            let len = buf.len();
            let (res, file) = blocking(move || {
                let r = file.write_all(&buf);
                (r, file)
            })
            .await;
            self.file = Some(file);
            self.written += len as u64;
            res.map_err(|e| FsError::from(&e))
        })
    }

    fn write_buf(&'_ mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async move {
            let mut file = self.file.take().ok_or(FsError::GeneralFailure)?;
            let mut hasher = std::mem::take(&mut self.hasher);
            let remaining = buf.remaining();
            let (res, file, hasher) = blocking(move || {
                while buf.remaining() > 0 {
                    let chunk = buf.chunk();
                    let n = match file.write(chunk) {
                        Ok(0) => {
                            return (
                                Err(std::io::Error::new(
                                    std::io::ErrorKind::WriteZero,
                                    "write returned 0 bytes",
                                )),
                                file,
                                hasher,
                            );
                        }
                        Ok(n) => n,
                        Err(e) => return (Err(e), file, hasher),
                    };
                    hasher.update(&chunk[..n]);
                    buf.advance(n);
                }
                (Ok(()), file, hasher)
            })
            .await;
            self.hasher = hasher;
            self.file = Some(file);
            self.written += remaining as u64;
            res.map_err(|e| FsError::from(&e))
        })
    }

    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            let mut file = self.file.take().ok_or(FsError::GeneralFailure)?;
            let mut buf = vec![0u8; count];
            let (res, buf, file) = blocking(move || {
                let n = file.read(&mut buf);
                (n, buf, file)
            })
            .await;
            self.file = Some(file);
            match res {
                Ok(n) => Ok(Bytes::copy_from_slice(&buf[..n])),
                Err(e) => Err(FsError::from(&e)),
            }
        })
    }

    fn seek(&'_ mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            let mut file = self.file.take().ok_or(FsError::GeneralFailure)?;
            let (res, file) = blocking(move || (file.seek(pos), file)).await;
            self.file = Some(file);
            res.map_err(|e| FsError::from(&e))
        })
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        Box::pin(async move { self.commit().await })
    }
}

impl Drop for AtomicFile {
    fn drop(&mut self) {
        // If the upload never committed, remove the leftover temp file.
        if !self.committed {
            let _ = std::fs::remove_file(&self.temp_path);
        }
    }
}

/// Parse an `OC-Checksum` value of the form `SHA256:<hex|base64>` into raw
/// bytes. Returns `None` for unsupported algorithms (verification skipped).
fn parse_sha256_checksum(raw: &str) -> Option<Vec<u8>> {
    let (alg, val) = raw.trim().split_once(':')?;
    if !alg.eq_ignore_ascii_case("sha256") {
        return None;
    }
    let val = val.trim();
    if let Ok(bytes) = hex::decode(val) {
        Some(bytes)
    } else {
        base64::engine::general_purpose::STANDARD.decode(val).ok()
    }
}

/// Best-effort: record the SHA-256 of a file as a user xattr
/// (`user.nzk_webdavs.sha256`) so it can be verified later with
/// `getfattr -n user.nzk_webdavs.sha256 <file>`. Ignored on filesystems
/// without xattr support.
fn set_xattr_sha256(path: &Path, sha_hex: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let name = b"user.nzk_webdavs.sha256\0";
        if let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_bytes())
            && let Ok(cval) = std::ffi::CString::new(sha_hex)
        {
            unsafe {
                libc::setxattr(
                    cpath.as_ptr(),
                    name.as_ptr() as *const libc::c_char,
                    cval.as_ptr() as *const libc::c_void,
                    cval.as_bytes().len(),
                    0,
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, sha_hex);
    }
}
