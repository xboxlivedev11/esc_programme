//! The Logbook: append-only event log: one file per origin relay, immutable CRC-protected records,
//! positional I/O only, group-commit `sync`. Std only.
//!
//! Record (little-endian): `[u32 payload_len][u32 crc32(payload)]` then payload
//! `[u64 ts_ms][u16 kind][json bytes]`.

use crate::crypto::crc32;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

/// Record header size: payload length + CRC.
pub const HDR: usize = 8;
/// Payload prefix size: timestamp + kind.
pub const META: usize = 10;
/// Largest accepted payload (META + json).
pub const MAX_PAYLOAD: usize = 16 << 20;

/// Sequential read size used when scanning the log on open.
const SCAN_BUF: usize = 1 << 20;

/// Parsed record: total bytes, timestamp, kind, json.
type Parsed<'a> = (usize, u64, u16, &'a [u8]);

fn invalid(why: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why)
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Builds the full record bytes (header, timestamp, kind, json); panics if the payload exceeds `MAX_PAYLOAD`.
pub fn encode(ts: u64, kind: u16, json: &[u8]) -> Vec<u8> {
    let len = META + json.len();
    assert!(len <= MAX_PAYLOAD, "log record payload too large");
    let mut out = Vec::with_capacity(HDR + len);
    out.extend_from_slice(&(len as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&ts.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(json);
    let crc = crc32(&out[HDR..]);
    out[4..HDR].copy_from_slice(&crc.to_le_bytes());
    out
}

/// Parses one record at the start of `buf`: `Ok(None)` = incomplete, `Err(())` = corrupt
/// (len < META, len > MAX_PAYLOAD, or CRC mismatch).
#[allow(clippy::result_unit_err)]
pub fn decode(buf: &[u8]) -> Result<Option<Parsed<'_>>, ()> {
    if buf.len() < HDR {
        return Ok(None);
    }
    let len = le_u32(buf) as usize;
    if !(META..=MAX_PAYLOAD).contains(&len) {
        return Err(());
    }
    let total = HDR + len;
    if buf.len() < total {
        return Ok(None);
    }
    let payload = &buf[HDR..total];
    if crc32(payload) != le_u32(&buf[4..]) {
        return Err(());
    }
    let ts = u64::from_le_bytes(payload[..8].try_into().unwrap());
    let kind = u16::from_le_bytes([payload[8], payload[9]]);
    Ok(Some((total, ts, kind, &payload[META..])))
}

/// Checks that `bytes` is a whole number of valid records.
fn validate_records(mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match decode(bytes) {
            Ok(Some((total, ..))) => bytes = &bytes[total..],
            Ok(None) => return Err(invalid("partial record")),
            Err(()) => return Err(invalid("corrupt record")),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_exact_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(f, buf, off)
}

#[cfg(unix)]
fn write_all_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(f, buf, off)
}

#[cfg(windows)]
fn read_exact_at(f: &File, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_read(buf, off) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(f: &File, mut buf: &[u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match f.seek_write(buf, off) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Locks a mutex, ignoring poisoning (the guarded data is a plain counter or unit).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Scans valid records from the start of `file`, calling `f` for each; returns the offset where valid data ends.
fn scan(file: &File, file_len: u64, f: &mut impl FnMut(u64, u64, u16, &[u8])) -> io::Result<u64> {
    let mut buf = vec![0u8; SCAN_BUF];
    // buf[start..have] is unconsumed; buf[0] sits at file offset `base`.
    let (mut base, mut start, mut have) = (0u64, 0usize, 0usize);
    loop {
        match decode(&buf[start..have]) {
            Ok(Some((total, ts, kind, json))) => {
                f(base + start as u64, ts, kind, json);
                start += total;
            }
            Ok(None) => {
                base += start as u64;
                buf.copy_within(start..have, 0);
                have -= start;
                start = 0;
                // decode() already validated the length once the header is present.
                let want = if have >= HDR { HDR + le_u32(&buf) as usize } else { HDR };
                if want > buf.len() {
                    buf.resize(want, 0);
                }
                let remaining = file_len - (base + have as u64);
                if remaining == 0 {
                    return Ok(base); // torn tail (or clean end)
                }
                let n = remaining.min((buf.len() - have) as u64) as usize;
                read_exact_at(file, &mut buf[have..have + n], base + have as u64)?;
                have += n;
            }
            Err(()) => return Ok(base + start as u64),
        }
    }
}

/// An append-only record log backed by one file; shareable across threads.
pub struct Log {
    file: File,
    /// Logical end: all appended bytes. Advanced only after the bytes are written.
    end: AtomicU64,
    /// Bytes known durable.
    synced: AtomicU64,
    /// Serializes appends.
    append: Mutex<()>,
    /// Serializes fsyncs so one serves many callers.
    sync: Mutex<()>,
}

impl Log {
    /// Opens or creates the log, calling `f(offset, ts, kind, json)` per valid record in order;
    /// truncates a torn or corrupt tail and returns `(log, bytes_dropped)`.
    pub fn open(path: &Path, mut f: impl FnMut(u64, u64, u16, &[u8])) -> io::Result<(Log, u64)> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let file_len = file.metadata()?.len();
        let valid = scan(&file, file_len, &mut f)?;
        let dropped = file_len - valid;
        if dropped > 0 {
            file.set_len(valid)?;
        }
        // Whatever we read back may not have been fsynced by a previous run in this boot; make it so.
        file.sync_all()?;
        let log = Log {
            file,
            end: AtomicU64::new(valid),
            synced: AtomicU64::new(valid),
            append: Mutex::new(()),
            sync: Mutex::new(()),
        };
        Ok((log, dropped))
    }

    /// Logical end offset: all appended bytes.
    pub fn end(&self) -> u64 {
        self.end.load(Ordering::Acquire)
    }

    /// Bytes known durable.
    pub fn synced(&self) -> u64 {
        self.synced.load(Ordering::Acquire)
    }

    /// Appends one record from `encode()` (written to the OS, not fsynced); returns its offset.
    pub fn append(&self, rec: &[u8]) -> io::Result<u64> {
        match decode(rec) {
            Ok(Some((total, ..))) if total == rec.len() => {}
            _ => return Err(invalid("not exactly one valid record")),
        }
        self.write_at_end(None, rec)
    }

    /// Appends whole valid records copied from a peer; `at` must equal `end()`. Returns the new end.
    pub fn append_raw(&self, at: u64, bytes: &[u8]) -> io::Result<u64> {
        validate_records(bytes)?;
        self.write_at_end(Some(at), bytes)?;
        Ok(self.end())
    }

    /// Writes already-validated `bytes` at the end (checking `expect` if given); returns the offset written.
    fn write_at_end(&self, expect: Option<u64>, bytes: &[u8]) -> io::Result<u64> {
        let _guard = lock(&self.append);
        let at = self.end.load(Ordering::Relaxed);
        if expect.is_some_and(|e| e != at) {
            return Err(invalid("append offset does not match log end"));
        }
        write_all_at(&self.file, bytes, at)?;
        self.end.store(at + bytes.len() as u64, Ordering::Release);
        Ok(at)
    }

    /// Group commit: makes everything up to `upto` durable; returns at once if already covered.
    pub fn sync(&self, upto: u64) -> io::Result<()> {
        if self.synced() >= upto {
            return Ok(());
        }
        let _guard = lock(&self.sync);
        if self.synced() >= upto {
            return Ok(());
        }
        // Everything below `target` was fully written before this call, so the fsync covers it.
        let target = self.end();
        self.file.sync_data()?;
        self.synced.fetch_max(target, Ordering::AcqRel);
        Ok(())
    }

    /// Reads the record at `off`: `(ts, kind, json)`; `InvalidData` if `off` is not a valid record start.
    pub fn read(&self, off: u64) -> io::Result<(u64, u16, Vec<u8>)> {
        let end = self.end();
        if off.checked_add(HDR as u64).is_none_or(|h| h > end) {
            return Err(invalid("offset past end of log"));
        }
        let mut hdr = [0u8; HDR];
        read_exact_at(&self.file, &mut hdr, off)?;
        let len = le_u32(&hdr) as usize;
        if !(META..=MAX_PAYLOAD).contains(&len) {
            return Err(invalid("bad record length"));
        }
        if off + (HDR + len) as u64 > end {
            return Err(invalid("record extends past end of log"));
        }
        let mut payload = vec![0u8; len];
        read_exact_at(&self.file, &mut payload, off + HDR as u64)?;
        if crc32(&payload) != le_u32(&hdr[4..]) {
            return Err(invalid("record crc mismatch"));
        }
        let ts = u64::from_le_bytes(payload[..8].try_into().unwrap());
        let kind = u16::from_le_bytes([payload[8], payload[9]]);
        payload.drain(..META);
        Ok((ts, kind, payload))
    }

    /// Up to `max` raw bytes from `from`, cut at the last whole-record boundary; if the first record alone
    /// exceeds `max`, returns exactly that record. Empty when `from == end()`. For replication
    /// (headers are sanity-checked, CRCs are left to the receiver's `append_raw`).
    pub fn read_raw(&self, from: u64, max: usize) -> io::Result<Vec<u8>> {
        let end = self.end();
        if from == end {
            return Ok(Vec::new());
        }
        if from > end {
            return Err(invalid("offset past end of log"));
        }
        let window = (end - from).min(max as u64) as usize;
        let mut buf = vec![0u8; window];
        read_exact_at(&self.file, &mut buf, from)?;
        // Walk record headers to find the last boundary inside the window.
        let mut pos = 0;
        while buf.len() - pos >= HDR {
            let len = le_u32(&buf[pos..]) as usize;
            if !(META..=MAX_PAYLOAD).contains(&len) {
                return Err(invalid("bad record length"));
            }
            if pos + HDR + len > buf.len() {
                break;
            }
            pos += HDR + len;
        }
        if pos > 0 {
            buf.truncate(pos);
            return Ok(buf);
        }
        // The first record does not fit in `max`: return it alone so the caller always progresses.
        let mut hdr = [0u8; HDR];
        if end - from < HDR as u64 {
            return Err(invalid("partial record at end of log"));
        }
        read_exact_at(&self.file, &mut hdr, from)?;
        let len = le_u32(&hdr) as usize;
        if !(META..=MAX_PAYLOAD).contains(&len) {
            return Err(invalid("bad record length"));
        }
        let total = HDR + len;
        if total as u64 > end - from {
            return Err(invalid("record extends past end of log"));
        }
        let mut rec = vec![0u8; total];
        read_exact_at(&self.file, &mut rec, from)?;
        Ok(rec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// Temp file path removed on drop.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new(name: &str) -> Tmp {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            Tmp(std::env::temp_dir().join(format!("esc-log-test-{}-{n}-{name}", std::process::id())))
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn file_len(p: &Path) -> u64 {
        std::fs::metadata(p).unwrap().len()
    }

    /// A scanned record: (offset, ts, kind, json).
    type Rec = (u64, u64, u16, Vec<u8>);

    /// Opens collecting every scanned record.
    fn open_all(p: &Path) -> (Log, u64, Vec<Rec>) {
        let mut recs = Vec::new();
        let (log, dropped) = Log::open(p, |o, t, k, j| recs.push((o, t, k, j.to_vec()))).unwrap();
        (log, dropped, recs)
    }

    fn json_for(i: u64) -> Vec<u8> {
        format!("{{\"n\":{i},\"pad\":\"{}\"}}", "x".repeat((i % 37) as usize)).into_bytes()
    }

    /// Writes `n` varied records and returns their offsets and the log's end.
    fn fill(p: &Path, n: u64) -> (Vec<u64>, u64) {
        let (log, dropped) = Log::open(p, |_, _, _, _| panic!("expected empty log")).unwrap();
        assert_eq!(dropped, 0);
        let offs: Vec<u64> = (0..n).map(|i| log.append(&encode(1000 + i, (i % 7) as u16, &json_for(i))).unwrap()).collect();
        log.sync(log.end()).unwrap();
        (offs, log.end())
    }

    #[test]
    fn log_is_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<Log>();
    }

    #[test]
    fn encode_decode_round_trip() {
        let rec = encode(0x0102_0304_0506_0708, 0xABCD, b"{\"a\":1}");
        assert_eq!(rec.len(), HDR + META + 7);
        assert_eq!(le_u32(&rec) as usize, META + 7);
        assert_eq!(le_u32(&rec[4..]), crc32(&rec[HDR..]));
        assert_eq!(&rec[HDR..HDR + 8], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&rec[HDR + 8..HDR + 10], &0xABCDu16.to_le_bytes());
        assert_eq!(decode(&rec), Ok(Some((rec.len(), 0x0102_0304_0506_0708, 0xABCD, &b"{\"a\":1}"[..]))));
        // Trailing bytes belong to the next record.
        let mut two = rec.clone();
        two.extend_from_slice(&encode(5, 6, b""));
        assert_eq!(decode(&two).unwrap().unwrap().0, rec.len());
        assert_eq!(decode(&encode(5, 6, b"")), Ok(Some((HDR + META, 5, 6, &b""[..]))));
    }

    #[test]
    fn decode_incomplete_and_corrupt() {
        let rec = encode(7, 8, b"hello world");
        for cut in 0..rec.len() {
            assert_eq!(decode(&rec[..cut]), Ok(None), "cut {cut}");
        }
        // Every single-bit flip is detected (or makes the record incomplete via the length field).
        for bit in 0..rec.len() * 8 {
            let mut bad = rec.clone();
            bad[bit / 8] ^= 1 << (bit % 8);
            assert_ne!(decode(&bad), Ok(Some((rec.len(), 7, 8, &b"hello world"[..]))), "bit {bit}");
            if bit / 8 >= 4 {
                assert_eq!(decode(&bad), Err(()), "bit {bit}");
            }
        }
        let mut short = vec![0u8; HDR + 20];
        short[..4].copy_from_slice(&((META - 1) as u32).to_le_bytes());
        assert_eq!(decode(&short), Err(()), "len < META");
        short[..4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode(&short), Err(()), "len 0");
        short[..4].copy_from_slice(&((MAX_PAYLOAD + 1) as u32).to_le_bytes());
        assert_eq!(decode(&short[..HDR]), Err(()), "len > MAX_PAYLOAD, header only");
        short[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(&short), Err(()), "len u32::MAX");
    }

    #[test]
    fn max_payload_record_is_accepted() {
        let json = vec![b'j'; MAX_PAYLOAD - META];
        let rec = encode(1, 2, &json);
        assert_eq!(decode(&rec).unwrap().unwrap().3.len(), json.len());
        let t = Tmp::new("big");
        let (log, _) = Log::open(&t.0, |_, _, _, _| {}).unwrap();
        log.append(&encode(9, 9, b"{}")).unwrap();
        let off = log.append(&rec).unwrap();
        log.append(&encode(10, 10, b"{}")).unwrap();
        drop(log);
        let (log, dropped, recs) = open_all(&t.0);
        assert_eq!((dropped, recs.len()), (0, 3));
        assert_eq!(recs[1].0, off);
        assert_eq!(recs[1].3.len(), json.len());
        assert_eq!(log.read(off).unwrap().2.len(), json.len());
    }

    #[test]
    fn empty_open() {
        let t = Tmp::new("empty");
        let (log, dropped, recs) = open_all(&t.0);
        assert_eq!((dropped, recs.len(), log.end(), log.synced()), (0, 0, 0, 0));
        assert_eq!(log.read_raw(0, 100).unwrap(), Vec::<u8>::new());
        assert!(log.read(0).is_err());
        log.sync(0).unwrap();
        drop(log);
        assert_eq!(file_len(&t.0), 0);
        let (log, dropped, _) = open_all(&t.0);
        assert_eq!((dropped, log.end()), (0, 0));
    }

    #[test]
    fn thousand_records_reopen() {
        let t = Tmp::new("thousand");
        let (offs, end) = fill(&t.0, 1000);
        assert_eq!(file_len(&t.0), end);
        let (log, dropped, recs) = open_all(&t.0);
        assert_eq!((dropped, log.end(), log.synced()), (0, end, end));
        assert_eq!(recs.len(), 1000);
        for (i, (rec, off)) in recs.iter().zip(&offs).enumerate() {
            let i = i as u64;
            assert_eq!(rec, &(*off, 1000 + i, (i % 7) as u16, json_for(i)));
            assert_eq!(log.read(*off).unwrap(), (1000 + i, (i % 7) as u16, json_for(i)));
        }
        // Not a record start.
        assert!(log.read(offs[1] + 1).is_err());
        assert!(log.read(end).is_err());
        assert!(log.read(end + 100).is_err());
        assert!(log.read(u64::MAX).is_err());
    }

    #[test]
    fn scan_handles_records_spanning_buffer_refills() {
        // Enough data to need several 1 MiB refills, with record sizes that straddle boundaries.
        let t = Tmp::new("span");
        let (log, _) = Log::open(&t.0, |_, _, _, _| {}).unwrap();
        let mut n = 0u64;
        while log.end() < 3 * SCAN_BUF as u64 + 12345 {
            log.append(&encode(n, 1, &vec![b'a' + (n % 26) as u8; (n as usize * 7919) % 70_000])).unwrap();
            n += 1;
        }
        let end = log.end();
        drop(log);
        let mut seen = 0u64;
        let (log, dropped) = Log::open(&t.0, |_, ts, _, j| {
            assert_eq!(ts, seen);
            assert_eq!(j.len(), (seen as usize * 7919) % 70_000);
            seen += 1;
        })
        .unwrap();
        assert_eq!((dropped, seen, log.end()), (0, n, end));
    }

    #[test]
    fn torn_tail_every_cut_point() {
        let src = Tmp::new("torn-src");
        let (offs, end) = fill(&src.0, 10);
        let last = *offs.last().unwrap();
        for cut in last..end {
            let t = Tmp::new("torn");
            std::fs::copy(&src.0, &t.0).unwrap();
            std::fs::OpenOptions::new().write(true).open(&t.0).unwrap().set_len(cut).unwrap();
            let (log, dropped, recs) = open_all(&t.0);
            assert_eq!(dropped, cut - last, "cut {cut}");
            assert_eq!(recs.len(), 9, "cut {cut}");
            assert_eq!(log.end(), last);
            assert_eq!(file_len(&t.0), last, "file truncated, cut {cut}");
            for (i, r) in recs.iter().enumerate() {
                assert_eq!(r.3, json_for(i as u64));
            }
            let off = log.append(&encode(5000, 3, b"{\"next\":true}")).unwrap();
            assert_eq!(off, last, "next append lands at truncated end");
            drop(log);
            let (_, dropped, recs) = open_all(&t.0);
            assert_eq!((dropped, recs.len()), (0, 10));
            assert_eq!(recs[9].3, b"{\"next\":true}");
        }
    }

    #[test]
    fn flipped_crc_bit_drops_last_record() {
        let t = Tmp::new("crcflip");
        let (offs, end) = fill(&t.0, 5);
        let last = offs[4];
        let mut bytes = std::fs::read(&t.0).unwrap();
        bytes[last as usize + 4] ^= 0x01; // crc field
        std::fs::write(&t.0, &bytes).unwrap();
        let (log, dropped, recs) = open_all(&t.0);
        assert_eq!((dropped, recs.len(), log.end()), (end - last, 4, last));
        // Corruption in the middle drops it and everything after.
        drop(log);
        std::fs::remove_file(&t.0).unwrap();
        let (offs, _) = fill(&t.0, 5);
        let mut bytes = std::fs::read(&t.0).unwrap();
        bytes[offs[2] as usize + HDR + META] ^= 0x80; // payload byte of record 2
        std::fs::write(&t.0, &bytes).unwrap();
        let (log, dropped, recs) = open_all(&t.0);
        assert_eq!((recs.len(), log.end()), (2, offs[2]));
        assert_eq!(dropped, bytes.len() as u64 - offs[2]);
    }

    #[test]
    fn garbage_tail_with_huge_length_is_dropped_without_allocating() {
        let t = Tmp::new("garbage");
        let (_, end) = fill(&t.0, 3);
        let mut bytes = std::fs::read(&t.0).unwrap();
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3]);
        std::fs::write(&t.0, &bytes).unwrap();
        let (log, dropped, recs) = open_all(&t.0);
        assert_eq!((dropped, recs.len(), log.end()), (7, 3, end));
    }

    #[test]
    fn append_rejects_non_records() {
        let t = Tmp::new("badappend");
        let (log, _) = Log::open(&t.0, |_, _, _, _| {}).unwrap();
        let rec = encode(1, 1, b"{}");
        assert!(log.append(&rec[..rec.len() - 1]).is_err());
        let mut two = rec.clone();
        two.extend_from_slice(&rec);
        assert!(log.append(&two).is_err());
        assert!(log.append(b"").is_err());
        assert_eq!((log.end(), file_len(&t.0)), (0, 0));
    }

    #[test]
    fn append_raw_replicates_byte_identical() {
        let a = Tmp::new("raw-a");
        let b = Tmp::new("raw-b");
        let (_, end) = fill(&a.0, 300);
        let (src, _, _) = open_all(&a.0);
        let (dst, _) = Log::open(&b.0, |_, _, _, _| {}).unwrap();
        let mut at = 0;
        let mut batches = 0;
        while at < src.end() {
            let chunk = src.read_raw(at, 1000).unwrap();
            assert!(!chunk.is_empty());
            at = dst.append_raw(at, &chunk).unwrap();
            batches += 1;
        }
        assert!(batches > 5);
        assert_eq!((at, dst.end()), (end, end));
        dst.sync(end).unwrap();
        assert_eq!(std::fs::read(&a.0).unwrap(), std::fs::read(&b.0).unwrap());
        assert_eq!(dst.append_raw(end, b"").unwrap(), end);
        // Replicated log reopens cleanly.
        drop(dst);
        let (_, dropped, recs) = open_all(&b.0);
        assert_eq!((dropped, recs.len()), (0, 300));
    }

    #[test]
    fn append_raw_rejections_write_nothing() {
        let t = Tmp::new("raw-bad");
        let (log, _) = Log::open(&t.0, |_, _, _, _| {}).unwrap();
        let r1 = encode(1, 1, b"{\"a\":1}");
        let r2 = encode(2, 2, b"{\"b\":2}");
        let both = [r1.clone(), r2.clone()].concat();
        assert_eq!(log.append_raw(0, &r1).unwrap(), r1.len() as u64);
        let end = log.end();
        let mut flipped = r2.clone();
        flipped[HDR + 3] ^= 1;
        let cases: Vec<(u64, Vec<u8>, &str)> = vec![
            (end + 1, r2.clone(), "offset too far"),
            (end - 1, r2.clone(), "offset too small"),
            (0, r2.clone(), "offset zero"),
            (end, r2[..r2.len() - 1].to_vec(), "partial record"),
            (end, r2[..3].to_vec(), "partial header"),
            (end, [r2.clone(), r1[..5].to_vec()].concat(), "valid then partial"),
            (end, [r2.clone(), vec![0; 20]].concat(), "valid then garbage"),
            (end, flipped, "bad crc"),
            (end, vec![0xFF; 64], "garbage"),
            (end, both[..both.len() - 2].to_vec(), "second record truncated"),
        ];
        for (at, bytes, what) in cases {
            let e = log.append_raw(at, &bytes).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{what}");
            assert_eq!((log.end(), file_len(&t.0)), (end, end), "{what}: nothing written");
        }
        assert_eq!(log.append_raw(end, &r2).unwrap(), end + r2.len() as u64);
    }

    #[test]
    fn read_raw_boundaries() {
        let t = Tmp::new("rawread");
        let (offs, end) = fill(&t.0, 20);
        let (log, _, _) = open_all(&t.0);
        let all = std::fs::read(&t.0).unwrap();
        assert_eq!(log.read_raw(end, 1000).unwrap(), Vec::<u8>::new());
        assert!(log.read_raw(end + 1, 1000).is_err());
        let rec_len = |i: usize| (offs.get(i + 1).copied().unwrap_or(end) - offs[i]) as usize;
        // max smaller than one record: exactly that one record, from every record start.
        for i in 0..20 {
            for max in [0, 1, 7, 8, 9, rec_len(i) - 1] {
                let got = log.read_raw(offs[i], max).unwrap();
                assert_eq!(got, &all[offs[i] as usize..offs[i] as usize + rec_len(i)], "rec {i} max {max}");
            }
        }
        // Exact boundaries: max == k whole records, and one byte either side.
        for k in 1..=5usize {
            let size = (offs[k] - offs[0]) as usize;
            assert_eq!(log.read_raw(0, size).unwrap(), &all[..size], "exact {k}");
            assert_eq!(log.read_raw(0, size + 1).unwrap(), &all[..size], "exact+1 {k}");
            // One byte short: drops the last record, unless that leaves nothing (then the first record alone).
            let short_end = if k == 1 { offs[1] } else { offs[k - 1] } as usize;
            assert_eq!(log.read_raw(0, size - 1).unwrap(), &all[..short_end], "exact-1 {k}");
        }
        // Huge max returns everything, and chunks concatenate to the file.
        assert_eq!(log.read_raw(0, usize::MAX).unwrap(), all);
        let mut got = Vec::new();
        while (got.len() as u64) < end {
            got.extend(log.read_raw(got.len() as u64, 150).unwrap());
        }
        assert_eq!(got, all);
        assert!(log.read_raw(end - 3, 100).is_err(), "partial header at end");
    }

    #[test]
    fn sync_tracks_progress() {
        let t = Tmp::new("sync");
        let (log, _) = Log::open(&t.0, |_, _, _, _| {}).unwrap();
        assert_eq!(log.synced(), 0);
        let rec = encode(1, 1, b"{}");
        let off = log.append(&rec).unwrap();
        assert_eq!((log.end(), log.synced()), (rec.len() as u64, 0));
        log.sync(off + rec.len() as u64).unwrap();
        assert_eq!(log.synced(), rec.len() as u64);
        log.sync(1).unwrap(); // already covered
        log.append(&rec).unwrap();
        assert_eq!(log.synced(), rec.len() as u64);
        log.sync(log.end()).unwrap();
        assert_eq!(log.synced(), log.end());
    }

    #[test]
    fn concurrent_appends_with_group_commit() {
        const THREADS: usize = 8;
        const PER: usize = 500;
        let t = Tmp::new("conc");
        let (log, _) = Log::open(&t.0, |_, _, _, _| {}).unwrap();
        let log = Arc::new(log);
        let handles: Vec<_> = (0..THREADS)
            .map(|th| {
                let log = Arc::clone(&log);
                std::thread::spawn(move || {
                    let mut offs = Vec::with_capacity(PER);
                    for seq in 0..PER {
                        let json = format!("{{\"t\":{th},\"s\":{seq},\"pad\":\"{}\"}}", "p".repeat((th * 13 + seq) % 90));
                        let rec = encode((th * 1_000_000 + seq) as u64, th as u16, json.as_bytes());
                        let off = log.append(&rec).unwrap();
                        log.sync(off + rec.len() as u64).unwrap();
                        assert!(log.synced() >= off + rec.len() as u64);
                        offs.push(off);
                    }
                    offs
                })
            })
            .collect();
        let mut all_offs: Vec<(u64, usize, usize)> = Vec::new();
        for (th, h) in handles.into_iter().enumerate() {
            for (seq, off) in h.join().unwrap().into_iter().enumerate() {
                all_offs.push((off, th, seq));
            }
        }
        let end = log.end();
        assert_eq!(log.synced(), end);
        drop(log);

        let (log, dropped, recs) = open_all(&t.0);
        assert_eq!((dropped, recs.len(), log.end()), (0, THREADS * PER, end));
        all_offs.sort_unstable();
        let mut next_seq = [0usize; THREADS];
        for (rec, expect) in recs.iter().zip(&all_offs) {
            let (off, ts, kind, json) = rec;
            let (e_off, th, seq) = *expect;
            assert_eq!((*off, *kind as usize, *ts as usize), (e_off, th, th * 1_000_000 + seq), "scan matches returned offsets");
            let text = std::str::from_utf8(json).unwrap();
            let want = format!("{{\"t\":{th},\"s\":{seq},\"pad\":\"{}\"}}", "p".repeat((th * 13 + seq) % 90));
            assert_eq!(text, want, "no interleaving or corruption");
            assert_eq!(seq, next_seq[th], "per-thread order preserved");
            next_seq[th] += 1;
            assert_eq!(log.read(*off).unwrap().2, json.clone());
        }
        assert_eq!(next_seq, [PER; THREADS]);
    }
}
