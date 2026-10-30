// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded format hints used to select a trusted filesystem worker executable.
//! `NativeBlock` already identifies a logical volume; this does not scan partitions.

/// Whole-sector prefix covering the FAT boot sector and EROFS/F2FS superblock magic.
pub const PROBE_BYTES: usize = 3 * 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    Fat,
    Erofs,
    F2fs,
}

impl Format {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Fat => "FAT",
            Self::Erofs => "EROFS",
            Self::F2fs => "F2FS",
        }
    }

    pub const fn worker_name(self) -> &'static str {
        match self {
            Self::Fat => "fs-fat",
            Self::Erofs => "fs-erofs",
            Self::F2fs => "fs-f2fs",
        }
    }
}

/// Candidate recognition only. The selected worker must validate full geometry
/// and allocation metadata before publishing a mount. Recognition does not
/// imply that the corresponding worker is installed. Ambiguous hints are rejected.
pub fn identify(prefix: &[u8]) -> Option<Format> {
    // EROFS reserves the first KiB and stores its little-endian magic at the
    // start of the superblock. No EROFS implementation is linked into this probe.
    let erofs = prefix.get(1024..1028) == Some(&0xe0f5_e1e2u32.to_le_bytes());
    let f2fs = prefix.get(1024..1028) == Some(&0xf2f5_2010u32.to_le_bytes());
    match (fat_geometry(prefix), erofs, f2fs) {
        (true, false, false) => Some(Format::Fat),
        (false, true, false) => Some(Format::Erofs),
        (false, false, true) => Some(Format::F2fs),
        _ => None,
    }
}

fn fat_geometry(prefix: &[u8]) -> bool {
    let Some(boot) = prefix.get(..512) else {
        return false;
    };
    if boot[510..512] != [0x55, 0xaa] {
        return false;
    }
    let word = |offset| u16::from_le_bytes([boot[offset], boot[offset + 1]]);
    let dword = |offset| {
        u32::from_le_bytes([
            boot[offset],
            boot[offset + 1],
            boot[offset + 2],
            boot[offset + 3],
        ])
    };
    // The informational FAT type string is optional and can be stale. Probe
    // plausible BPB geometry, leaving FAT type and full validation to the driver.
    let sector_bytes = word(11);
    let clusters = boot[13];
    let reserved = word(14);
    let fats = boot[16];
    let total = if word(19) != 0 {
        u32::from(word(19))
    } else {
        dword(32)
    };
    let fat_sectors = if word(22) != 0 {
        u32::from(word(22))
    } else {
        dword(36)
    };
    matches!(sector_bytes, 512 | 1024 | 2048 | 4096)
        && clusters.is_power_of_two()
        && clusters <= 128
        && reserved != 0
        && (1..=2).contains(&fats)
        && fat_sectors != 0
        && u64::from(total) > u64::from(reserved) + u64::from(fats) * u64::from(fat_sectors)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn format_detection_uses_geometry_not_informational_label() {
        let mut boot = [0; 512];
        boot[11..13].copy_from_slice(&512u16.to_le_bytes());
        boot[13] = 1;
        boot[14..16].copy_from_slice(&32u16.to_le_bytes());
        boot[16] = 2;
        boot[32..36].copy_from_slice(&70000u32.to_le_bytes());
        boot[36..40].copy_from_slice(&600u32.to_le_bytes());
        boot[510..512].copy_from_slice(&[0x55, 0xaa]);
        assert_eq!(identify(&boot), Some(Format::Fat));
        boot[82..90].copy_from_slice(b"UNKNOWN ");
        assert_eq!(identify(&boot), Some(Format::Fat));
        boot[13] = 3;
        assert_eq!(identify(&boot), None);
        let mut partition = [0; 512];
        partition[510..512].copy_from_slice(&[0x55, 0xaa]);
        assert_eq!(identify(&partition), None);
    }

    #[test]
    fn erofs_recognition_is_independent_of_worker_availability() {
        let mut prefix = [0; PROBE_BYTES];
        prefix[..4].copy_from_slice(&0xe0f5_e1e2u32.to_le_bytes());
        assert_eq!(identify(&prefix), None);
        prefix[1024..1028].copy_from_slice(&0xe0f5_e1e2u32.to_be_bytes());
        assert_eq!(identify(&prefix), None);
        prefix[1024..1028].copy_from_slice(&0xe0f5_e1e2u32.to_le_bytes());
        assert_eq!(identify(&prefix), Some(Format::Erofs));
        assert_eq!(identify(&prefix[..1027]), None);
        assert_eq!(Format::Erofs.worker_name(), "fs-erofs");
    }

    #[test]
    fn f2fs_recognition_requires_complete_little_endian_superblock_magic() {
        let mut prefix = [0; PROBE_BYTES];
        prefix[1024..1028].copy_from_slice(&0xf2f5_2010u32.to_be_bytes());
        assert_eq!(identify(&prefix), None);
        prefix[1024..1028].copy_from_slice(&0xf2f5_2010u32.to_le_bytes());
        assert_eq!(identify(&prefix), Some(Format::F2fs));
        assert_eq!(identify(&prefix[..1027]), None);
        assert_eq!(Format::F2fs.worker_name(), "fs-f2fs");
    }

    #[test]
    fn ambiguous_format_hints_do_not_select_a_worker() {
        let mut prefix = [0; PROBE_BYTES];
        prefix[11..13].copy_from_slice(&512u16.to_le_bytes());
        prefix[13] = 1;
        prefix[14..16].copy_from_slice(&32u16.to_le_bytes());
        prefix[16] = 2;
        prefix[32..36].copy_from_slice(&70000u32.to_le_bytes());
        prefix[36..40].copy_from_slice(&600u32.to_le_bytes());
        prefix[510..512].copy_from_slice(&[0x55, 0xaa]);
        assert_eq!(identify(&prefix), Some(Format::Fat));
        for magic in [0xe0f5_e1e2u32, 0xf2f5_2010u32] {
            prefix[1024..1028].copy_from_slice(&magic.to_le_bytes());
            assert_eq!(identify(&prefix), None);
        }
    }
}
