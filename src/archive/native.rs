//! Pure-Rust 7z backend via `sevenz-rust2` (Phase 1–3).
//!
//! Phase 1: streaming solid→non-solid (no full tree).
//! Phase 2: decode∥encode pipeline, size-aware LZMA2 MT.
//! Phase 3: pluggable LZMA2 codecs (pure Rust / liblzma) + windowed parallel encode +
//! streaming custom packer (packs written as they finish; no full-archive RAM hold).

use super::detect::format_from_path;
use super::{ArchiveBackend, ArchiveFormat, EntryMeta, PackOptions};
use crate::codec::{
    open_codec, CodecKind, FileMeta, Lzma2Codec, Lzma2Compressed, NonsolidLzma2Writer,
};
use crate::error::{Error, Result};
use crate::filter::MemberFilter;
use crate::util::pathnorm::{is_safe_member_path, normalize_member_path};
use sevenz_rust2::encoder_options::Lzma2Options;
use sevenz_rust2::{
    ArchiveEntry, ArchiveReader, ArchiveWriter, EncoderConfiguration, Password,
};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Cursor, Write};
use std::path::Path;
use std::sync::{mpsc, Arc};
use std::thread;
use walkdir::WalkDir;

/// Default size (bytes) above which a single entry uses multi-threaded LZMA2.
pub const DEFAULT_LARGE_FILE_THRESHOLD: u64 = 512 * 1024; // 512 KiB

/// How the native streaming convert schedules decode vs encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NativePipeline {
    /// Decode entry then encode immediately (Phase 1).
    Sequential,
    /// Decode ahead into a bounded queue while the previous entry encodes.
    DecodeAhead {
        /// Max decoded-but-not-yet-encoded entries held in memory.
        depth: usize,
    },
    /// Phase 3: solid-order decode → windowed **parallel LZMA2** → stream packs to disk.
    ///
    /// In-flight window ≈ encode thread count; a single huge file is always admitted alone.
    /// Packs are appended as they finish; header written at the end.
    #[default]
    ParallelCodec,
}

/// Phase 2/3 tuning knobs for the pure-Rust backend.
#[derive(Debug, Clone)]
pub struct NativeOptions {
    /// Threads for multi-threaded **decode** (LZMA2 when supported).
    pub decode_threads: u32,
    /// Threads for multi-threaded **encode** of large members (`None` = auto).
    pub encode_threads: Option<u32>,
    /// Decode/encode scheduling for streaming convert.
    pub pipeline: NativePipeline,
    /// Entries ≥ this size use LZMA2 multi-threaded encode (sevenz-rust2 path).
    pub large_file_threshold: u64,
    /// Parallel filesystem reads when packing a directory.
    pub parallel_pack_read: bool,
    /// Phase 3 LZMA2 codec for `ParallelCodec` pipeline.
    pub codec: CodecKind,
}

impl Default for NativeOptions {
    fn default() -> Self {
        let n = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1)
            .clamp(1, 256);
        Self {
            decode_threads: n,
            encode_threads: None,
            pipeline: NativePipeline::ParallelCodec,
            large_file_threshold: DEFAULT_LARGE_FILE_THRESHOLD,
            parallel_pack_read: true,
            codec: CodecKind::LibLzma, // prefer system liblzma for speed
        }
    }
}

/// Pure-Rust backend. No external 7z binary required for core operations.
#[derive(Debug, Clone)]
pub struct NativeSevenZ {
    pub options: NativeOptions,
}

impl Default for NativeSevenZ {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeSevenZ {
    pub fn new() -> Self {
        Self {
            options: NativeOptions::default(),
        }
    }

    pub fn with_options(options: NativeOptions) -> Self {
        Self { options }
    }

    fn open_reader(&self, archive: &Path) -> Result<ArchiveReader<File>> {
        let mut reader = ArchiveReader::open(archive, Password::empty()).map_err(map_err)?;
        reader.set_thread_count(self.options.decode_threads);
        Ok(reader)
    }

    fn encode_threads(&self, pack: &PackOptions) -> u32 {
        if let Some(t) = pack.threads {
            return t.max(1);
        }
        if let Some(t) = self.options.encode_threads {
            return t.max(1);
        }
        std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1)
            .clamp(1, 256)
    }

