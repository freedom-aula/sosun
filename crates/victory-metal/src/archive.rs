//! Compiled pipeline states kept across runs in an `MTLBinaryArchive` (one file in the library
//! cache's folder, `disk::use_dir`). A pipeline is looked up in the archive first (failing on a
//! miss); a miss is compiled as before and added. The archive is written back at most every few
//! seconds while pipelines are being added, and when the backend is dropped, so a run that is
//! killed loses at most the last few seconds' pipelines. A file Metal refuses (another GPU or OS)
//! is replaced by an empty archive.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSArray, NSString, NSURL};
use objc2_metal::*;
use std::cell::Cell;
use std::time::{Duration, Instant};

const SAVE_EVERY: Duration = Duration::from_secs(5);

pub(crate) struct Archive {
    archive: Retained<ProtocolObject<dyn MTLBinaryArchive>>,
    list: Retained<NSArray<ProtocolObject<dyn MTLBinaryArchive>>>,
    path: std::path::PathBuf,
    added: Cell<u32>,
    saved: Cell<Instant>,
}

type Dev = Retained<ProtocolObject<dyn MTLDevice>>;

impl Archive {
    /// The archive for this GPU, if libraries are kept on disk.
    pub(crate) fn open(dev: &Dev) -> Option<Archive> {
        let dir = crate::disk::dir()?;
        let gpu = blake3::hash(dev.name().to_string().as_bytes()).to_hex();
        let path = dir.join(format!("pipelines-{}.metalarchive", &gpu[..8]));
        let make = |load: bool| {
            let d = MTLBinaryArchiveDescriptor::new();
            if load {
                d.setUrl(Some(&url(&path)));
            }
            dev.newBinaryArchiveWithDescriptor_error(&d)
        };
        let archive = match path.exists().then(|| make(true)) {
            Some(Ok(a)) => a,
            _ => {
                let _ = std::fs::remove_file(&path);
                make(false).ok()?
            }
        };
        let list = NSArray::from_retained_slice(&[archive.clone()]);
        Some(Archive { archive, list, path, added: Cell::new(0), saved: Cell::new(Instant::now()) })
    }

