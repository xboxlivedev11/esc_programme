//! The Hold: content-addressed blob store (SHA-256) with resumable chunked uploads, plus a disk-space query.
//! Layout: `<data>/blobs/<first 2 hex>/<64 hex>` for stored blobs, `<data>/tmp/<upload id>` for in-flight uploads.
//! Std only.

use crate::crypto::{Sha256, hex, random_u64};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// SHA-256 digest identifying a blob.
pub type Hash = [u8; 32];

/// Streaming buffer size.
const BUF: usize = 256 * 1024;
/// Most spare buffers kept for reuse.
const POOL_MAX: usize = 16;

/// Result of writing a chunk.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Put {
    /// Chunk accepted; bytes written so far.
    More(u64),
    /// Upload complete and stored under this hash.
    Done(Hash),
}

/// Bookkeeping for one upload; `work` is `None` while a `put` has it checked out.
struct Entry {
    owner: u32,
    total: u64,
    written: u64,
    last_ms: u64,
    tmp: PathBuf,
    work: Option<Work>,
}

/// The heavy state of an upload: tmp file and running hash.
struct Work {
    file: File,
    hasher: Sha256,
}

/// The blob store; share it in an `Arc`.
pub struct Store {
    root: PathBuf,
    uploads: Mutex<HashMap<u64, Entry>>,
    bufs: Mutex<Vec<Vec<u8>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn not_found() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "no such upload")
}

fn invalid_input(why: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, why)
}

