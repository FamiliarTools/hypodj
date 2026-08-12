//! The COVER STORE: an on-disk cache of cover-art bytes, so a cover is fetched from
//! the server ONCE ever rather than once per daemon lifetime.
//!
//! The handler already holds a small in-memory [`crate::cache::TtlLru`] of decoded
//! covers, which is what keeps ncmpcpp's many small `albumart` offset chunks from
//! re-fetching the same image. That cache is 64 entries with a 10 minute TTL, so a
//! cover leaves it after ten minutes and EVERY cover is gone on restart - and the
//! miss is paid on the art pane's critical path, which is the "the cover takes a few
//! seconds" the TUI shows. This module is the layer under it: the bytes land next to
//! the mirrored audio, survive a restart, and the memory cache becomes a hot window
//! over a warm disk rather than the only thing between the pane and the network.
//!
//! ## Self-describing entries, so a hash collision cannot serve the wrong cover
//!
//! A file is named for a 64-bit hash of its cache key, which is not wide enough to
//! bet an image on by itself. So each file CARRIES its key: the payload is
//! [`MAGIC`], the key, a newline, then the raw image bytes. A read that finds a
//! different key is a MISS, not a wrong picture - the two-line cost of turning a
//! collision (and a stale file from a changed key scheme) into a re-fetch.
//!
//! ## Sweeping deletes only its own files
//!
//! Eviction is by oldest mtime down to the budget, and it only ever considers files
//! matching [`SUFFIX`]. Unlike the audio store this directory is therefore NOT owned
//! exclusively and needs no ownership marker: pointed at a directory full of someone
//! else's files it caches beside them and deletes none of them. A cover is pure
//! derived data - losing the whole directory costs a re-fetch and nothing else -
//! so the store is deliberately the cheapest thing that can be correct.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// File header, so a truncated / foreign / old-format file is a clean miss.
const MAGIC: &[u8] = b"HDJCOVER1\n";

/// Extension every entry carries. The sweep considers nothing else, which is what
/// makes the directory safe to share.
const SUFFIX: &str = ".cov";

/// Default byte budget: 256 MiB. A cover is tens to a few hundred KiB, so this holds
/// a library's worth of art and still cannot meaningfully compete with the offline
/// AUDIO store (gibibytes) for the disk.
pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Refuse to cache anything larger than this. The same 8 MiB ceiling the TUI's
/// fetch path applies - past it something is wrong with the source, not with us.
const MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;

/// A disk-backed cover cache. Cheap to clone-share behind an `Arc`; every method is
/// best-effort and infallible from the caller's side, because a cover that fails to
/// cache must degrade to a plain network fetch and never to an error the user sees.
pub struct CoverStore {
    root: PathBuf,
    max_bytes: u64,
    /// Bytes currently on disk, seeded by the open-time scan and maintained by
    /// [`Self::put`]. Approximate by design: it is a sweep TRIGGER, and the sweep
    /// itself re-measures from the directory.
    bytes: AtomicU64,
}

impl CoverStore {
    /// Open (creating) the cover directory. Fails only when the directory cannot
    /// exist at all; the caller treats that as "run without a cover store".
    pub fn open(root: PathBuf, max_bytes: u64) -> std::io::Result<CoverStore> {
        std::fs::create_dir_all(&root)?;
        let bytes = scan_bytes(&root);
        Ok(CoverStore { root, max_bytes: max_bytes.max(1024 * 1024), bytes: AtomicU64::new(bytes) })
    }