    /// Size-aware encoder: small files ST (better for many tiny members), large files MT.
    fn lzma2_config_for_size(&self, pack: &PackOptions, size: u64) -> EncoderConfiguration {
        let level = pack.level.min(9);
        let threads = self.encode_threads(pack);
        if size >= self.options.large_file_threshold && threads > 1 {
            // chunk_size: ~dict-sized minimum is enforced by the library
            let chunk = (size / threads as u64).max(1 << 20); // ≥1 MiB chunks when possible
            Lzma2Options::from_level_mt(level, threads, chunk).into()
        } else {
            Lzma2Options::from_level(level).into()
        }
    }

    fn lzma2_config_default(&self, pack: &PackOptions) -> EncoderConfiguration {
        // Used when size unknown (directory pack path for solid).
        self.lzma2_config_for_size(pack, self.options.large_file_threshold)
    }

    /// Streaming solid→non-solid without a full extract tree.
    pub fn stream_convert_to_nonsolid(
        &self,
        input: &Path,
        output: &Path,
        filter: &MemberFilter,
        pack: &PackOptions,
    ) -> Result<()> {
        match self.options.pipeline {
            NativePipeline::Sequential => {
                self.stream_convert_sequential(input, output, filter, pack)
            }
            NativePipeline::DecodeAhead { depth } => {
                self.stream_convert_pipeline(input, output, filter, pack, depth.max(1))
            }
            NativePipeline::ParallelCodec => {
                self.stream_convert_parallel_codec(input, output, filter, pack)
            }
        }
    }