    /// The pipeline of `d`: from the archive, else compiled and added.
    pub(crate) fn render(
        &self,
        dev: &Dev,
        d: &MTLRenderPipelineDescriptor,
        crash_key: &str,
    ) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
        d.setBinaryArchives(Some(&self.list));
        if let Ok(p) = dev.newRenderPipelineStateWithDescriptor_options_reflection_error(
            d,
            MTLPipelineOption::FailOnBinaryArchiveMiss,
            None,
        ) {
            return Ok(p);
        }
        let p = compile_render(dev, d, crash_key)?;
        if self.archive.addRenderPipelineFunctionsWithDescriptor_error(d).is_ok() {
            self.added_one();
        }
        Ok(p)
    }

    /// As [`Self::render`], for a compute function.
    pub(crate) fn compute(
        &self,
        dev: &Dev,
        f: &ProtocolObject<dyn MTLFunction>,
    ) -> Result<Retained<ProtocolObject<dyn MTLComputePipelineState>>, String> {
        let d = MTLComputePipelineDescriptor::new();
        d.setComputeFunction(Some(f));
        d.setBinaryArchives(Some(&self.list));
        if let Ok(p) = dev.newComputePipelineStateWithDescriptor_options_reflection_error(
            &d,
            MTLPipelineOption::FailOnBinaryArchiveMiss,
            None,
        ) {
            return Ok(p);
        }
        let p = dev
            .newComputePipelineStateWithDescriptor_options_reflection_error(&d, MTLPipelineOption::None, None)
            .map_err(|e| e.localizedDescription().to_string())?;
        if self.archive.addComputePipelineFunctionsWithDescriptor_error(&d).is_ok() {
            self.added_one();
        }
        Ok(p)
    }

    fn added_one(&self) {
        self.added.set(self.added.get() + 1);
        if self.saved.get().elapsed() >= SAVE_EVERY {
            self.save();
        }
    }

    /// Writes the archive back (to a temporary name, then renamed) if pipelines were added.
    fn save(&self) {
        if self.added.replace(0) == 0 {
            return;
        }
        self.saved.set(Instant::now());
        let tmp = self.path.with_extension(format!("{}.tmp", std::process::id()));
        if self.archive.serializeToURL_error(&url(&tmp)).is_err() || std::fs::rename(&tmp, &self.path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

impl Drop for Archive {
    fn drop(&mut self) {
        self.save();
    }
}

/// How long a render pipeline may compile before it is refused for this run. Metal's compiler
/// service crashes on some pipelines, and Metal retries before it reports the crash: about 30 s a
/// pipeline on macOS 15 (0.7 s on macOS 27), on the frame. A normal compile takes well under this.
/// shortcut: a pipeline that compiles slower than this is refused for the run, and the compile it
/// finishes is dropped (not archived); keep it in the archive if a real one ever needs longer.
const COMPILE_DEADLINE: Duration = Duration::from_secs(10);

type RenderPso = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;

/// Metal objects handed to the compile thread: Metal's devices, descriptors and pipeline states
/// may be used from any thread.
struct Sendable<T>(T);
// SAFETY: as above; only the compile thread uses them once moved.
unsafe impl<T> Send for Sendable<T> {}

/// Compiles the render pipeline `d` on a thread of its own, waiting at most `COMPILE_DEADLINE`.
/// A compile that ends in a compiler crash, in time or after, is remembered under `crash_key`
/// ([`crate::disk::remember_crash`]), so later runs refuse it at once.
pub(crate) fn compile_render(dev: &Dev, d: &MTLRenderPipelineDescriptor, crash_key: &str) -> Result<RenderPso, String> {
    use objc2_foundation::NSCopying;
    let job = Sendable((dev.clone(), d.copy()));
    let compile = move || {
        let job = job;
        let (dev, d) = &job.0;
        Sendable(dev.newRenderPipelineStateWithDescriptor_error(d).map_err(|e| e.localizedDescription().to_string()))
    };
    let late_key = crash_key.to_owned();
    let made = with_deadline(COMPILE_DEADLINE, compile, move |r| remember_crash(&r.0, &late_key));
    let Some(Sendable(r)) = made else {
        return Err(format!(
            "Metal's compiler took over {} s (refused for this run; a crash is remembered when it ends)",
            COMPILE_DEADLINE.as_secs()
        ));
    };
    remember_crash(&r, crash_key);
    r
}

fn remember_crash<T>(r: &Result<T, String>, key: &str) {
    if let Err(e) = r
        && e.contains("XPC_ERROR_CONNECTION_INTERRUPTED")
    {
        crate::disk::remember_crash(key.as_bytes());
    }
}

/// `f`'s result if it comes within `deadline`; else `None`, and `late` gets it when it comes.
fn with_deadline<T: Send + 'static>(
    deadline: Duration,
    f: impl FnOnce() -> T + Send + 'static,
    late: impl FnOnce(T) + Send + 'static,
) -> Option<T> {
    use std::sync::{Arc, Mutex, mpsc};
    let gave_up = Arc::new(Mutex::new(false));
    let (tx, rx) = mpsc::channel();
    let flag = gave_up.clone();
    std::thread::spawn(move || {
        let r = f();
        let gave_up = flag.lock().unwrap_or_else(|e| e.into_inner());
        if *gave_up {
            late(r);
        } else {
            let _ = tx.send(r);
        }
    });
    match rx.recv_timeout(deadline) {
        Ok(r) => Some(r),
        Err(_) => {
            *gave_up.lock().unwrap_or_else(|e| e.into_inner()) = true;
            // The result may have been sent between the timeout and the flag.
            rx.try_recv().ok()
        }
    }
}

fn url(p: &std::path::Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn a_result_past_its_deadline_goes_to_the_late_handler() {
        let (tx, rx) = mpsc::channel();
        let quick = with_deadline(Duration::from_secs(5), || 1, |_| panic!("not late"));
        assert_eq!(quick, Some(1));
        let slow = with_deadline(
            Duration::from_millis(10),
            || {
                std::thread::sleep(Duration::from_millis(200));
                2
            },
            move |r| tx.send(r).unwrap(),
        );
        assert_eq!(slow, None);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(2));
    }
}