    /// Bytes currently accounted for on disk (test/diagnostic accessor).
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The image bytes cached for `key`, or `None` on any miss - absent, unreadable,
    /// truncated, or holding a DIFFERENT key (see the module header).
    ///
    /// A hit refreshes the file's mtime, which is the recency the sweep evicts by.
    /// Best-effort: a failed touch costs the entry an early eviction, nothing more.
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let path = self.path(key);
        let mut f = std::fs::File::open(&path).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        let body = decode(&buf, key)?;
        // Touch for LRU. `File::set_times` needs no second open and no libc.
        let now = std::fs::FileTimes::new().set_accessed(std::time::SystemTime::now());
        let _ = f.set_times(now);
        Some(body)
    }

    /// Cache `bytes` under `key`, then sweep if the directory is over budget.
    ///
    /// Written to a temp file and RENAMED, so a concurrent reader sees either the
    /// old entry or the whole new one and never a half-written image. Silently does
    /// nothing on an empty or oversized payload, or on any IO failure.
    pub fn put(&self, key: &str, bytes: &[u8]) {
        if bytes.is_empty() || bytes.len() > MAX_ENTRY_BYTES {
            return;
        }
        let path = self.path(key);
        // The temp name carries the pid so two daemons on one directory (a live one
        // and a probe) cannot collide mid-write.
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let mut encoded = Vec::with_capacity(MAGIC.len() + key.len() + 1 + bytes.len());
        encoded.extend_from_slice(MAGIC);
        encoded.extend_from_slice(key.as_bytes());
        encoded.push(b'\n');
        encoded.extend_from_slice(bytes);
        let written = (|| -> std::io::Result<u64> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&encoded)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, &path)?;
            Ok(encoded.len() as u64)
        })();
        match written {
            Ok(n) => {
                if self.bytes.fetch_add(n, Ordering::Relaxed) + n > self.max_bytes {
                    self.sweep();
                }
            }
            Err(_) => {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    /// Evict oldest-accessed entries until the directory is back under budget.
    ///
    /// Re-measures from the directory rather than trusting the counter, so a stale
    /// count (a crash mid-put, an entry deleted underneath us) self-heals here.
    /// Only [`SUFFIX`] files are ever considered, let alone removed.
    pub fn sweep(&self) {
        let mut entries: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
        let mut total: u64 = 0;
        let Ok(rd) = std::fs::read_dir(&self.root) else { return };
        for e in rd.flatten() {
            let path = e.path();
            if !is_entry(&path) {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            let when = md.accessed().or_else(|_| md.modified()).unwrap_or(std::time::UNIX_EPOCH);
            total += md.len();
            entries.push((when, md.len(), path));
        }
        if total > self.max_bytes {
            // Oldest first, deleting until under budget.
            entries.sort_by_key(|(when, _, _)| *when);
            for (_, len, path) in entries {
                if total <= self.max_bytes {
                    break;
                }
                if std::fs::remove_file(&path).is_ok() {
                    total = total.saturating_sub(len);
                }
            }
        }
        self.bytes.store(total, Ordering::Relaxed);
    }

    /// Where `key` lives: the hash of the key, hex, plus [`SUFFIX`]. The key itself
    /// is never a filename - a cover key can be a whole remote URL, which is neither
    /// a legal nor a length-bounded path component.
    fn path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{:016x}{SUFFIX}", fnv1a64(key.as_bytes())))
    }
}

/// Is `path` one of ours? The sweep's entire safety argument.
fn is_entry(path: &Path) -> bool {
    path.is_file()
        && path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(SUFFIX))
}

/// Total bytes of our entries under `root`. Zero for an unreadable directory.
fn scan_bytes(root: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(root) else { return 0 };
    rd.flatten()
        .filter(|e| is_entry(&e.path()))
        .filter_map(|e| e.metadata().ok().map(|m| m.len()))
        .sum()
}

/// Validate a file's header + key and return the image bytes, or `None` for any
/// shape that is not exactly an entry for `key`. Pure, so the collision and
/// truncation cases are unit-testable without a filesystem.
fn decode(raw: &[u8], key: &str) -> Option<Vec<u8>> {
    let rest = raw.strip_prefix(MAGIC)?;
    let nl = rest.iter().position(|b| *b == b'\n')?;
    if &rest[..nl] != key.as_bytes() {
        return None;
    }
    let body = &rest[nl + 1..];
    if body.is_empty() {
        return None;
    }
    Some(body.to_vec())
}