/// Reads exactly `len` bytes from `r` through `buf`, handing each piece to `sink`.
fn pump(r: &mut impl Read, mut len: u64, buf: &mut [u8], mut sink: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<()> {
    while len > 0 {
        let n = len.min(buf.len() as u64) as usize;
        r.read_exact(&mut buf[..n])?;
        sink(&buf[..n])?;
        len -= n as u64;
    }
    Ok(())
}

impl Store {
    /// Creates `<data>/blobs` and `<data>/tmp`, deleting leftover tmp files from earlier runs.
    pub fn open(data_dir: &Path) -> io::Result<Store> {
        fs::create_dir_all(data_dir.join("blobs"))?;
        let tmp = data_dir.join("tmp");
        fs::create_dir_all(&tmp)?;
        for entry in fs::read_dir(&tmp)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(Store { root: data_dir.to_path_buf(), uploads: Mutex::new(HashMap::new()), bufs: Mutex::new(Vec::new()) })
    }

    /// Path of the blob with hash `h`: `<data>/blobs/<first 2 hex>/<64 hex>`.
    pub fn path(&self, h: &Hash) -> PathBuf {
        let name = hex(h);
        self.root.join("blobs").join(&name[..2]).join(name)
    }

    /// Whether the blob is stored.
    pub fn has(&self, h: &Hash) -> bool {
        self.path(h).is_file()
    }

    /// Opens a stored blob for reading; returns the file and its length.
    pub fn open_blob(&self, h: &Hash) -> io::Result<(File, u64)> {
        let file = File::open(self.path(h))?;
        let len = file.metadata()?.len();
        Ok((file, len))
    }

    /// Creates a fresh tmp file named `<prefix><16 hex of id>`; returns the id, file and path.
    fn create_tmp(&self, prefix: &str) -> io::Result<(u64, File, PathBuf)> {
        loop {
            let id = random_u64();
            if id == 0 {
                continue;
            }
            let path = self.root.join("tmp").join(format!("{prefix}{id:016x}"));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((id, file, path)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Starts an upload of exactly `total` bytes for `owner`; returns a random non-zero upload id.
    pub fn begin(&self, owner: u32, total: u64) -> io::Result<u64> {
        let (id, file, tmp) = self.create_tmp("")?;
        let entry = Entry { owner, total, written: 0, last_ms: now_ms(), tmp, work: Some(Work { file, hasher: Sha256::new() }) };
        lock(&self.uploads).insert(id, entry);
        Ok(id)
    }

    /// `(written, total)` if the upload exists and belongs to `owner` (also while a put is running).
    pub fn status(&self, id: u64, owner: u32) -> Option<(u64, u64)> {
        lock(&self.uploads).get(&id).filter(|e| e.owner == owner).map(|e| (e.written, e.total))
    }

    fn take_buf(&self) -> Vec<u8> {
        lock(&self.bufs).pop().unwrap_or_else(|| vec![0; BUF])
    }

    fn give_buf(&self, buf: Vec<u8>) {
        let mut pool = lock(&self.bufs);
        if pool.len() < POOL_MAX {
            pool.push(buf);
        }
    }

    /// Returns a checked-out upload to the map with `written` committed; if it was aborted meanwhile,
    /// deletes its tmp file and reports `NotFound`.
    fn check_in(&self, id: u64, work: Work, written: u64, tmp: &Path) -> io::Result<()> {
        let mut map = lock(&self.uploads);
        match map.get_mut(&id) {
            Some(e) => {
                e.work = Some(work);
                e.written = written;
                e.last_ms = now_ms();
                Ok(())
            }
            None => {
                drop(map);
                drop(work);
                let _ = fs::remove_file(tmp);
                Err(not_found())
            }
        }
    }

    /// Writes the next chunk of an upload: `offset` must equal the bytes written so far and `offset + len <= total`.
    /// Reads exactly `len` bytes from `r`. On a read or write error the tmp file and hash roll back to the chunk
    /// start so the same chunk can be retried. Completing the upload stores the blob and returns `Done(hash)`.
    /// While one put runs, other puts on the same id get `NotFound`.
    pub fn put(&self, id: u64, owner: u32, offset: u64, len: u64, r: &mut impl Read) -> io::Result<Put> {
        let (mut work, total, tmp) = {
            let mut map = lock(&self.uploads);
            let e = map.get_mut(&id).filter(|e| e.owner == owner).ok_or_else(not_found)?;
            if e.work.is_none() {
                return Err(not_found());
            }
            if offset != e.written {
                return Err(invalid_input("offset does not match bytes written"));
            }
            if len > e.total - e.written {
                return Err(invalid_input("chunk exceeds upload size"));
            }
            (e.work.take().expect("checked above"), e.total, e.tmp.clone())
        };

        let snapshot = work.hasher.clone();
        let mut buf = self.take_buf();
        let streamed = work.file.seek(SeekFrom::Start(offset)).and_then(|_| {
            let Work { file, hasher } = &mut work;
            pump(r, len, &mut buf, |chunk| {
                file.write_all(chunk)?;
                hasher.update(chunk);
                Ok(())
            })
        });
        self.give_buf(buf);
        if let Err(e) = streamed {
            work.hasher = snapshot;
            let _ = work.file.set_len(offset);
            let _ = self.check_in(id, work, offset, &tmp);
            return Err(e);
        }

        let written = offset + len;
        if written < total {
            self.check_in(id, work, written, &tmp)?;
            return Ok(Put::More(written));
        }
        if !lock(&self.uploads).contains_key(&id) {
            // Aborted while this final chunk was streaming: do not store the blob.
            drop(work);
            let _ = fs::remove_file(&tmp);
            return Err(not_found());
        }
        match self.finish(&work, total, &tmp) {
            Ok(hash) => {
                drop(work);
                lock(&self.uploads).remove(&id);
                Ok(Put::Done(hash))
            }
            Err(e) => {
                let _ = self.check_in(id, work, written, &tmp);
                Err(e)
            }
        }
    }

    /// Makes a completed upload durable and moves it into place; returns its hash.
    fn finish(&self, work: &Work, total: u64, tmp: &Path) -> io::Result<Hash> {
        work.file.set_len(total)?; // drops any stray bytes left by a failed chunk
        work.file.sync_all()?;
        let hash = work.hasher.clone().finalize();
        self.install(tmp, &hash)?;
        Ok(hash)
    }

    /// Moves the finished tmp file to the blob path of `hash`, or deletes it if that blob already exists.
    fn install(&self, tmp: &Path, hash: &Hash) -> io::Result<()> {
        let dest = self.path(hash);
        if !dest.exists() {
            if let Some(dir) = dest.parent() {
                fs::create_dir_all(dir)?;
            }
            match fs::rename(tmp, &dest) {
                Ok(()) => return Ok(()),
                // Lost a race with an identical upload: the blob is there, which is all we need.
                Err(_) if dest.exists() => {}
                Err(e) => return Err(e),
            }
        }
        let _ = fs::remove_file(tmp);
        Ok(())
    }

    /// Aborts an upload and deletes its tmp file (a running put cleans up when it finishes).
    pub fn abort(&self, id: u64) {
        let entry = lock(&self.uploads).remove(&id);
        if let Some(Entry { work: Some(work), tmp, .. }) = entry {
            drop(work);
            let _ = fs::remove_file(tmp);
        }
    }

    /// Aborts idle uploads not touched for more than `max_idle_ms` (uploads with a running put are skipped).
    pub fn gc_uploads(&self, max_idle_ms: u64) {
        let now = now_ms();
        let stale: Vec<Entry> = {
            let mut map = lock(&self.uploads);
            let ids: Vec<u64> = map
                .iter()
                .filter(|(_, e)| e.work.is_some() && now.saturating_sub(e.last_ms) > max_idle_ms)
                .map(|(id, _)| *id)
                .collect();
            ids.iter().filter_map(|id| map.remove(id)).collect()
        };
        for e in stale {
            drop(e.work);
            let _ = fs::remove_file(e.tmp);
        }
    }

    /// Deletes a stored blob; a missing file is fine.
    pub fn remove(&self, h: &Hash) -> io::Result<()> {
        match fs::remove_file(self.path(h)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// Stores a blob fetched from a peer: streams exactly `len` bytes, verifies SHA-256 equals `*expected`
    /// (`InvalidData` otherwise, nothing left behind), then moves it into place.
    pub fn import(&self, expected: &Hash, len: u64, r: &mut impl Read) -> io::Result<()> {
        let (_, mut file, tmp) = self.create_tmp("imp-")?;
        let mut buf = self.take_buf();
        let result = (|| {
            let mut hasher = Sha256::new();
            pump(r, len, &mut buf, |chunk| {
                file.write_all(chunk)?;
                hasher.update(chunk);
                Ok(())
            })?;
            if hasher.finalize() != *expected {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "blob hash mismatch"));
            }
            file.sync_all()?;
            self.install(&tmp, expected)
        })();
        self.give_buf(buf);
        if result.is_err() {
            drop(file);
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

/// `(bytes available to this user, total bytes)` on the volume holding `path` (pass an existing path).
pub fn disk_free(path: &Path) -> io::Result<(u64, u64)> {
    sys::disk_free(path)
}

#[cfg(windows)]
mod sys {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
    }

    pub fn disk_free(path: &Path) -> io::Result<(u64, u64)> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        // SAFETY: `wide` is NUL-terminated and the three out-pointers refer to live u64s.
        let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
        if ok == 0 { Err(io::Error::last_os_error()) } else { Ok((avail, total)) }
    }
}

/// Linux x86_64 / aarch64 (glibc and musl share this 64-bit `struct statvfs` prefix).
#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
mod sys {
    use std::ffi::{CString, c_char, c_int, c_ulong};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// Leading fields of `struct statvfs`, padded generously so the kernel/libc never writes out of bounds.
    #[repr(C)]
    struct StatVfs {
        f_bsize: c_ulong,
        f_frsize: c_ulong,
        f_blocks: u64,
        f_bfree: u64,
        f_bavail: u64,
        rest: [u64; 32],
    }

    unsafe extern "C" {
        fn statvfs(path: *const c_char, buf: *mut StatVfs) -> c_int;
    }

    pub fn disk_free(path: &Path) -> io::Result<(u64, u64)> {
        let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut st = StatVfs { f_bsize: 0, f_frsize: 0, f_blocks: 0, f_bfree: 0, f_bavail: 0, rest: [0; 32] };
        // SAFETY: `c` is NUL-terminated; `st` is at least as large as the C struct (112 bytes) and writable.
        if unsafe { statvfs(c.as_ptr(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let unit = if st.f_frsize != 0 { st.f_frsize } else { st.f_bsize };
        Ok((st.f_bavail.saturating_mul(unit), st.f_blocks.saturating_mul(unit)))
    }
}

/// macOS: `statfs` (64-bit counts); its first five fields are identical in the legacy and `$INODE64` layouts.
#[cfg(target_os = "macos")]
mod sys {
    use std::ffi::{CString, c_char, c_int};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// Leading fields of `struct statfs`, padded past its full size (about 2168 bytes).
    #[repr(C)]
    struct StatFs {
        f_bsize: u32,
        f_iosize: i32,
        f_blocks: u64,
        f_bfree: u64,
        f_bavail: u64,
        rest: [u64; 512],
    }

    unsafe extern "C" {
        fn statfs(path: *const c_char, buf: *mut StatFs) -> c_int;
    }

    pub fn disk_free(path: &Path) -> io::Result<(u64, u64)> {
        let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut st = StatFs { f_bsize: 0, f_iosize: 0, f_blocks: 0, f_bfree: 0, f_bavail: 0, rest: [0; 512] };
        // SAFETY: `c` is NUL-terminated; `st` is larger than the C struct and writable.
        if unsafe { statfs(c.as_ptr(), &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let unit = u64::from(st.f_bsize);
        Ok((st.f_bavail.saturating_mul(unit), st.f_blocks.saturating_mul(unit)))
    }
}

/// Targets without a verified binding.
#[cfg(not(any(windows, all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")), target_os = "macos")))]
mod sys {
    use std::io;
    use std::path::Path;

    pub fn disk_free(_path: &Path) -> io::Result<(u64, u64)> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::sha256;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};

    /// Temp directory removed on drop.
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> TmpDir {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("esc-blobs-test-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        fn store(&self) -> Store {
            Store::open(&self.0).unwrap()
        }
        fn tmp_files(&self) -> Vec<String> {
            fs::read_dir(self.0.join("tmp")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn data(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 131 % 251) as u8).collect()
    }

    /// Reader that yields `ok` bytes of `data` and then fails.
    struct FailAfter<'a> {
        data: &'a [u8],
        ok: usize,
        pos: usize,
    }
    impl Read for FailAfter<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.ok {
                return Err(io::Error::other("injected failure"));
            }
            let n = buf.len().min(self.ok - self.pos).min(1000);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    /// Reader that announces its first read, then waits to be released.
    struct Gate<'a> {
        data: &'a [u8],
        started: Option<mpsc::Sender<()>>,
        go: Option<mpsc::Receiver<()>>,
    }
    impl Read for Gate<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if let (Some(tx), Some(rx)) = (self.started.take(), self.go.take()) {
                tx.send(()).unwrap();
                rx.recv().unwrap();
            }
            let n = buf.len().min(self.data.len());
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Ok(n)
        }
    }

    fn upload_all(store: &Store, owner: u32, bytes: &[u8], chunk: usize) -> Hash {
        let id = store.begin(owner, bytes.len() as u64).unwrap();
        let mut off = 0;
        loop {
            let end = (off + chunk).min(bytes.len());
            match store.put(id, owner, off as u64, (end - off) as u64, &mut &bytes[off..end]).unwrap() {
                Put::More(w) => {
                    assert_eq!(w, end as u64);
                    off = end;
                }
                Put::Done(h) => return h,
            }
        }
    }

    #[test]
    fn store_is_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<Store>();
    }

    #[test]
    fn three_chunk_upload() {
        let dir = TmpDir::new();
        let store = dir.store();
        let bytes = data(600_000);
        let id = store.begin(7, bytes.len() as u64).unwrap();
        assert_ne!(id, 0);
        assert_eq!(store.status(id, 7), Some((0, 600_000)));
        assert_eq!(store.put(id, 7, 0, 200_000, &mut &bytes[..200_000]).unwrap(), Put::More(200_000));
        assert_eq!(store.status(id, 7), Some((200_000, 600_000)));
        assert_eq!(store.put(id, 7, 200_000, 200_000, &mut &bytes[200_000..400_000]).unwrap(), Put::More(400_000));
        assert_eq!(store.status(id, 7), Some((400_000, 600_000)));
        assert_eq!(dir.tmp_files().len(), 1);
        let Put::Done(h) = store.put(id, 7, 400_000, 200_000, &mut &bytes[400_000..]).unwrap() else {
            panic!("expected Done");
        };
        assert_eq!(h, sha256(&bytes));
        assert!(store.has(&h));
        let p = store.path(&h);
        assert_eq!(p, dir.0.join("blobs").join(&hex(&h)[..2]).join(hex(&h)));
        assert_eq!(p.parent().unwrap().file_name().unwrap().to_string_lossy(), hex(&h)[..2]);
        assert_eq!(fs::read(&p).unwrap(), bytes);
        let (mut f, len) = store.open_blob(&h).unwrap();
        assert_eq!(len, 600_000);
        let mut back = Vec::new();
        f.read_to_end(&mut back).unwrap();
        assert_eq!(back, bytes);
        assert_eq!(store.status(id, 7), None);
        assert!(dir.tmp_files().is_empty());
        assert!(store.open_blob(&[0; 32]).is_err());
        assert!(!store.has(&[0; 32]));
    }

    #[test]
    fn odd_chunk_sizes_larger_than_buffer() {
        let dir = TmpDir::new();
        let store = dir.store();
        let bytes = data(BUF * 2 + 12_345);
        for chunk in [1usize << 20, BUF, BUF + 1, 4097] {
            assert_eq!(upload_all(&store, 1, &bytes, chunk), sha256(&bytes), "chunk {chunk}");
        }
    }

    #[test]
    fn rejects_bad_requests_and_stays_usable() {
        let dir = TmpDir::new();
        let store = dir.store();
        let bytes = data(1000);
        let id = store.begin(1, 1000).unwrap();
        let kind = |r: io::Result<Put>| r.unwrap_err().kind();
        assert_eq!(kind(store.put(id, 1, 5, 10, &mut &bytes[..10])), io::ErrorKind::InvalidInput, "offset ahead");
        assert_eq!(kind(store.put(id, 1, 0, 1001, &mut &bytes[..])), io::ErrorKind::InvalidInput, "overlong chunk");
        assert_eq!(kind(store.put(id, 2, 0, 10, &mut &bytes[..10])), io::ErrorKind::NotFound, "wrong owner");
        assert_eq!(kind(store.put(id ^ 1, 1, 0, 10, &mut &bytes[..10])), io::ErrorKind::NotFound, "unknown id");
        assert_eq!(kind(store.put(0, 1, 0, 10, &mut &bytes[..10])), io::ErrorKind::NotFound, "id 0");
        assert_eq!(store.status(id, 2), None);
        assert_eq!(store.status(id, 1), Some((0, 1000)));
        store.put(id, 1, 0, 400, &mut &bytes[..400]).unwrap();
        assert_eq!(kind(store.put(id, 1, 0, 100, &mut &bytes[..100])), io::ErrorKind::InvalidInput, "offset behind");
        assert_eq!(kind(store.put(id, 1, 400, 601, &mut &bytes[400..])), io::ErrorKind::InvalidInput, "overlong second chunk");
        assert_eq!(store.status(id, 1), Some((400, 1000)));
        let Put::Done(h) = store.put(id, 1, 400, 600, &mut &bytes[400..]).unwrap() else { panic!() };
        assert_eq!(h, sha256(&bytes));
    }

    #[test]
    fn reader_error_mid_chunk_is_retryable() {
        let dir = TmpDir::new();
        let store = dir.store();
        let bytes = data(300_000);
        let id = store.begin(3, 300_000).unwrap();
        store.put(id, 3, 0, 100_000, &mut &bytes[..100_000]).unwrap();
        // Fail partway through the second chunk, more than one buffer in.
        let mut bad = FailAfter { data: &bytes[100_000..200_000], ok: 70_000, pos: 0 };
        let e = store.put(id, 3, 100_000, 100_000, &mut bad).unwrap_err();
        assert_eq!(e.to_string(), "injected failure");
        assert_eq!(store.status(id, 3), Some((100_000, 300_000)));
        let tmp = dir.0.join("tmp").join(dir.tmp_files().pop().unwrap());
        assert_eq!(fs::metadata(&tmp).unwrap().len(), 100_000, "tmp rolled back to chunk start");
        // A short stream (EOF) is an error too, and also leaves the upload retryable.
        let e = store.put(id, 3, 100_000, 100_000, &mut &bytes[100_000..150_000]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(store.status(id, 3), Some((100_000, 300_000)));
        // Retry the same chunk (even with a smaller length), then finish.
        assert_eq!(store.put(id, 3, 100_000, 60_000, &mut &bytes[100_000..160_000]).unwrap(), Put::More(160_000));
        assert_eq!(store.put(id, 3, 160_000, 40_000, &mut &bytes[160_000..200_000]).unwrap(), Put::More(200_000));
        let Put::Done(h) = store.put(id, 3, 200_000, 100_000, &mut &bytes[200_000..]).unwrap() else { panic!() };
        assert_eq!(h, sha256(&bytes), "hash correct after rollbacks");
        assert_eq!(fs::read(store.path(&h)).unwrap(), bytes);
        assert!(dir.tmp_files().is_empty());
    }

    #[test]
    fn error_in_final_chunk_is_retryable() {
        let dir = TmpDir::new();
        let store = dir.store();
        let bytes = data(50_000);
        let id = store.begin(3, 50_000).unwrap();
        let mut bad = FailAfter { data: &bytes, ok: 49_999, pos: 0 };
        assert!(store.put(id, 3, 0, 50_000, &mut bad).is_err());
        assert_eq!(store.status(id, 3), Some((0, 50_000)));
        let Put::Done(h) = store.put(id, 3, 0, 50_000, &mut &bytes[..]).unwrap() else { panic!() };
        assert_eq!(h, sha256(&bytes));
    }

    #[test]
    fn duplicate_upload_leaves_no_tmp() {
        let dir = TmpDir::new();
        let store = dir.store();
        let bytes = data(70_000);
        let h1 = upload_all(&store, 1, &bytes, 30_000);
        let h2 = upload_all(&store, 2, &bytes, 70_000);
        assert_eq!(h1, h2);
        assert!(dir.tmp_files().is_empty());
        assert_eq!(fs::read(store.path(&h1)).unwrap(), bytes);
        let sub = store.path(&h1).parent().unwrap().to_path_buf();
        assert_eq!(fs::read_dir(sub).unwrap().count(), 1);
    }

    #[test]
    fn zero_byte_upload() {
        let dir = TmpDir::new();
        let store = dir.store();
        let id = store.begin(9, 0).unwrap();
        assert_eq!(store.status(id, 9), Some((0, 0)));
        let Put::Done(h) = store.put(id, 9, 0, 0, &mut io::empty()).unwrap() else { panic!() };
        assert_eq!(h, sha256(b""));
        assert_eq!(store.open_blob(&h).unwrap().1, 0);
        assert_eq!(store.status(id, 9), None);
        assert!(dir.tmp_files().is_empty());
        // Empty put mid-upload is a harmless no-op.
        let id = store.begin(9, 4).unwrap();
        assert_eq!(store.put(id, 9, 0, 0, &mut io::empty()).unwrap(), Put::More(0));
        assert_eq!(store.put(id, 9, 0, 4, &mut &b"abcd"[..]).unwrap(), Put::Done(sha256(b"abcd")));
    }

    #[test]
    fn begin_ids_are_unique_nonzero() {
        let dir = TmpDir::new();
        let store = dir.store();
        let ids: std::collections::HashSet<u64> = (0..200).map(|_| store.begin(1, 10).unwrap()).collect();
        assert_eq!(ids.len(), 200);
        assert!(!ids.contains(&0));
        assert_eq!(dir.tmp_files().len(), 200);
    }

    #[test]
    fn gc_removes_idle_uploads_and_tmp_files() {
        let dir = TmpDir::new();
        let store = dir.store();
        let old = store.begin(1, 100).unwrap();
        store.put(old, 1, 0, 10, &mut &[0u8; 10][..]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let fresh = store.begin(1, 100).unwrap();
        assert_eq!(dir.tmp_files().len(), 2);
        store.gc_uploads(100);
        assert_eq!(store.status(old, 1), None);
        assert_eq!(store.status(fresh, 1), Some((0, 100)));
        assert_eq!(dir.tmp_files(), vec![format!("{fresh:016x}")]);
        assert_eq!(store.put(old, 1, 10, 10, &mut &[0u8; 10][..]).unwrap_err().kind(), io::ErrorKind::NotFound);
        store.gc_uploads(u64::MAX);
        assert_eq!(store.status(fresh, 1), Some((0, 100)), "nothing is older than u64::MAX");
        store.gc_uploads(0);
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.gc_uploads(0);
        assert_eq!(store.status(fresh, 1), None);
        assert!(dir.tmp_files().is_empty());
    }

    #[test]
    fn abort_removes_upload() {
        let dir = TmpDir::new();
        let store = dir.store();
        let id = store.begin(1, 100).unwrap();
        store.put(id, 1, 0, 10, &mut &[1u8; 10][..]).unwrap();
        store.abort(id);
        assert_eq!(store.status(id, 1), None);
        assert!(dir.tmp_files().is_empty());
        store.abort(id); // idempotent
        store.abort(12345);
    }

    #[test]
    fn open_deletes_leftover_tmp_files_but_keeps_blobs() {
        let dir = TmpDir::new();
        let store = dir.store();
        let h = upload_all(&store, 1, b"keep me", 100);
        let id = store.begin(1, 10).unwrap();
        assert_eq!(dir.tmp_files().len(), 1);
        drop(store);
        let store = dir.store();
        assert!(dir.tmp_files().is_empty());
        assert!(store.has(&h));
        assert_eq!(store.status(id, 1), None);
    }

    #[test]
    fn import_good_and_bad() {
        let dir = TmpDir::new();
        let store = dir.store();
        let bytes = data(BUF + 999);
        let h = sha256(&bytes);
        store.import(&h, bytes.len() as u64, &mut &bytes[..]).unwrap();
        assert!(store.has(&h));
        assert_eq!(fs::read(store.path(&h)).unwrap(), bytes);
        assert!(dir.tmp_files().is_empty());
        // Importing something we already have is fine.
        store.import(&h, bytes.len() as u64, &mut &bytes[..]).unwrap();
        assert!(dir.tmp_files().is_empty());

        let other = data(5000);
        let wrong = sha256(b"something else");
        let e = store.import(&wrong, other.len() as u64, &mut &other[..]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(!store.has(&wrong));
        assert!(!store.has(&sha256(&other)), "mismatching content is not stored under its real hash either");
        assert!(dir.tmp_files().is_empty());

        // Truncated stream and failing stream also leave nothing behind.
        let hh = sha256(&other);
        let e = store.import(&hh, other.len() as u64, &mut &other[..100]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
        let mut bad = FailAfter { data: &other, ok: 100, pos: 0 };
        assert!(store.import(&hh, other.len() as u64, &mut bad).is_err());
        assert!(!store.has(&hh));
        assert!(dir.tmp_files().is_empty());
        // Declared length shorter than the stream: only `len` bytes are consumed, hash mismatch.
        let mut cur = &other[..];
        assert!(store.import(&hh, 10, &mut cur).is_err());
        assert_eq!(cur.len(), other.len() - 10);
        // Zero-length import.
        store.import(&sha256(b""), 0, &mut io::empty()).unwrap();
        assert!(store.has(&sha256(b"")));
    }

    #[test]
    fn remove_blob() {
        let dir = TmpDir::new();
        let store = dir.store();
        let h = upload_all(&store, 1, b"remove me", 4);
        assert!(store.has(&h));
        store.remove(&h).unwrap();
        assert!(!store.has(&h));
        store.remove(&h).unwrap();
        store.remove(&[7; 32]).unwrap();
    }

    #[test]
    fn concurrent_put_on_same_id_gets_not_found() {
        let dir = TmpDir::new();
        let store = Arc::new(dir.store());
        let bytes = Arc::new(data(10_000));
        let id = store.begin(1, 10_000).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        let worker = {
            let (store, bytes) = (Arc::clone(&store), Arc::clone(&bytes));
            std::thread::spawn(move || {
                let mut gate = Gate { data: &bytes, started: Some(started_tx), go: Some(go_rx) };
                store.put(id, 1, 0, 10_000, &mut gate)
            })
        };
        started_rx.recv().unwrap(); // worker is inside put, reading
        let e = store.put(id, 1, 0, 10_000, &mut &bytes[..]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert_eq!(store.status(id, 1), Some((0, 10_000)), "status still answers while a put runs");
        assert!(dir.tmp_files().len() == 1);
        store.gc_uploads(0); // a busy upload is never collected
        assert_eq!(store.status(id, 1), Some((0, 10_000)));
        go_tx.send(()).unwrap();
        let Put::Done(h) = worker.join().unwrap().unwrap() else { panic!() };
        assert_eq!(h, sha256(&bytes));
        assert!(dir.tmp_files().is_empty());
    }

    #[test]
    fn abort_during_put_cleans_up() {
        let dir = TmpDir::new();
        let store = Arc::new(dir.store());
        let bytes = Arc::new(data(5000));
        let id = store.begin(1, 5000).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel();
        let worker = {
            let (store, bytes) = (Arc::clone(&store), Arc::clone(&bytes));
            std::thread::spawn(move || {
                let mut gate = Gate { data: &bytes, started: Some(started_tx), go: Some(go_rx) };
                store.put(id, 1, 0, 5000, &mut gate)
            })
        };
        started_rx.recv().unwrap();
        store.abort(id);
        go_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap().unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(dir.tmp_files().is_empty());
        assert!(!store.has(&sha256(&bytes)), "an aborted upload is never stored");
    }

    #[test]
    fn parallel_uploads_of_distinct_and_identical_content() {
        let dir = TmpDir::new();
        let store = Arc::new(dir.store());
        let handles: Vec<_> = (0..8usize)
            .map(|t| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    let mut hashes = Vec::new();
                    for i in 0..10usize {
                        let bytes = data(1000 + (i % 3) * 70_000 + if i % 2 == 0 { 0 } else { t });
                        hashes.push((upload_all(&store, t as u32, &bytes, 33_333), sha256(&bytes)));
                    }
                    hashes
                })
            })
            .collect();
        for h in handles {
            for (got, want) in h.join().unwrap() {
                assert_eq!(got, want);
                assert!(store.has(&want));
            }
        }
        assert!(dir.tmp_files().is_empty());
    }

    #[test]
    fn disk_free_reports_sane_numbers() {
        let (free, total) = disk_free(&std::env::temp_dir()).unwrap();
        assert!(total > 0);
        assert!(free <= total, "free {free} total {total}");
        assert!(disk_free(&std::env::temp_dir().join("definitely-missing-dir-xyz")).is_err());
    }
}