    /// Phase 3: solid-order decode → windowed parallel LZMA2 → stream packs to disk.
    ///
    /// Decode never races ahead of more than `encode_threads` in-flight files
    /// (bounded `sync_channel`). Each file is compressed on a worker, then the
    /// pack is appended in solid/decode order via a small reorder map of
    /// **compressed** results only. Uncompressed buffers are dropped after encode.
    fn stream_convert_parallel_codec(
        &self,
        input: &Path,
        output: &Path,
        filter: &MemberFilter,
        pack: &PackOptions,
    ) -> Result<()> {
        prepare_output(output)?;
        let solid = {
            let reader = self.open_reader(input)?;
            reader.archive().is_solid
        };
        let workers = self.encode_threads(pack).max(1) as usize;
        let level = pack.level.min(9);
        let codec_kind = self.options.codec;
        let codec_name = open_codec(codec_kind).name();
        tracing::info!(
            input = %input.display(),
            solid,
            codec = codec_name,
            workers,
            pipeline = "parallel_codec_windowed",
            "native Phase 3 windowed parallel LZMA2 convert"
        );

        let input = input.to_path_buf();
        let decode_threads = self.options.decode_threads;
        let filter = filter.clone();

        // Backpressure: at most `workers` decoded-but-not-yet-encoded files.
        let (work_tx, work_rx) = mpsc::sync_channel::<EncodeJob>(workers);
        let (res_tx, res_rx) = mpsc::channel::<EncodeResult>();
        let (stats_tx, stats_rx) = mpsc::channel::<(u64, u64)>(); // (kept, skipped)

        // Decode thread: solid-order materialize → bounded work queue.
        let decode = thread::Builder::new()
            .name("native-p3-decode".into())
            .spawn(move || -> Result<()> {
                let mut reader =
                    ArchiveReader::open(&input, Password::empty()).map_err(map_err)?;
                reader.set_thread_count(decode_threads);
                let mut kept = 0u64;
                let mut skipped = 0u64;
                let mut id = 0usize;
                let decode_result = reader.for_each_entries(|entry, r| {
                    if entry.is_directory() {
                        return Ok(true);
                    }
                    let path = normalize_member_path(entry.name());
                    if !is_safe_member_path(&path) || !filter.should_keep(&path) {
                        io::copy(r, &mut io::sink()).map_err(sevenz_rust2::Error::from)?;
                        skipped += 1;
                        return Ok(true);
                    }
                    let meta = file_meta_from_entry(entry);
                    let mut buf = Vec::with_capacity(entry.size.min(8 << 20) as usize);
                    io::copy(r, &mut buf).map_err(sevenz_rust2::Error::from)?;
                    // Blocks when `workers` jobs are already in flight.
                    if work_tx
                        .send(EncodeJob {
                            id,
                            name: path,
                            data: buf,
                            meta,
                        })
                        .is_err()
                    {
                        return Ok(false); // encode side gone
                    }
                    id += 1;
                    kept += 1;
                    Ok(true)
                });
                // Close work queue so encode workers finish (Sender dropped).
                drop(work_tx);
                decode_result.map_err(map_err)?;
                let _ = stats_tx.send((kept, skipped));
                Ok(())
            })
            .map_err(|e| Error::Other(format!("spawn p3 decode: {e}")))?;

        // Encode thread: drain work queue with rayon, emit compressed packs.
        let encode = thread::Builder::new()
            .name("native-p3-encode".into())
            .spawn(move || -> Result<()> {
                use rayon::prelude::*;
                let codec: Arc<dyn Lzma2Codec> = Arc::from(open_codec(codec_kind));
                // Single-consumer channel → parallel bridge over jobs.
                work_rx
                    .into_iter()
                    .par_bridge()
                    .try_for_each(|job| -> Result<()> {
                        let compressed = codec.compress(&job.data, level)?;
                        // Uncompressed `job.data` dropped here as job is consumed.
                        res_tx
                            .send(EncodeResult {
                                id: job.id,
                                name: job.name,
                                compressed,
                                meta: job.meta,
                            })
                            .map_err(|_| Error::Other("p3 result channel closed".into()))?;
                        Ok(())
                    })?;
                // res_tx dropped → writer sees EOF
                Ok(())
            })
            .map_err(|e| Error::Other(format!("spawn p3 encode: {e}")))?;

        // Writer (this thread): reorder by decode id, append packs, finish header.
        let mut writer = NonsolidLzma2Writer::create(output)?;
        let mut pending: BTreeMap<usize, EncodeResult> = BTreeMap::new();
        let mut next_id = 0usize;
        for item in res_rx {
            pending.insert(item.id, item);
            while let Some(done) = pending.remove(&next_id) {
                writer.push_packed(done.name, done.compressed, done.meta)?;
                next_id += 1;
            }
        }

        encode
            .join()
            .map_err(|_| Error::Other("p3 encode thread panicked".into()))??;
        decode
            .join()
            .map_err(|_| Error::Other("p3 decode thread panicked".into()))??;

        let (kept, skipped) = stats_rx
            .recv()
            .map_err(|_| Error::Other("p3 decode stats missing".into()))?;

        if writer.is_empty() {
            // Remove empty placeholder file.
            let _ = fs::remove_file(output);
            return Err(Error::Other(
                "no files left after filters; refusing empty archive".into(),
            ));
        }

        let kept_written = writer.len() as u64;
        writer.finish()?;
        tracing::info!(
            kept = kept_written,
            skipped,
            workers,
            codec = codec_name,
            output = %output.display(),
            "native parallel_codec windowed convert finished"
        );
        debug_assert_eq!(kept, kept_written);
        let _ = kept; // used in debug_assert; silence release unused
        Ok(())
    }

    /// Phase 1 path: decode then encode each entry on the same thread.
    fn stream_convert_sequential(
        &self,
        input: &Path,
        output: &Path,
        filter: &MemberFilter,
        pack: &PackOptions,
    ) -> Result<()> {
        prepare_output(output)?;
        let mut reader = self.open_reader(input)?;
        let solid = reader.archive().is_solid;
        tracing::info!(
            input = %input.display(),
            solid,
            pipeline = "sequential",
            "native streaming solid→non-solid"
        );

        let mut writer = ArchiveWriter::create(output).map_err(map_err)?;
        let mut kept = 0u64;
        let mut skipped = 0u64;

        reader
            .for_each_entries(|entry, r| {
                process_entry_stream(
                    entry,
                    r,
                    filter,
                    &mut writer,
                    pack,
                    self,
                    &mut kept,
                    &mut skipped,
                )
            })
            .map_err(map_err)?;

        writer.finish().map_err(|e| Error::Other(e.to_string()))?;
        tracing::info!(kept, skipped, output = %output.display(), "native convert finished");
        Ok(())
    }

