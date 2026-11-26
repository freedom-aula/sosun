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
    ) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
        d.setBinaryArchives(Some(&self.list));
        if let Ok(p) = dev.newRenderPipelineStateWithDescriptor_options_reflection_error(
            d,
            MTLPipelineOption::FailOnBinaryArchiveMiss,
            None,
        ) {
            return Ok(p);
        }
        let p = dev
            .newRenderPipelineStateWithDescriptor_error(d)
            .map_err(|e| e.localizedDescription().to_string())?;
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

fn url(p: &std::path::Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy()))
}
