//! Shared 7z **raw (unencoded) header** writer matching sevenz-rust2 / 7-Zip layout.
//!
//! Layout for non-solid archives (one folder + one unpack stream per file):
//!
//! ```text
//! kHeader
//!   kMainStreamsInfo
//!     kPackInfo (pos, num, kSize…, kCRC all-defined…, kEnd)
//!     kUnpackInfo (kFolder…, kCodersUnpackSize…, kEnd)  // no folder CRCs
//!     kSubStreamsInfo (kCRC all-defined content CRCs…, kEnd)
//!     kEnd
//!   kFilesInfo (num, [kEmptyStream], [kEmptyFile], kName, [times], [kWinAttributes], kEnd)
//! kEnd
//! ```
//!
//! Coder properties size is written as a single byte when `props.len() < 128`,
//! matching sevenz-rust2 (`write_u8(props.len())`).
//!
//! File times and Windows attributes are written **only when defined** on each
//! member (same rules as sevenz-rust2). Callers must supply source metadata;
//! this writer does not invent a shared conversion-time timestamp.

use crate::error::Result;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const SIG: &[u8] = b"7z\xBC\xAF\x27\x1C";
pub const SIG_HEADER_SIZE: u64 = 32;

pub const K_END: u8 = 0x00;
pub const K_HEADER: u8 = 0x01;
pub const K_MAIN_STREAMS_INFO: u8 = 0x04;
pub const K_FILES_INFO: u8 = 0x05;
pub const K_PACK_INFO: u8 = 0x06;
pub const K_UNPACK_INFO: u8 = 0x07;
pub const K_SUB_STREAMS_INFO: u8 = 0x08;
pub const K_SIZE: u8 = 0x09;
pub const K_CRC: u8 = 0x0A;
pub const K_FOLDER: u8 = 0x0B;
pub const K_CODERS_UNPACK_SIZE: u8 = 0x0C;
pub const K_EMPTY_STREAM: u8 = 0x0E;
pub const K_EMPTY_FILE: u8 = 0x0F;
pub const K_NAME: u8 = 0x11;
pub const K_C_TIME: u8 = 0x12;
pub const K_A_TIME: u8 = 0x13;
pub const K_M_TIME: u8 = 0x14;
pub const K_WIN_ATTRIBUTES: u8 = 0x15;

/// Windows FILE_ATTRIBUTE_ARCHIVE | (unix regular file mode in high word optional).
/// 0x20 = ARCHIVE; high word 0o100644 << 16 for tools that read Unix bits.
/// Used only as a last-resort default when packing synthetic test data with no source attrs.
pub const ATTR_FILE: u32 = 0x20 | ((0o100644u32) << 16);

/// Per-member file information for 7z FilesInfo (times + Windows attributes).
///
/// Times are Windows FILETIME (100ns ticks since 1601-01-01 UTC).
/// `None` means the property is not defined for that member (omit from the bitset).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileMeta {
    pub mtime: Option<u64>,
    pub ctime: Option<u64>,
    pub atime: Option<u64>,
    pub windows_attributes: Option<u32>,
}

impl FileMeta {
    /// Build metadata from filesystem timestamps/mode of `path`.
    pub fn from_fs_path(path: &Path) -> Self {
        let mut m = Self::default();
        if let Ok(meta) = std::fs::metadata(path) {
            if let Ok(t) = meta.modified() {
                m.mtime = system_time_to_filetime(t);
            }
            if let Ok(t) = meta.created() {
                m.ctime = system_time_to_filetime(t);
            }
            if let Ok(t) = meta.accessed() {
                m.atime = system_time_to_filetime(t);
            }
            m.windows_attributes = Some(attrs_from_fs_meta(&meta));
        }
        m
    }

    /// True when at least one field is set.
    pub fn is_empty(&self) -> bool {
        self.mtime.is_none()
            && self.ctime.is_none()
            && self.atime.is_none()
            && self.windows_attributes.is_none()
    }

    /// Prefer `self` fields; fill gaps from `other`.
    pub fn merge_missing(&mut self, other: &FileMeta) {
        if self.mtime.is_none() {
            self.mtime = other.mtime;
        }
        if self.ctime.is_none() {
            self.ctime = other.ctime;
        }
        if self.atime.is_none() {
            self.atime = other.atime;
        }
        if self.windows_attributes.is_none() {
            self.windows_attributes = other.windows_attributes;
        }
    }
}