    /// Phase 2 path: solid-order decode into a bounded queue; encode on another thread.
    ///
    /// Overlaps LZMA2 encode of entry N with decode of entry N+1…N+depth.
    fn stream_convert_pipeline(
        &self,
        input: &Path,
        output: &Path,
        filter: &MemberFilter,
        pack: &PackOptions,
        depth: usize,
    ) -> Result<()> {
        prepare_output(output)?;
        let solid = {
            let reader = self.open_reader(input)?;
            reader.archive().is_solid
        };
        tracing::info!(
            input = %input.display(),
            solid,
            pipeline = "decode_ahead",
            depth,
            "native streaming solid→non-solid"
        );

        // Owned filter/pack for the decode thread.
        let filter = filter.clone();
        let input = input.to_path_buf();
        let decode_threads = self.options.decode_threads;
        let large_threshold = self.options.large_file_threshold;

        let (tx, rx) = mpsc::sync_channel::<DecodedEntry>(depth);

        let decode = thread::Builder::new()
            .name("native-decode".into())
            .spawn(move || -> Result<(u64, u64)> {
                let mut reader =
                    ArchiveReader::open(&input, Password::empty()).map_err(map_err)?;
                reader.set_thread_count(decode_threads);
                let mut kept = 0u64;
                let mut skipped = 0u64;
                reader
                    .for_each_entries(|entry, r| {
                        if entry.is_directory() {
                            return Ok(true);
                        }
                        let path = normalize_member_path(entry.name());
                        if !is_safe_member_path(&path) || !filter.should_keep(&path) {
                            io::copy(r, &mut io::sink()).map_err(sevenz_rust2::Error::from)?;
                            skipped += 1;
                            return Ok(true);
                        }
                        let meta = file_meta_from_entry(entry);
                        let mut buf = Vec::with_capacity(entry.size.min(8 << 20) as usize);
                        io::copy(r, &mut buf).map_err(sevenz_rust2::Error::from)?;
                        let size = buf.len() as u64;
                        // Back-pressure via sync_channel.
                        if tx
                            .send(DecodedEntry {
                                path,
                                data: buf,
                                size,
                                meta,
                            })
                            .is_err()
                        {
                            return Ok(false); // encoder side gone
                        }
                        kept += 1;
                        Ok(true)
                    })
                    .map_err(map_err)?;
                // tx dropped → encoder sees EOF
                Ok((kept, skipped))
            })
            .map_err(|e| Error::Other(format!("spawn decode thread: {e}")))?;

        let mut writer = ArchiveWriter::create(output).map_err(map_err)?;
        // Default methods; may be overridden per large entry.
        let encode_threads = self.encode_threads(pack);
        let level = pack.level.min(9);

        while let Ok(item) = rx.recv() {
            let cfg = if item.size >= large_threshold && encode_threads > 1 {
                let chunk = (item.size / encode_threads as u64).max(1 << 20);
                Lzma2Options::from_level_mt(level, encode_threads, chunk).into()
            } else {
                Lzma2Options::from_level(level).into()
            };
            writer.set_content_methods(vec![cfg]);
            let ae = archive_entry_with_meta(&item.path, &item.meta);
            writer
                .push_archive_entry(ae, Some(Cursor::new(item.data)))
                .map_err(map_err)?;
        }

        let (kept, skipped) = decode
            .join()
            .map_err(|_| Error::Other("native decode thread panicked".into()))??;

        writer.finish().map_err(|e| Error::Other(e.to_string()))?;
        tracing::info!(
            kept,
            skipped,
            depth,
            output = %output.display(),
            "native pipeline convert finished"
        );
        Ok(())
    }
}

struct DecodedEntry {
    path: String,
    data: Vec<u8>,
    size: u64,
    meta: FileMeta,
}

/// One decoded file waiting for LZMA2 encode (Phase 3 window).
struct EncodeJob {
    id: usize,
    name: String,
    data: Vec<u8>,
    meta: FileMeta,
}

