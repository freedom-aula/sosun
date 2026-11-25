//! Mapping a PE image into a macOS process: no Windows loader, so we do
//! what it does. The image is laid out at its preferred base if free (else
//! anywhere), relocated, and given its final page protections by
//! [`NativeImage::finalize`] once imports are bound.
//!
//! Nothing here runs title code and nothing is x86-64 specific, so an arm64
//! host (the recompiler's) maps images the same way.

use anyhow::{Context, Result, bail};
use victory_pe::layout::{self, dir};
use victory_pe::{ImageInfo, ImportSymbol};
use victory_platform::mem::{self, Protect};

const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const SCN_MEM_READ: u32 = 0x4000_0000;
const SCN_MEM_WRITE: u32 = 0x8000_0000;
const REL_ABSOLUTE: u16 = 0;
const REL_DIR64: u16 = 10;
const REL_HIGHLOW: u16 = 3;

/// The image's static TLS, as its TLS directory describes it (addresses
/// relocated, so they point into the mapped image).
#[derive(Debug, Clone, Copy)]
pub struct TlsTemplate {
    /// Start of the initial data every thread's block begins with.
    pub start: usize,
    pub end: usize,
    pub zero_fill: usize,
    /// Where the image keeps its TLS index (`_tls_index`).
    pub index_at: usize,
}

impl TlsTemplate {
    /// Bytes of one thread's block.
    pub fn block_len(&self) -> usize {
        self.end - self.start + self.zero_fill
    }
}

/// A title image mapped into this process.
#[derive(Debug)]
pub struct NativeImage {
    base: usize,
    len: usize,
    info: ImageInfo,
}

// SAFETY: the base is just an address; the image has no interior state.
unsafe impl Send for NativeImage {}
// SAFETY: as above.
unsafe impl Sync for NativeImage {}

impl NativeImage {
    /// Maps `bytes`, a PE32+ file. `base` forces the load address (to test
    /// relocation, or to place a DLL); by default the preferred base, then
    /// any free range if that is taken.
    pub fn map(bytes: &[u8], base: Option<usize>) -> Result<NativeImage> {
        let info = ImageInfo::parse(bytes)?;
        let len = (info.size_of_image as usize).next_multiple_of(mem::page_size());
        let preferred = info.image_base as usize;
        let at = base.unwrap_or(preferred);
        // SAFETY: a fresh allocation; it fails rather than replacing anything.
        let region = unsafe { mem::alloc(Some(at), len, Protect::READ_WRITE) }
            .or_else(|first| {
                if base.is_some() || !info.has_relocations {
                    return Err(first);
                }
                // SAFETY: as above, anywhere this time.
                unsafe { mem::alloc(None, len, Protect::READ_WRITE) }
            })
            .with_context(|| format!("mapping {len:#x} bytes of image at {at:#x}"))?;
        let image = NativeImage {
            base: region as usize,
            len,
            info,
        };
        image.copy_sections(bytes)?;
        image.relocate(bytes)?;
        tracing::info!(
            base = format_args!("{:#x}", image.base),
            preferred = format_args!("{preferred:#x}"),
            "native image mapped"
        );
        Ok(image)
    }

