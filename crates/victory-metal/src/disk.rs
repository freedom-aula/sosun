//! Metal libraries kept on disk: the converter's output (a metallib and its shader stage) for a
//! DXIL container, its root layout and the converter itself, so a later run skips the conversion.
//! One file per key, written to a temporary name and renamed (a reader sees all of it or none);
//! a file that is damaged, or made by another format, reads as a miss. The folder is the user's
//! cache folder (`victory_gpu::shader_cache::use_native_cache`), never the repo.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Names the file format: part of every key, and the start of every file.
const MAGIC: &[u8; 4] = b"VML1";

static DIR: OnceLock<PathBuf> = OnceLock::new();
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

/// Keeps converted libraries in `dir` from now on.
pub fn use_dir(dir: PathBuf) {
    match std::fs::create_dir_all(&dir) {
        Ok(()) => {
            let _ = DIR.set(dir);
        }
        Err(e) => eprintln!("metal: no library cache at {}: {e}", dir.display()),
    }
}

/// The folder, if libraries are kept.
pub fn dir() -> Option<&'static Path> {
    DIR.get().map(PathBuf::as_path)
}

/// The key of a conversion: every part that its output depends on.
pub fn key(parts: &[&[u8]]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(MAGIC);
    for p in parts {
        h.update(&(p.len() as u64).to_le_bytes());
        h.update(p);
    }
    h.finalize().to_hex().to_string()
}

/// The payload stored under `key` in `dir`; `None` if there is none or it is damaged. Counted
/// in [`counts`].
pub fn read(dir: &Path, key: &str) -> Option<Vec<u8>> {
    let found = std::fs::read(dir.join(key)).ok().and_then(|bytes| {
        let rest = bytes.strip_prefix(MAGIC.as_slice())?;
        let (sum, payload) = rest.split_at_checked(32)?;
        (blake3::hash(payload).as_bytes() == sum).then(|| payload.to_vec())
    });
    (if found.is_some() { &HITS } else { &MISSES }).fetch_add(1, Ordering::Relaxed);
    found
}

/// Lookups since the start that found their entry, and those that did not: the slow-conversion
/// log line shows only conversions of 20 ms or more, which a hit never is.
pub fn counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// Stores `payload` under `key` in `dir`, atomically. Failures are not fatal: the next run converts again.
pub fn write(dir: &Path, key: &str, payload: &[u8]) {
    let tmp = dir.join(format!("{key}.{}.tmp", std::process::id()));
    let bytes = [MAGIC.as_slice(), blake3::hash(payload).as_bytes(), payload].concat();
    if std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, dir.join(key))).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Whether compiling the pipeline `key` names crashed Metal's compiler service on an earlier run
/// on this OS build ([`remember_crash`]).
pub fn crashed(key: &[u8]) -> bool {
    dir().is_some_and(|d| d.join(crash_name(key)).exists())
}

/// Remembers that compiling the pipeline `key` names crashed Metal's compiler service
/// (`XPC_ERROR_CONNECTION_INTERRUPTED`), so later runs refuse it at once instead of waiting through
/// Metal's retries again (about 30 s a pipeline on macOS 15). An OS update forgets it.
pub fn remember_crash(key: &[u8]) {
    if let Some(d) = dir() {
        write(d, &crash_name(key), b"");
    }
}

fn crash_name(k: &[u8]) -> String {
    format!("crash-{}", &key(&[b"compiler crash", os_build().as_bytes(), k])[..32])
}

/// The OS build (`kern.osversion`), empty if it cannot be read.
#[cfg(not(target_os = "macos"))]
fn os_build() -> String {
    String::new()
}

/// The OS build (`kern.osversion`), empty if it cannot be read.
#[cfg(target_os = "macos")]
fn os_build() -> String {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: the name is NUL-terminated and `buf` holds `len` bytes.
    let ok = unsafe {
        libc::sysctlbyname(c"kern.osversion".as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0)
    } == 0;
    let s = if ok { &buf[..len.min(buf.len())] } else { &[][..] };
    String::from_utf8_lossy(s).trim_end_matches('\0').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_compiler_crash_is_remembered_for_its_pipeline_only() {
        let d = tempfile::tempdir().unwrap();
        // The folder is set once per process; this test is the only one that sets it.
        use_dir(d.path().to_path_buf());
        assert!(cfg!(not(target_os = "macos")) || !os_build().is_empty());
        assert!(!crashed(b"pipeline a"));
        remember_crash(b"pipeline a");
        assert!(crashed(b"pipeline a"));
        assert!(!crashed(b"pipeline b"));
    }

    #[test]
    fn payloads_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let k = key(&[b"dxil", b"layout"]);
        assert_eq!(read(d.path(), &k), None);
        write(d.path(), &k, b"metallib bytes");
        assert_eq!(read(d.path(), &k).as_deref(), Some(&b"metallib bytes"[..]));
        // No temporary file is left behind.
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn lookups_are_counted() {
        let d = tempfile::tempdir().unwrap();
        let k = key(&[b"counted"]);
        let (hits, misses) = counts();
        assert_eq!(read(d.path(), &k), None);
        write(d.path(), &k, b"payload");
        assert!(read(d.path(), &k).is_some());
        // Other tests read too, at the same time: at least these two.
        let (h, m) = counts();
        assert!(h > hits && m > misses);
    }

    #[test]
    fn another_version_or_input_misses() {
        // Every part is in the key, and where one ends and the next starts counts.
        assert_ne!(key(&[b"ab", b"c"]), key(&[b"a", b"bc"]));
        assert_ne!(key(&[b"a", b"converter 1"]), key(&[b"a", b"converter 2"]));
        // A file of another format under the right name is a miss.
        let d = tempfile::tempdir().unwrap();
        let k = key(&[b"x"]);
        write(d.path(), &k, b"payload");
        let mut bytes = std::fs::read(d.path().join(&k)).unwrap();
        bytes[3] = b'0';
        std::fs::write(d.path().join(&k), bytes).unwrap();
        assert_eq!(read(d.path(), &k), None);
    }

    #[test]
    fn damaged_files_miss() {
        let d = tempfile::tempdir().unwrap();
        let k = key(&[b"x"]);
        write(d.path(), &k, b"payload");
        let good = std::fs::read(d.path().join(&k)).unwrap();
        for bad in [&good[..good.len() - 1], &good[..10], &[][..], b"VML1"] {
            std::fs::write(d.path().join(&k), bad).unwrap();
            assert_eq!(read(d.path(), &k), None);
        }
        let mut flipped = good.clone();
        *flipped.last_mut().unwrap() ^= 1;
        std::fs::write(d.path().join(&k), flipped).unwrap();
        assert_eq!(read(d.path(), &k), None);
    }
}