/// Compressed pack ready to append (Phase 3).
struct EncodeResult {
    id: usize,
    name: String,
    compressed: Lzma2Compressed,
    meta: FileMeta,
}

fn file_meta_from_entry(entry: &ArchiveEntry) -> FileMeta {
    FileMeta {
        mtime: if entry.has_last_modified_date {
            Some(u64::from(entry.last_modified_date))
        } else {
            None
        },
        ctime: if entry.has_creation_date {
            Some(u64::from(entry.creation_date))
        } else {
            None
        },
        atime: if entry.has_access_date {
            Some(u64::from(entry.access_date))
        } else {
            None
        },
        windows_attributes: if entry.has_windows_attributes {
            Some(entry.windows_attributes)
        } else {
            None
        },
    }
}

fn archive_entry_with_meta(path: &str, meta: &FileMeta) -> ArchiveEntry {
    let mut ae = ArchiveEntry::new_file(path);
    if let Some(t) = meta.mtime {
        ae.has_last_modified_date = true;
        ae.last_modified_date = t.into();
    }
    if let Some(t) = meta.ctime {
        ae.has_creation_date = true;
        ae.creation_date = t.into();
    }
    if let Some(t) = meta.atime {
        ae.has_access_date = true;
        ae.access_date = t.into();
    }
    if let Some(a) = meta.windows_attributes {
        ae.has_windows_attributes = true;
        ae.windows_attributes = a;
    }
    ae
}

fn process_entry_stream(
    entry: &ArchiveEntry,
    r: &mut dyn Read,
    filter: &MemberFilter,
    writer: &mut ArchiveWriter<File>,
    pack: &PackOptions,
    native: &NativeSevenZ,
    kept: &mut u64,
    skipped: &mut u64,
) -> std::result::Result<bool, sevenz_rust2::Error> {
    if entry.is_directory() {
        return Ok(true);
    }
    let path = normalize_member_path(entry.name());
    if !is_safe_member_path(&path) || !filter.should_keep(&path) {
        io::copy(r, &mut io::sink()).map_err(sevenz_rust2::Error::from)?;
        *skipped += 1;
        return Ok(true);
    }

    // Buffer so we can pick ST vs MT encode from actual size.
    let meta = file_meta_from_entry(entry);
    let mut buf = Vec::with_capacity(entry.size.min(8 << 20) as usize);
    io::copy(r, &mut buf).map_err(sevenz_rust2::Error::from)?;
    let size = buf.len() as u64;
    writer.set_content_methods(vec![native.lzma2_config_for_size(pack, size)]);
    let ae = archive_entry_with_meta(&path, &meta);
    writer
        .push_archive_entry(ae, Some(Cursor::new(buf)))
        .map_err(|e| {
            sevenz_rust2::Error::from(io::Error::other(format!("encode entry {path}: {e}")))
        })?;
    *kept += 1;
    Ok(true)
}

// Need Read in scope for dyn Read
use std::io::Read;

fn prepare_output(output: &Path) -> Result<()> {
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    if output.exists() {
        fs::remove_file(output)?;
    }
    Ok(())
}

impl ArchiveBackend for NativeSevenZ {
    fn format(&self) -> ArchiveFormat {
        ArchiveFormat::SevenZ
    }

    fn list(&self, archive: &Path) -> Result<Vec<EntryMeta>> {
        let reader = self.open_reader(archive)?;
        let mut out = Vec::new();
        for f in &reader.archive().files {
            let path = normalize_member_path(f.name());
            let is_dir = f.is_directory();
            let format_hint = if is_dir {
                ArchiveFormat::Unknown
            } else {
                format_from_path(&path)
            };
            out.push(EntryMeta {
                path,
                size: f.size,
                is_dir,
                format_hint,
                meta: file_meta_from_entry(f),
            });
        }
        Ok(out)
    }

    fn is_solid(&self, archive: &Path) -> Result<bool> {
        let reader = self.open_reader(archive)?;
        Ok(reader.archive().is_solid)
    }