    pub fn base(&self) -> usize {
        self.base
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Parsed from the original image (imports, entry point, TLS callbacks).
    pub fn info(&self) -> &ImageInfo {
        &self.info
    }

    pub fn entry(&self) -> Option<usize> {
        (self.info.entry_rva != 0).then(|| self.base + self.info.entry_rva as usize)
    }

    /// Address of a named export.
    pub fn export(&self, name: &str) -> Option<usize> {
        let rva = self
            .info
            .exports
            .iter()
            .find(|e| e.name.as_deref() == Some(name))?
            .rva?;
        Some(self.base + rva as usize)
    }

    /// Address of an export, by name or ordinal.
    pub fn export_symbol(&self, symbol: &ImportSymbol) -> Option<usize> {
        match symbol {
            ImportSymbol::Name { name, .. } => self.export(name),
            ImportSymbol::Ordinal(o) => {
                let rva = self.info.exports.iter().find(|e| e.ordinal == *o)?.rva?;
                Some(self.base + rva as usize)
            }
        }
    }

    fn copy_sections(&self, bytes: &[u8]) -> Result<()> {
        let opt = layout::optional_header_offset(bytes).context("no optional header")?;
        // SizeOfHeaders is at offset 60 of the optional header.
        let headers = layout::read_u32(bytes, opt + 60).context("no SizeOfHeaders")? as usize;
        let copy = |rva: usize, from: usize, len: usize| -> Result<()> {
            let src = bytes
                .get(from..from.checked_add(len).context("section overflows")?)
                .with_context(|| format!("section data {from:#x}+{len:#x} past the file"))?;
            if rva.checked_add(len).is_none_or(|end| end > self.len) {
                bail!("section at {rva:#x}+{len:#x} past the image");
            }
            // SAFETY: inside our own writable mapping, checked above.
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), (self.base + rva) as *mut u8, len)
            };
            Ok(())
        };
        copy(0, 0, headers.min(bytes.len()))?;
        for s in &self.info.sections {
            let len = if s.virtual_size == 0 {
                s.file_size
            } else {
                s.file_size.min(s.virtual_size)
            };
            copy(s.rva as usize, s.file_offset as usize, len as usize)?;
        }
        Ok(())
    }

    /// Applies base relocations for the distance between the load address
    /// and the preferred base.
    fn relocate(&self, bytes: &[u8]) -> Result<()> {
        let delta = self.base.wrapping_sub(self.info.image_base as usize);
        if delta == 0 {
            return Ok(());
        }
        let at = layout::data_directory_offset(bytes, dir::BASERELOC).context("no directory")?;
        let rva = layout::read_u32(bytes, at).unwrap_or(0) as usize;
        let size = layout::read_u32(bytes, at + 4).unwrap_or(0) as usize;
        if rva == 0 || size == 0 {
            bail!("image is not at its preferred base and has no relocations");
        }
        let mut block = rva;
        while block + 8 <= rva + size {
            let page = self.read::<u32>(block) as usize;
            let block_size = self.read::<u32>(block + 4) as usize;
            if block_size < 8 {
                break;
            }
            for i in 0..(block_size - 8) / 2 {
                let entry = self.read::<u16>(block + 8 + i * 2);
                let target = page + usize::from(entry & 0xFFF);
                match entry >> 12 {
                    REL_ABSOLUTE => {}
                    REL_DIR64 => {
                        let v = self.read::<u64>(target).wrapping_add(delta as u64);
                        self.write(target, v);
                    }
                    REL_HIGHLOW => {
                        let v = self.read::<u32>(target).wrapping_add(delta as u32);
                        self.write(target, v);
                    }
                    other => bail!("unsupported relocation type {other}"),
                }
            }
            block += block_size;
        }
        Ok(())
    }

    fn read<T: Copy>(&self, rva: usize) -> T {
        assert!(
            rva + size_of::<T>() <= self.len,
            "rva {rva:#x} outside the image"
        );
        // SAFETY: inside the mapping (asserted); PE data is unaligned.
        unsafe { ((self.base + rva) as *const T).read_unaligned() }
    }

    fn write<T>(&self, rva: usize, value: T) {
        assert!(
            rva + size_of::<T>() <= self.len,
            "rva {rva:#x} outside the image"
        );
        // SAFETY: inside our writable mapping (asserted).
        unsafe { ((self.base + rva) as *mut T).write_unaligned(value) };
    }

    /// The image's static TLS, if it has any.
    pub fn tls(&self) -> Option<TlsTemplate> {
        let dir = self.info.tls.as_ref()?.directory_rva as usize;
        Some(TlsTemplate {
            start: self.read::<u64>(dir) as usize,
            end: self.read::<u64>(dir + 8) as usize,
            index_at: self.read::<u64>(dir + 16) as usize,
            zero_fill: self.read::<u32>(dir + 32) as usize,
        })
    }

    /// Fills IAT slots: `(slot rva, address)` pairs.
    pub fn write_slots(&self, slots: &[(u32, usize)]) -> Result<()> {
        for &(rva, address) in slots {
            if rva as usize + 8 > self.len {
                bail!("IAT slot {rva:#x} outside the image");
            }
            self.write(rva as usize, address as u64);
        }
        Ok(())
    }

    /// Sets each page's protection from the sections on it (the union where
    /// two share a page, as the host's page may be larger than the PE's).
    /// Call it once imports are bound; the headers become read-only.
    pub fn finalize(&self) -> Result<()> {
        let page = mem::page_size();
        let mut pages = vec![0u32; self.len / page];
        pages[0] |= SCN_MEM_READ; // headers
        for s in &self.info.sections {
            let start = s.rva as usize / page;
            let end = (s.rva as usize + s.virtual_size.max(s.file_size) as usize).div_ceil(page);
            let flags = s.characteristics & (SCN_MEM_EXECUTE | SCN_MEM_READ | SCN_MEM_WRITE);
            for p in pages.iter_mut().take(end.min(self.len / page)).skip(start) {
                *p |= flags;
            }
        }
        let protect_of = |f: u32| match (f & SCN_MEM_WRITE != 0, f & SCN_MEM_EXECUTE != 0) {
            (true, true) => Protect::READ_WRITE_EXECUTE,
            (true, false) => Protect::READ_WRITE,
            (false, true) => Protect::READ_EXECUTE,
            (false, false) => Protect::READ,
        };
        let mut i = 0;
        while i < pages.len() {
            let mut j = i + 1;
            while j < pages.len() && protect_of(pages[j]) == protect_of(pages[i]) {
                j += 1;
            }
            // SAFETY: a run of pages of the mapping this image owns.
            unsafe {
                mem::protect(
                    (self.base + i * page) as *mut u8,
                    (j - i) * page,
                    protect_of(pages[i]),
                )
                .with_context(|| format!("protecting image pages {i}..{j}"))?;
                mem::flush_icache((self.base + i * page) as *mut u8, (j - i) * page);
            }
            i = j;
        }
        Ok(())
    }

    /// Unmaps the image.
    ///
    /// # Safety
    /// No title code may run or hold pointers into it afterwards.
    pub unsafe fn unmap(self) {
        // SAFETY: forwarded from the caller.
        unsafe { mem::release(self.base as *mut u8, self.len) }.ok();
    }
}