/// Convert `SystemTime` to Windows FILETIME, or `None` if before Unix epoch / unrepresentable.
pub fn system_time_to_filetime(t: SystemTime) -> Option<u64> {
    let d = t.duration_since(UNIX_EPOCH).ok()?;
    let secs = d.as_secs();
    let nanos = d.subsec_nanos() as u64;
    // 11644473600 seconds between 1601-01-01 and 1970-01-01
    let ft_secs = secs.checked_add(11_644_473_600)?;
    let ft = ft_secs
        .checked_mul(10_000_000)?
        .checked_add(nanos / 100)?;
    Some(ft)
}

/// Current time as Windows FILETIME (100ns since 1601-01-01).
/// Prefer per-file source metadata; only for synthetic fixtures without a source.
#[allow(dead_code)]
pub fn filetime_now() -> u64 {
    system_time_to_filetime(SystemTime::now()).unwrap_or(0)
}

fn attrs_from_fs_meta(meta: &std::fs::Metadata) -> u32 {
    // ARCHIVE bit always; high word carries a rough Unix mode when available.
    let mut attr: u32 = 0x20; // FILE_ATTRIBUTE_ARCHIVE
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        // 7-Zip convention: Unix mode in high 16 bits of Windows attributes.
        attr |= (mode & 0xFFFF) << 16;
    }
    if meta.is_dir() {
        attr |= 0x10; // FILE_ATTRIBUTE_DIRECTORY
    }
    attr
}

/// One non-directory file member in a non-solid multi-file 7z.
#[derive(Debug, Clone)]
pub struct HeaderFile {
    pub name: String,
    /// Packed stream size (compressed or stored).
    pub pack_size: u64,
    /// CRC32 of the **pack** stream bytes.
    pub pack_crc: u32,
    /// Uncompressed size.
    pub unpack_size: u64,
    /// CRC32 of **uncompressed** content.
    pub content_crc: u32,
    /// Coder method id bytes (e.g. `[0x21]` LZMA2, `[0x00]` Copy).
    pub method_id: Vec<u8>,
    /// Optional coder properties (e.g. 1-byte LZMA2 dict prop).
    pub method_props: Vec<u8>,
    /// If true, file has no pack stream (empty file).
    pub empty: bool,
    /// Source file times / attributes (written only when defined).
    pub meta: FileMeta,
}

/// Write raw header bytes (starts with kHeader, ends with kEnd of header).
pub fn write_raw_header(h: &mut Vec<u8>, files: &[HeaderFile]) -> Result<()> {
    let content_files: Vec<&HeaderFile> = files.iter().filter(|f| !f.empty).collect();
    let empty_files: Vec<&HeaderFile> = files.iter().filter(|f| f.empty).collect();

    h.push(K_HEADER);
    h.push(K_MAIN_STREAMS_INFO);

    if !content_files.is_empty() {
        write_pack_info(h, &content_files)?;
        write_unpack_info(h, &content_files)?;
        write_substreams_info(h, &content_files)?;
    }
    h.push(K_END); // end main streams

    write_files_info(h, files, !empty_files.is_empty())?;
    h.push(K_END); // end header
    Ok(())
}

/// Build start signature header (32 bytes) for given end-header location/CRC.
pub fn write_start_header(
    next_header_offset: u64,
    next_header_size: u64,
    next_header_crc: u32,
) -> [u8; SIG_HEADER_SIZE as usize] {
    let mut sig = [0u8; SIG_HEADER_SIZE as usize];
    {
        let mut w = &mut sig[..];
        let _ = w.write_all(SIG);
        let _ = w.write_all(&[0, 4]); // version 0.4
        let _ = w.write_all(&0u32.to_le_bytes()); // placeholder start CRC
        let _ = w.write_all(&next_header_offset.to_le_bytes());
        let _ = w.write_all(&next_header_size.to_le_bytes());
        let _ = w.write_all(&next_header_crc.to_le_bytes());
    }
    let start_crc = crc32fast::hash(&sig[12..]);
    sig[8..12].copy_from_slice(&start_crc.to_le_bytes());
    sig
}

fn write_pack_info(h: &mut Vec<u8>, files: &[&HeaderFile]) -> Result<()> {
    h.push(K_PACK_INFO);
    write_u64(h, 0)?; // packPos = 0 (data starts right after signature)
    write_u64(h, files.len() as u64)?;
    h.push(K_SIZE);
    for f in files {
        write_u64(h, f.pack_size)?;
    }
    // Pack CRCs (all defined) — sevenz-rust2 always writes this block.
    h.push(K_CRC);
    h.push(1); // all defined
    for f in files {
        h.extend_from_slice(&f.pack_crc.to_le_bytes());
    }
    h.push(K_END);
    Ok(())
}