    fn extract_member(&self, archive: &Path, member: &str, dest_file: &Path) -> Result<()> {
        if !is_safe_member_path(member) {
            return Err(Error::Other(format!(
                "refusing to extract unsafe member path: {member}"
            )));
        }
        if let Some(parent) = dest_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let want = normalize_member_path(member);
        let mut reader = self.open_reader(archive)?;
        let data = reader.read_file(&want).map_err(map_err)?;
        let mut f = File::create(dest_file)?;
        f.write_all(&data)?;
        Ok(())
    }

    fn extract_members(&self, archive: &Path, members: &[&str], dest_dir: &Path) -> Result<()> {
        if members.is_empty() {
            return Ok(());
        }
        for m in members {
            if !is_safe_member_path(m) {
                return Err(Error::Other(format!(
                    "refusing to extract unsafe member path: {m}"
                )));
            }
        }
        fs::create_dir_all(dest_dir)?;
        let want: std::collections::HashSet<String> =
            members.iter().map(|m| normalize_member_path(m)).collect();
        let mut reader = self.open_reader(archive)?;
        reader
            .for_each_entries(|entry, r| {
                if entry.is_directory() {
                    return Ok(true);
                }
                let path = normalize_member_path(entry.name());
                if !is_safe_member_path(&path) {
                    io::copy(r, &mut io::sink()).map_err(sevenz_rust2::Error::from)?;
                    return Ok(true);
                }
                if want.contains(&path) {
                    let dest = dest_dir.join(&path);
                    if let Some(p) = dest.parent() {
                        fs::create_dir_all(p).map_err(sevenz_rust2::Error::from)?;
                    }
                    let mut f = File::create(&dest).map_err(sevenz_rust2::Error::from)?;
                    io::copy(r, &mut f).map_err(sevenz_rust2::Error::from)?;
                } else {
                    io::copy(r, &mut io::sink()).map_err(sevenz_rust2::Error::from)?;
                }
                Ok(true)
            })
            .map_err(map_err)?;
        Ok(())
    }

    fn extract_all(&self, archive: &Path, dest_dir: &Path) -> Result<()> {
        fs::create_dir_all(dest_dir)?;
        sevenz_rust2::decompress_file(archive, dest_dir).map_err(map_err)
    }