/// FNV-1a, 64-bit. A filename needs a stable, dependency-free spread, not a
/// cryptographic hash - collisions are handled by the key check in [`decode`], so
/// the only thing riding on this function is how evenly the directory fills.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let p = std::env::temp_dir()
            .join(format!("hypodj-cover-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn round_trips_bytes_through_the_disk() {
        let root = tmpdir("roundtrip");
        let s = CoverStore::open(root.clone(), DEFAULT_MAX_BYTES).unwrap();
        assert_eq!(s.get("cover/abc"), None, "cold is a miss");
        s.put("cover/abc", b"\xff\xd8\xff-jpeg-bytes");
        assert_eq!(s.get("cover/abc").as_deref(), Some(&b"\xff\xd8\xff-jpeg-bytes"[..]));

        // And it survives a reopen - the whole point over the in-memory cache.
        let again = CoverStore::open(root.clone(), DEFAULT_MAX_BYTES).unwrap();
        assert_eq!(again.get("cover/abc").as_deref(), Some(&b"\xff\xd8\xff-jpeg-bytes"[..]));
        assert!(again.bytes() > 0, "the reopen scan accounts for the entry");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_key_mismatch_in_the_same_file_is_a_miss_not_a_wrong_cover() {
        // The filename is a 64-bit hash, so this is the case that decides whether a
        // collision serves someone else's album art or costs a re-fetch.
        let raw = {
            let mut v = MAGIC.to_vec();
            v.extend_from_slice(b"cover/other\n");
            v.extend_from_slice(b"IMAGE");
            v
        };
        assert_eq!(decode(&raw, "cover/other").as_deref(), Some(&b"IMAGE"[..]));
        assert_eq!(decode(&raw, "cover/mine"), None, "a different key must MISS");
    }

    #[test]
    fn a_truncated_or_foreign_file_is_a_miss() {
        assert_eq!(decode(b"", "k"), None);
        assert_eq!(decode(b"not ours at all", "k"), None);
        assert_eq!(decode(MAGIC, "k"), None, "header but no key line");
        let mut no_body = MAGIC.to_vec();
        no_body.extend_from_slice(b"k\n");
        assert_eq!(decode(&no_body, "k"), None, "key but no image bytes");
    }

    #[test]
    fn sweeping_evicts_oldest_first_down_to_the_budget() {
        let root = tmpdir("sweep");
        // A budget the floor cannot raise past what these entries exceed.
        let s = CoverStore::open(root.clone(), 4 * 1024 * 1024).unwrap();
        let big = vec![7u8; 1024 * 1024];
        for i in 0..8 {
            s.put(&format!("cover/{i}"), &big);
            // Distinct access times, so "oldest first" is a real ordering and not a
            // tie the filesystem breaks arbitrarily.
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        s.sweep();
        assert!(s.bytes() <= 4 * 1024 * 1024, "swept to budget, got {}", s.bytes());
        assert!(s.get("cover/7").is_some(), "the newest entry survives");
        assert!(s.get("cover/0").is_none(), "the oldest entry was evicted");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_sweep_never_touches_a_file_that_is_not_ours() {
        let root = tmpdir("strangers");
        let stranger = root.join("someones-notes.txt");
        std::fs::write(&stranger, vec![3u8; 2 * 1024 * 1024]).unwrap();
        let s = CoverStore::open(root.clone(), 1024 * 1024).unwrap();
        s.put("cover/a", &vec![1u8; 900 * 1024]);
        s.put("cover/b", &vec![2u8; 900 * 1024]);
        s.sweep();
        assert!(stranger.exists(), "a foreign file is never swept");
        assert_eq!(
            std::fs::metadata(&stranger).unwrap().len(),
            2 * 1024 * 1024,
            "and never counted against the budget either"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_oversized_payload_is_declined_rather_than_stored() {
        let root = tmpdir("oversize");
        let s = CoverStore::open(root.clone(), DEFAULT_MAX_BYTES).unwrap();
        s.put("cover/huge", &vec![0u8; MAX_ENTRY_BYTES + 1]);
        assert_eq!(s.get("cover/huge"), None);
        s.put("cover/empty", b"");
        assert_eq!(s.get("cover/empty"), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