fn write_unpack_info(h: &mut Vec<u8>, files: &[&HeaderFile]) -> Result<()> {
    h.push(K_UNPACK_INFO);
    h.push(K_FOLDER);
    write_u64(h, files.len() as u64)?;
    h.push(0); // external = 0
    for f in files {
        write_folder(h, &f.method_id, &f.method_props)?;
    }
    h.push(K_CODERS_UNPACK_SIZE);
    for f in files {
        write_u64(h, f.unpack_size)?;
    }
    // sevenz-rust2 / 7-Zip: **no** folder CRCs here — digests live in SubStreamsInfo.
    h.push(K_END);
    Ok(())
}

fn write_substreams_info(h: &mut Vec<u8>, files: &[&HeaderFile]) -> Result<()> {
    // One unpack stream per folder (default) — omit kNumUnpackStream and kSize.
    h.push(K_SUB_STREAMS_INFO);
    h.push(K_CRC);
    h.push(1); // all defined
    for f in files {
        h.extend_from_slice(&f.content_crc.to_le_bytes());
    }
    h.push(K_END);
    Ok(())
}

fn write_folder(h: &mut Vec<u8>, method_id: &[u8], props: &[u8]) -> Result<()> {
    // numCoders = 1
    write_u64(h, 1)?;
    let id = if method_id.is_empty() {
        &[0x00u8][..]
    } else {
        method_id
    };
    // flags: low 4 bits = id size; bit 5 = has attributes
    let mut flags = (id.len() as u8) & 0x0F;
    if !props.is_empty() {
        flags |= 0x20;
    }
    h.push(flags);
    h.extend_from_slice(id);
    if !props.is_empty() {
        // sevenz-rust2 writes props length as a raw u8 (not full UINT64) for small sizes.
        // For sizes < 128 this matches UINT64 encoding; we follow sevenz-rust2 for compat.
        if props.len() < 128 {
            h.push(props.len() as u8);
        } else {
            write_u64(h, props.len() as u64)?;
        }
        h.extend_from_slice(props);
    }
    // simple coder: no bind pairs; single packed stream inferred by readers
    Ok(())
}

fn write_files_info(h: &mut Vec<u8>, files: &[HeaderFile], has_empty: bool) -> Result<()> {
    h.push(K_FILES_INFO);
    write_u64(h, files.len() as u64)?;

    if has_empty {
        // kEmptyStream bit vector over all files
        h.push(K_EMPTY_STREAM);
        let bits = bitset_bytes(files.len(), |i| files[i].empty);
        write_u64(h, bits.len() as u64)?;
        h.extend_from_slice(&bits);

        // kEmptyFile: among empty streams, which are files (not dirs). We only emit files.
        let empty_count = files.iter().filter(|f| f.empty).count();
        if empty_count > 0 {
            h.push(K_EMPTY_FILE);
            let bits = bitset_bytes(empty_count, |_| true);
            write_u64(h, bits.len() as u64)?;
            h.extend_from_slice(&bits);
        }
    }

    // Names
    h.push(K_NAME);
    let mut names = Vec::new();
    names.push(0); // external = 0
    for f in files {
        for c in f.name.encode_utf16() {
            names.extend_from_slice(&c.to_le_bytes());
        }
        names.extend_from_slice(&0u16.to_le_bytes());
    }
    write_u64(h, names.len() as u64)?;
    h.extend_from_slice(&names);

    // Optional times / attributes — only when at least one member defines them.
    write_optional_u64_prop(h, K_C_TIME, files, |f| f.meta.ctime)?;
    write_optional_u64_prop(h, K_A_TIME, files, |f| f.meta.atime)?;
    write_optional_u64_prop(h, K_M_TIME, files, |f| f.meta.mtime)?;
    write_optional_u32_prop(h, K_WIN_ATTRIBUTES, files, |f| f.meta.windows_attributes)?;

    h.push(K_END);
    Ok(())
}

/// Write a FilesInfo property of u64 values (times), matching sevenz-rust2 layout.
fn write_optional_u64_prop(
    h: &mut Vec<u8>,
    prop_id: u8,
    files: &[HeaderFile],
    get: impl Fn(&HeaderFile) -> Option<u64>,
) -> Result<()> {
    let defined: Vec<bool> = files.iter().map(|f| get(f).is_some()).collect();
    let num_defined = defined.iter().filter(|d| **d).count();
    if num_defined == 0 {
        return Ok(());
    }
    h.push(prop_id);
    let mut body = Vec::new();
    if num_defined == files.len() {
        body.push(1); // all defined
    } else {
        body.push(0);
        body.extend_from_slice(&bitset_bytes(files.len(), |i| defined[i]));
    }
    body.push(0); // external = 0
    for f in files {
        if let Some(v) = get(f) {
            body.extend_from_slice(&v.to_le_bytes());
        }
    }
    write_u64(h, body.len() as u64)?;
    h.extend_from_slice(&body);
    Ok(())
}

