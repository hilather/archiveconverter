//! Format detection from extension and magic bytes.

use super::ArchiveFormat;
use crate::util::pathnorm::{member_basename, normalize_member_path};
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Guess format from a member path (extension only).
pub fn format_from_path(path: &str) -> ArchiveFormat {
    let base = member_basename(path).to_ascii_lowercase();
    if base.ends_with(".7z") {
        ArchiveFormat::SevenZ
    } else if base.ends_with(".zip") {
        ArchiveFormat::Zip
    } else {
        ArchiveFormat::Unknown
    }
}

/// Detect format by reading magic bytes; falls back to extension.
pub fn detect_file(path: &Path) -> ArchiveFormat {
    if let Ok(mut f) = File::open(path) {
        let mut magic = [0u8; 6];
        if f.read_exact(&mut magic).is_ok() {
            // 7z signature: 37 7A BC AF 27 1C
            if magic == [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C] {
                return ArchiveFormat::SevenZ;
            }
            // ZIP local file header / empty archive
            if &magic[0..2] == b"PK" {
                return ArchiveFormat::Zip;
            }
        }
    }
    path.file_name()
        .and_then(|s| s.to_str())
        .map(format_from_path)
        .unwrap_or(ArchiveFormat::Unknown)
}

pub fn is_7z_member(path: &str) -> bool {
    matches!(format_from_path(&normalize_member_path(path)), ArchiveFormat::SevenZ)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_detection() {
        assert_eq!(format_from_path("a/b/c.7z"), ArchiveFormat::SevenZ);
        assert_eq!(format_from_path("C.7Z"), ArchiveFormat::SevenZ);
        assert_eq!(format_from_path("x.zip"), ArchiveFormat::Zip);
        assert_eq!(format_from_path("x.txt"), ArchiveFormat::Unknown);
    }
}