    fn extract_all_with_excludes(
        &self,
        archive: &Path,
        dest_dir: &Path,
        exclude_globs: &[String],
    ) -> Result<()> {
        self.extract_all(archive, dest_dir)?;
        if exclude_globs.is_empty() {
            return Ok(());
        }
        for entry in WalkDir::new(dest_dir).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(dest_dir)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            if exclude_globs.iter().any(|g| glob_match_simple(g, &rel)) {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    fn pack_dir(&self, src_dir: &Path, dest_archive: &Path, opts: &PackOptions) -> Result<()> {
        if let Some(parent) = dest_archive.parent() {
            fs::create_dir_all(parent)?;
        }
        if dest_archive.exists() {
            fs::remove_file(dest_archive)?;
        }

        let mut writer = ArchiveWriter::create(dest_archive).map_err(map_err)?;

        if opts.non_solid {
            // Collect files (optionally parallel read), then encode sequentially with size-aware MT.
            let paths: Vec<_> = WalkDir::new(src_dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .map(|e| e.into_path())
                .collect();

            let mut items: Vec<(String, Vec<u8>, FileMeta)> = if self.options.parallel_pack_read
                && paths.len() > 32
            {
                use rayon::prelude::*;
                paths
                    .par_iter()
                    .filter_map(|path| {
                        let rel = path
                            .strip_prefix(src_dir)
                            .unwrap_or(path)
                            .to_string_lossy()
                            .replace('\\', "/");
                        let rel = normalize_member_path(&rel);
                        if !is_safe_member_path(&rel) {
                            return None;
                        }
                        let meta = FileMeta::from_fs_path(path);
                        let data = fs::read(path).ok()?;
                        Some((rel, data, meta))
                    })
                    .collect()
            } else {
                let mut v = Vec::new();
                for path in &paths {
                    let rel = path
                        .strip_prefix(src_dir)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    let rel = normalize_member_path(&rel);
                    if !is_safe_member_path(&rel) {
                        continue;
                    }
                    let meta = FileMeta::from_fs_path(path);
                    v.push((rel, fs::read(path)?, meta));
                }
                v
            };

            // Stable order for reproducible archives
            items.sort_by(|a, b| a.0.cmp(&b.0));

            for (rel, data, meta) in items {
                let size = data.len() as u64;
                writer.set_content_methods(vec![self.lzma2_config_for_size(opts, size)]);
                let ae = archive_entry_with_meta(&rel, &meta);
                writer
                    .push_archive_entry(ae, Some(Cursor::new(data)))
                    .map_err(map_err)?;
            }
        } else {
            writer.set_content_methods(vec![self.lzma2_config_default(opts)]);
            writer
                .push_source_path(src_dir, |_| true)
                .map_err(map_err)?;
        }
        writer.finish().map_err(|e| Error::Other(e.to_string()))?;
        Ok(())
    }

    fn test(&self, archive: &Path) -> Result<()> {
        let mut reader = self.open_reader(archive)?;
        reader
            .for_each_entries(|_e, r| {
                io::copy(r, &mut io::sink()).map_err(sevenz_rust2::Error::from)?;
                Ok(true)
            })
            .map_err(map_err)?;
        Ok(())
    }

    fn convert_to_nonsolid_streaming(
        &self,
        input: &Path,
        output: &Path,
        filter: &MemberFilter,
        pack: &PackOptions,
    ) -> Result<bool> {
        self.stream_convert_to_nonsolid(input, output, filter, pack)?;
        Ok(true)
    }
}

fn map_err(e: sevenz_rust2::Error) -> Error {
    Error::Other(format!("native 7z: {e}"))
}

fn glob_match_simple(pattern: &str, path: &str) -> bool {
    if let Some(ext) = pattern.strip_prefix("*.") {
        return path.ends_with(&format!(".{ext}"))
            || path
                .to_ascii_lowercase()
                .ends_with(&format!(".{}", ext.to_ascii_lowercase()));
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        return path == prefix || path.starts_with(&format!("{prefix}/"));
    }
    path == pattern || path.ends_with(&format!("/{pattern}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::PackOptions;
    use crate::codec::CodecKind;

    fn read_member(backend: &NativeSevenZ, archive: &Path, member: &str) -> Vec<u8> {
        let want = normalize_member_path(member);
        let mut reader = backend.open_reader(archive).unwrap();
        reader.read_file(&want).map_err(map_err).unwrap()
    }

    #[test]
    fn extract_rejects_unsafe_member_paths() {
        let backend = NativeSevenZ::new();
        let dummy = Path::new("missing.7z");
        let dest = tempfile::tempdir().unwrap();
        let err = backend
            .extract_member(dummy, "../evil", &dest.path().join("x"))
            .unwrap_err();
        assert!(err.to_string().contains("unsafe"), "{err}");
        let err = backend
            .extract_members(dummy, &["/abs/path"], dest.path())
            .unwrap_err();
        assert!(err.to_string().contains("unsafe"), "{err}");
    }

    #[test]
    fn native_roundtrip_nonsolid() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("t");
        fs::create_dir_all(tree.join("sub")).unwrap();
        fs::write(tree.join("a.txt"), b"hello").unwrap();
        fs::write(tree.join("sub/b.txt"), b"world").unwrap();

        let arch = dir.path().join("out.7z");
        let backend = NativeSevenZ::new();
        backend
            .pack_dir(
                &tree,
                &arch,
                &PackOptions {
                    non_solid: true,
                    threads: Some(1),
                    level: 1,
                },
            )
            .unwrap();
        assert!(!backend.is_solid(&arch).unwrap());

        let list = backend.list(&arch).unwrap();
        let names: Vec<_> = list
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.path.as_str())
            .collect();
        assert!(names.iter().any(|n| n.ends_with("a.txt")));
    }

    #[test]
    fn streaming_pipeline_drops_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("t");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("keep.txt"), b"keep").unwrap();
        fs::write(tree.join("drop.tmp"), b"drop").unwrap();

        let solid = dir.path().join("solid.7z");
        let mut opts = NativeOptions::default();
        opts.pipeline = NativePipeline::DecodeAhead { depth: 2 };
        opts.encode_threads = Some(1);
        let backend = NativeSevenZ::with_options(opts);
        backend
            .pack_dir(
                &tree,
                &solid,
                &PackOptions {
                    non_solid: false,
                    threads: Some(1),
                    level: 1,
                },
            )
            .unwrap();

        let out = dir.path().join("nonsolid.7z");
        let filter = MemberFilter::with_excludes([r"(?i)\.tmp$"]).unwrap();
        backend
            .stream_convert_to_nonsolid(
                &solid,
                &out,
                &filter,
                &PackOptions {
                    non_solid: true,
                    threads: Some(1),
                    level: 1,
                },
            )
            .unwrap();

        assert!(!backend.is_solid(&out).unwrap());
        let names: Vec<_> = backend
            .list(&out)
            .unwrap()
            .into_iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.path)
            .collect();
        assert!(names.iter().any(|n| n.ends_with("keep.txt")), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with(".tmp")), "{names:?}");
    }

    #[test]
    fn parallel_codec_liblzma_drops_excluded_and_extracts() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("t");
        fs::create_dir_all(tree.join("sub")).unwrap();
        fs::write(tree.join("keep.txt"), b"keep-content-aaaa").unwrap();
        fs::write(tree.join("sub/more.txt"), b"more-content-bbbb").unwrap();
        fs::write(tree.join("drop.tmp"), b"tmp").unwrap();

        let solid = dir.path().join("solid.7z");
        let packer = NativeSevenZ::with_options(NativeOptions {
            pipeline: NativePipeline::Sequential,
            encode_threads: Some(1),
            ..NativeOptions::default()
        });
        packer
            .pack_dir(
                &tree,
                &solid,
                &PackOptions {
                    non_solid: false,
                    threads: Some(1),
                    level: 1,
                },
            )
            .unwrap();

        let mut opts = NativeOptions::default();
        opts.pipeline = NativePipeline::ParallelCodec;
        opts.codec = CodecKind::LibLzma;
        let backend = NativeSevenZ::with_options(opts);
        let out = dir.path().join("nonsolid.7z");
        let filter = MemberFilter::with_excludes([r"(?i)\.tmp$"]).unwrap();
        backend
            .stream_convert_to_nonsolid(
                &solid,
                &out,
                &filter,
                &PackOptions {
                    non_solid: true,
                    threads: Some(4),
                    level: 1,
                },
            )
            .unwrap();

        assert!(!backend.is_solid(&out).unwrap());
        backend.test(&out).unwrap();
        assert_eq!(read_member(&backend, &out, "keep.txt"), b"keep-content-aaaa");
        assert_eq!(
            read_member(&backend, &out, "sub/more.txt"),
            b"more-content-bbbb"
        );
        let names: Vec<_> = backend
            .list(&out)
            .unwrap()
            .into_iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.path)
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with(".tmp")), "{names:?}");
    }

    #[test]
    fn parallel_codec_pure_rust_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("t");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("x.dat"), b"pure-rust phase3 payload ".repeat(20)).unwrap();

        let solid = dir.path().join("solid.7z");
        let packer = NativeSevenZ::with_options(NativeOptions {
            pipeline: NativePipeline::Sequential,
            encode_threads: Some(1),
            ..NativeOptions::default()
        });
        packer
            .pack_dir(
                &tree,
                &solid,
                &PackOptions {
                    non_solid: false,
                    threads: Some(1),
                    level: 1,
                },
            )
            .unwrap();

        let backend = NativeSevenZ::with_options(NativeOptions {
            pipeline: NativePipeline::ParallelCodec,
            codec: CodecKind::PureRust,
            ..NativeOptions::default()
        });
        let out = dir.path().join("out.7z");
        backend
            .stream_convert_to_nonsolid(
                &solid,
                &out,
                &MemberFilter::default(),
                &PackOptions {
                    non_solid: true,
                    threads: Some(2),
                    level: 1,
                },
            )
            .unwrap();
        backend.test(&out).unwrap();
        assert_eq!(
            read_member(&backend, &out, "x.dat"),
            b"pure-rust phase3 payload ".repeat(20)
        );
    }
}
