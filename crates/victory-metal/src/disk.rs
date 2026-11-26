//! Metal libraries kept on disk: the converter's output (a metallib and its shader stage) for a
//! DXIL container, its root layout and the converter itself, so a later run skips the conversion.
//! One file per key, written to a temporary name and renamed (a reader sees all of it or none);
//! a file that is damaged, or made by another format, reads as a miss. The folder is the user's
//! cache folder (`victory_gpu::shader_cache::use_native_cache`), never the repo.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Names the file format: part of every key, and the start of every file.
const MAGIC: &[u8; 4] = b"VML1";

static DIR: OnceLock<PathBuf> = OnceLock::new();

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

/// The payload stored under `key` in `dir`; `None` if there is none or it is damaged.
pub fn read(dir: &Path, key: &str) -> Option<Vec<u8>> {
    let bytes = std::fs::read(dir.join(key)).ok()?;
    let rest = bytes.strip_prefix(MAGIC.as_slice())?;
    let (sum, payload) = rest.split_at_checked(32)?;
    (blake3::hash(payload).as_bytes() == sum).then(|| payload.to_vec())
}

/// Stores `payload` under `key` in `dir`, atomically. Failures are not fatal: the next run converts again.
pub fn write(dir: &Path, key: &str, payload: &[u8]) {
    let tmp = dir.join(format!("{key}.{}.tmp", std::process::id()));
    let bytes = [MAGIC.as_slice(), blake3::hash(payload).as_bytes(), payload].concat();
    if std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, dir.join(key))).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