fn write_optional_u32_prop(
    h: &mut Vec<u8>,
    prop_id: u8,
    files: &[HeaderFile],
    get: impl Fn(&HeaderFile) -> Option<u32>,
) -> Result<()> {
    let defined: Vec<bool> = files.iter().map(|f| get(f).is_some()).collect();
    let num_defined = defined.iter().filter(|d| **d).count();
    if num_defined == 0 {
        return Ok(());
    }
    h.push(prop_id);
    let mut body = Vec::new();
    if num_defined == files.len() {
        body.push(1); // all defined
    } else {
        body.push(0);
        body.extend_from_slice(&bitset_bytes(files.len(), |i| defined[i]));
    }
    body.push(0); // external = 0
    for f in files {
        if let Some(v) = get(f) {
            body.extend_from_slice(&v.to_le_bytes());
        }
    }
    write_u64(h, body.len() as u64)?;
    h.extend_from_slice(&body);
    Ok(())
}

/// Bit set in 7z order (MSB of first byte = index 0), matching sevenz-rust2 BitSet write.
fn bitset_bytes(n: usize, mut is_set: impl FnMut(usize) -> bool) -> Vec<u8> {
    let nbytes = n.div_ceil(8);
    let mut bytes = vec![0u8; nbytes];
    for i in 0..n {
        if is_set(i) {
            let byte = i / 8;
            let bit = 7 - (i % 8);
            bytes[byte] |= 1 << bit;
        }
    }
    bytes
}

/// 7z UINT64 encoding (same algorithm as sevenz-rust2).
pub fn write_u64(h: &mut Vec<u8>, mut value: u64) -> Result<()> {
    let mut first: u64 = 0;
    let mut mask: u64 = 0x80;
    let mut i = 0u32;
    while i < 8 {
        if value < (1u64 << (7 * (i + 1))) {
            first |= value >> (8 * i);
            break;
        }
        first |= mask;
        mask >>= 1;
        i += 1;
    }
    h.push((first & 0xFF) as u8);
    while i > 0 {
        h.push((value & 0xFF) as u8);
        value >>= 8;
        i -= 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitset_msb_first() {
        // index 0 set only → 0x80
        assert_eq!(bitset_bytes(1, |i| i == 0), vec![0x80]);
        // first two of 8 set → 0xC0
        assert_eq!(bitset_bytes(8, |i| i < 2), vec![0xC0]);
    }

    #[test]
    fn header_starts_and_ends_correctly() {
        let files = vec![HeaderFile {
            name: "a.txt".into(),
            pack_size: 4,
            pack_crc: 1,
            unpack_size: 4,
            content_crc: 2,
            method_id: vec![0x00],
            method_props: vec![],
            empty: false,
            meta: FileMeta {
                mtime: Some(0x01D5C0_5A_A0_00_0000),
                windows_attributes: Some(ATTR_FILE),
                ..Default::default()
            },
        }];
        let mut h = Vec::new();
        write_raw_header(&mut h, &files).unwrap();
        assert_eq!(h[0], K_HEADER);
        assert_eq!(*h.last().unwrap(), K_END);
        assert!(h.contains(&K_SUB_STREAMS_INFO));
        assert!(h.contains(&K_WIN_ATTRIBUTES));
        assert!(h.contains(&K_M_TIME));
    }

    #[test]
    fn header_omits_times_when_undefined() {
        let files = vec![HeaderFile {
            name: "a.txt".into(),
            pack_size: 4,
            pack_crc: 1,
            unpack_size: 4,
            content_crc: 2,
            method_id: vec![0x00],
            method_props: vec![],
            empty: false,
            meta: FileMeta::default(),
        }];
        let mut h = Vec::new();
        write_raw_header(&mut h, &files).unwrap();
        assert!(!h.contains(&K_M_TIME), "must not invent mtime when undefined");
        assert!(
            !h.contains(&K_WIN_ATTRIBUTES),
            "must not invent attrs when undefined"
        );
    }

    #[test]
    fn system_time_roundtrip_epoch() {
        let ft = system_time_to_filetime(UNIX_EPOCH).unwrap();
        // Unix epoch FILETIME = 11644473600 * 10_000_000
        assert_eq!(ft, 11_644_473_600 * 10_000_000);
    }
}
