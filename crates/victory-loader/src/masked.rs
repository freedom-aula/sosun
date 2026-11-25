//! Finds the AVX masked stores in a title's code (`vpmaskmovd`,
//! `vpmaskmovq`, `vmaskmovps` and `vmaskmovpd` to memory).
//!
//! Rosetta cannot resume one of these that faults partway (#14), so
//! fault-based write tracking is off under Rosetta and every submission
//! copies title memory whole: about 129 GB per frame once the world
//! streams in (#50). Knowing where the title uses these stores is the
//! first step to patching them so tracking can come back on. The scan
//! reads bytes, not decoded instructions, so a match inside another
//! instruction's bytes is possible, though rare.

use victory_pe::ImageInfo;

/// `IMAGE_SCN_MEM_EXECUTE`.
const EXECUTE: u32 = 0x2000_0000;

/// One masked store found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaskedStore {
    pub rva: u32,
    pub kind: MaskedKind,
    /// 256-bit (`ymm`) rather than 128-bit.
    pub wide: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskedKind {
    Vpmaskmovd,
    Vpmaskmovq,
    Vmaskmovps,
    Vmaskmovpd,
}

/// The masked store whose encoding starts at `at`, if one does: a
/// three-byte VEX prefix (`c4`) in the 0F38 map with the 66 prefix, then
/// opcode 8e (`vpmaskmov`, W picks d or q), 2e (`vmaskmovps`) or 2f
/// (`vmaskmovpd`), and a memory operand.
fn decode(code: &[u8], at: usize) -> Option<(MaskedKind, bool)> {
    let &[c4, b1, b2, opcode, modrm] = code.get(at..at + 5)? else {
        return None;
    };
    if c4 != 0xc4 || b1 & 0x1f != 2 || b2 & 3 != 1 || modrm >> 6 == 3 {
        return None;
    }
    let w = b2 & 0x80 != 0;
    let kind = match opcode {
        0x8e if w => MaskedKind::Vpmaskmovq,
        0x8e => MaskedKind::Vpmaskmovd,
        0x2e => MaskedKind::Vmaskmovps,
        0x2f => MaskedKind::Vmaskmovpd,
        _ => return None,
    };
    Some((kind, b2 & 4 != 0))
}

/// Every masked store in the executable sections of `bytes` (the image
/// file `info` was parsed from), by RVA.
pub fn masked_stores(info: &ImageInfo, bytes: &[u8]) -> Vec<MaskedStore> {
    let mut found = Vec::new();
    for s in info
        .sections
        .iter()
        .filter(|s| s.characteristics & EXECUTE != 0)
    {
        let start = (s.file_offset as usize).min(bytes.len());
        let end = start.saturating_add(s.file_size as usize).min(bytes.len());
        let code = &bytes[start..end];
        for at in 0..code.len() {
            if let Some((kind, wide)) = decode(code, at) {
                found.push(MaskedStore {
                    rva: s.rva + at as u32,
                    kind,
                    wide,
                });
            }
        }
    }
    found
}

/// One line for the start-up report: how many of each kind, and the first
/// few RVAs.
pub fn summary(found: &[MaskedStore]) -> String {
    if found.is_empty() {
        return "none in code".to_owned();
    }
    let count = |k| found.iter().filter(|f| f.kind == k).count();
    let first: Vec<String> = found
        .iter()
        .take(16)
        .map(|f| format!("{:#x}", f.rva))
        .collect();
    format!(
        "{} in code (vpmaskmovd {}, vpmaskmovq {}, vmaskmovps {}, vmaskmovpd {}), first at {}{}",
        found.len(),
        count(MaskedKind::Vpmaskmovd),
        count(MaskedKind::Vpmaskmovq),
        count(MaskedKind::Vmaskmovps),
        count(MaskedKind::Vmaskmovpd),
        first.join(" "),
        if found.len() > 16 { " ..." } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_store_seen_in_the_rosetta_hang() {
        // `vpmaskmovd [rax], ymm6, ymm4` from #1 and #50: c4 e2 45 8e 30.
        assert_eq!(
            decode(&[0xc4, 0xe2, 0x45, 0x8e, 0x30], 0),
            Some((MaskedKind::Vpmaskmovd, true))
        );
        // W set: vpmaskmovq; 128-bit.
        assert_eq!(
            decode(&[0xc4, 0xe2, 0xc1, 0x8e, 0x30], 0),
            Some((MaskedKind::Vpmaskmovq, false))
        );
        assert_eq!(
            decode(&[0xc4, 0xe2, 0x45, 0x2e, 0x08], 0),
            Some((MaskedKind::Vmaskmovps, true))
        );
        assert_eq!(
            decode(&[0xc4, 0xe2, 0x45, 0x2f, 0x08], 0),
            Some((MaskedKind::Vmaskmovpd, true))
        );
    }

    #[test]
    fn leaves_loads_registers_and_other_maps_alone() {
        // The load forms (8c, 2c) are not stores.
        assert_eq!(decode(&[0xc4, 0xe2, 0x45, 0x8c, 0x30], 0), None);
        // A register operand (mod 3) is not a store to memory.
        assert_eq!(decode(&[0xc4, 0xe2, 0x45, 0x8e, 0xf0], 0), None);
        // The 0F map, or no 66 prefix.
        assert_eq!(decode(&[0xc4, 0xe1, 0x45, 0x8e, 0x30], 0), None);
        assert_eq!(decode(&[0xc4, 0xe2, 0x44, 0x8e, 0x30], 0), None);
        // Too short.
        assert_eq!(decode(&[0xc4, 0xe2, 0x45, 0x8e], 0), None);
    }

    #[test]
    fn the_summary_counts_and_lists() {
        let f = |rva, kind| MaskedStore {
            rva,
            kind,
            wide: true,
        };
        let found = [
            f(0x1000, MaskedKind::Vpmaskmovd),
            f(0x2000, MaskedKind::Vmaskmovps),
        ];
        assert_eq!(
            summary(&found),
            "2 in code (vpmaskmovd 1, vpmaskmovq 0, vmaskmovps 1, vmaskmovpd 0), first at 0x1000 0x2000"
        );
        assert_eq!(summary(&[]), "none in code");
    }
}
