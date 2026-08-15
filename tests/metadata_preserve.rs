//! Regression: repack paths must preserve member Modified times and attributes.
//!
//! Pre-fix behavior (custom header writer + native streaming) overwrote all
//! mtimes with conversion-time "now" or dropped them. These tests seed distinct
//! historical timestamps and assert they survive convert-single and nested
//! outer 7z store append.

mod common;

use archiveconverter::archive::native::{NativeOptions, NativePipeline, NativeSevenZ};
use archiveconverter::archive::{open_backend, ArchiveBackend, BackendKind, PackOptions};
use archiveconverter::codec::FileMeta;
use archiveconverter::filter::MemberFilter;
use archiveconverter::pipeline::{convert_single, run, PipelineOptions};
use common::*;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// Seeded historical mtimes (seconds since Unix epoch), distinct per member.
const MTIME_A: u64 = 1_579_090_000; // ~2020-01-15
const MTIME_B: u64 = 1_527_840_000; // ~2018-06-01
const MTIME_README: u64 = 1_514_160_000; // ~2017-12-25
const MTIME_NEST: u64 = 1_552_200_000; // ~2019-03-10

fn set_mtime(path: &Path, unix_secs: u64) {
    common::set_mtime(path, unix_secs);
}

#[test]
fn set_mtime_helper_roundtrips_without_gnu_touch() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("seed.txt");
    fs::write(&p, b"x").unwrap();
    set_mtime(&p, MTIME_A);
    let got = fs::metadata(&p)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(got, MTIME_A);
}

fn unix_to_filetime(unix_secs: u64) -> u64 {
    (unix_secs + 11_644_473_600) * 10_000_000
}

/// Map of member path → Modified FILETIME from native listing.
fn native_mtimes(archive: &Path) -> BTreeMap<String, u64> {
    let native = NativeSevenZ::new();
    let list = native.list(archive).expect("native list");
    let mut m = BTreeMap::new();
    for e in list {
        if e.is_dir {
            continue;
        }
        if let Some(ft) = e.meta.mtime {
            m.insert(e.path, ft);
        }
    }
    m
}

fn native_attrs(archive: &Path) -> BTreeMap<String, u32> {
    let native = NativeSevenZ::new();
    let list = native.list(archive).expect("native list");
    let mut m = BTreeMap::new();
    for e in list {
        if e.is_dir {
            continue;
        }
        if let Some(a) = e.meta.windows_attributes {
            m.insert(e.path, a);
        }
    }
    m
}

/// Parse `7zz l -slt` Path → Modified display string (when 7zz is available).
fn sevenz_modified_map(archive: &Path) -> Option<BTreeMap<String, String>> {
    let bin = archiveconverter::archive::find_7z_binary().ok()?;
    let out = Command::new(bin)
        .args(["l", "-slt", "-ba", archive.to_str()?])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut map = BTreeMap::new();
    let mut path: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Path = ") {
            path = Some(rest.trim().replace('\\', "/"));
        } else if let Some(rest) = line.strip_prefix("Modified = ") {
            if let Some(p) = path.take() {
                // Strip fractional zeros for stable compare: "2020-01-15 13:00:00.0000000"
                let m = rest.trim().to_string();
                map.insert(p, m);
            }
        }
    }
    Some(map)
}

fn assert_mtimes_match(src: &Path, dst: &Path, members: &[&str]) {
    let src_m = native_mtimes(src);
    let dst_m = native_mtimes(dst);
    for name in members {
        let s = src_m
            .iter()
            .find(|(p, _)| p.ends_with(name) || p.as_str() == *name)
            .map(|(_, v)| *v)
            .unwrap_or_else(|| panic!("source missing mtime for {name}: {src_m:?}"));
        let d = dst_m
            .iter()
            .find(|(p, _)| p.ends_with(name) || p.as_str() == *name)
            .map(|(_, v)| *v)
            .unwrap_or_else(|| panic!("dest missing mtime for {name}: {dst_m:?}"));
        assert_eq!(
            s, d,
            "mtime FILETIME mismatch for {name}: src={s} dest={d} (archive {src:?} → {dst:?})"
        );
        // Must not be "now" (within last 2 days of wall clock would still be far from 2018/2020).
        let now_ft = unix_to_filetime(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        );
        let two_days = 2 * 86_400 * 10_000_000u64;
        assert!(
            d + two_days < now_ft || now_ft + two_days < d,
            "mtime for {name} looks like conversion-time now ({d} vs now {now_ft})"
        );
    }

    // Cross-check with 7zz listing strings when available.
    if let (Some(sm), Some(dm)) = (sevenz_modified_map(src), sevenz_modified_map(dst)) {
        for name in members {
            let s = sm
                .iter()
                .find(|(p, _)| p.ends_with(name) || p.as_str() == *name)
                .map(|(_, v)| v.clone());
            let d = dm
                .iter()
                .find(|(p, _)| p.ends_with(name) || p.as_str() == *name)
                .map(|(_, v)| v.clone());
            assert_eq!(
                s, d,
                "7zz Modified string mismatch for {name}: src={s:?} dest={d:?}"
            );
        }
    }
}

fn assert_attrs_preserved(src: &Path, dst: &Path, members: &[&str]) {
    let src_a = native_attrs(src);
    let dst_a = native_attrs(dst);
    for name in members {
        let Some(s) = src_a
            .iter()
            .find(|(p, _)| p.ends_with(name) || p.as_str() == *name)
            .map(|(_, v)| *v)
        else {
            continue; // source had no attrs defined
        };
        let d = dst_a
            .iter()
            .find(|(p, _)| p.ends_with(name) || p.as_str() == *name)
            .map(|(_, v)| *v)
            .unwrap_or_else(|| panic!("dest missing attrs for {name} (src had {s:#x})"));
        assert_eq!(
            s, d,
            "windows attributes mismatch for {name}: src={s:#x} dest={d:#x}"
        );
    }
}

/// Solid 7z with two files at known distinct mtimes.
fn make_solid_with_mtimes(root: &Path) -> PathBuf {
    ensure_7z();
    let tree = root.join("tree");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("old_a.txt"), b"alpha content\n").unwrap();
    fs::write(tree.join("old_b.txt"), b"beta content!!\n").unwrap();
    set_mtime(&tree.join("old_a.txt"), MTIME_A);
    set_mtime(&tree.join("old_b.txt"), MTIME_B);
    let solid = root.join("solid.7z");
    pack_solid(&tree, &solid);
    // Ensure archive actually carries the times (7zz pack reads FS mtime).
    let m = native_mtimes(&solid);
    assert!(
        m.values().any(|t| *t == unix_to_filetime(MTIME_A))
            || m.values().any(|&t| (t / 10_000_000) == MTIME_A + 11_644_473_600
                || (t / 10_000_000).abs_diff(MTIME_A + 11_644_473_600) <= 1),
        "fixture solid missing expected mtime A: {m:?}"
    );
    solid
}

fn pack_opts() -> PackOptions {
    PackOptions {
        non_solid: true,
        threads: Some(1),
        level: 1,
    }
}

#[test]
fn native_phase3_convert_single_preserves_mtime_and_attrs() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let solid = make_solid_with_mtimes(root.path());
    let out = root.path().join("out-p3.7z");

    let native = NativeSevenZ::with_options(NativeOptions {
        pipeline: NativePipeline::ParallelCodec,
        encode_threads: Some(1),
        ..Default::default()
    });
    let filter = MemberFilter::new();
    native
        .stream_convert_to_nonsolid(&solid, &out, &filter, &pack_opts())
        .unwrap();

    assert_mtimes_match(&solid, &out, &["old_a.txt", "old_b.txt"]);
    assert_attrs_preserved(&solid, &out, &["old_a.txt", "old_b.txt"]);
    // Distinct times must both survive (not collapsed to one shared "now").
    let m = native_mtimes(&out);
    let va = m
        .iter()
        .find(|(p, _)| p.ends_with("old_a.txt"))
        .map(|(_, v)| *v)
        .unwrap();
    let vb = m
        .iter()
        .find(|(p, _)| p.ends_with("old_b.txt"))
        .map(|(_, v)| *v)
        .unwrap();
    assert_ne!(va, vb, "both members got the same mtime — likely shared conversion time");
}

#[test]
fn native_decode_ahead_convert_preserves_mtime() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let solid = make_solid_with_mtimes(root.path());
    let out = root.path().join("out-ahead.7z");

    let native = NativeSevenZ::with_options(NativeOptions {
        pipeline: NativePipeline::DecodeAhead { depth: 2 },
        encode_threads: Some(1),
        ..Default::default()
    });
    native
        .stream_convert_to_nonsolid(&solid, &out, &MemberFilter::new(), &pack_opts())
        .unwrap();

    assert_mtimes_match(&solid, &out, &["old_a.txt", "old_b.txt"]);
}

#[test]
fn native_sequential_convert_preserves_mtime() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let solid = make_solid_with_mtimes(root.path());
    let out = root.path().join("out-seq.7z");

    let native = NativeSevenZ::with_options(NativeOptions {
        pipeline: NativePipeline::Sequential,
        encode_threads: Some(1),
        ..Default::default()
    });
    native
        .stream_convert_to_nonsolid(&solid, &out, &MemberFilter::new(), &pack_opts())
        .unwrap();

    assert_mtimes_match(&solid, &out, &["old_a.txt", "old_b.txt"]);
}

#[test]
fn store_writer_preserves_explicit_meta_not_now() {
    use archiveconverter::codec::NonsolidStoreWriter;

    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a.bin");
    let b = root.path().join("b.txt");
    fs::write(&a, b"nested-like-payload").unwrap();
    fs::write(&b, b"readme").unwrap();
    // FS times = "now"; explicit meta must win.
    let meta_a = FileMeta {
        mtime: Some(unix_to_filetime(MTIME_NEST)),
        windows_attributes: Some(0x20 | (0o100644 << 16)),
        ..Default::default()
    };
    let meta_b = FileMeta {
        mtime: Some(unix_to_filetime(MTIME_README)),
        windows_attributes: Some(0x20 | (0o100644 << 16)),
        ..Default::default()
    };
    let out = root.path().join("outer.7z");
    let mut w = NonsolidStoreWriter::create(&out).unwrap();
    w.push_path_with_meta("nest.7z".into(), &a, Some(meta_a.clone()))
        .unwrap();
    w.push_path_with_meta("readme.txt".into(), &b, Some(meta_b.clone()))
        .unwrap();
    w.finish().unwrap();

    let m = native_mtimes(&out);
    assert_eq!(
        m.get("nest.7z").copied().or_else(|| m.values().find(|_| true).copied()),
        Some(unix_to_filetime(MTIME_NEST)).filter(|_| m.values().any(|v| *v == unix_to_filetime(MTIME_NEST))),
    );
    assert!(
        m.values().any(|v| *v == unix_to_filetime(MTIME_NEST)),
        "nest.7z mtime not preserved: {m:?}"
    );
    assert!(
        m.values().any(|v| *v == unix_to_filetime(MTIME_README)),
        "readme.txt mtime not preserved: {m:?}"
    );
}

/// Nested outer: passthrough + solid nest with known outer-layer mtimes.
fn make_nested_outer_with_mtimes(root: &Path) -> PathBuf {
    ensure_7z();
    let inners = root.join("inners");
    fs::create_dir_all(&inners).unwrap();
    let solid = make_solid_with_mtimes(&inners);
    // Rename for clearer outer member name.
    let nest_src = inners.join("nest.7z");
    fs::rename(&solid, &nest_src).unwrap();
    set_mtime(&nest_src, MTIME_NEST);

    let stage = root.join("outer-stage");
    fs::create_dir_all(&stage).unwrap();
    fs::copy(&nest_src, stage.join("nest.7z")).unwrap();
    set_mtime(&stage.join("nest.7z"), MTIME_NEST);
    fs::write(stage.join("readme.txt"), b"outer readme\n").unwrap();
    set_mtime(&stage.join("readme.txt"), MTIME_README);

    let outer = root.join("outer.7z");
    pack_nonsolid(&stage, &outer);
    outer
}

fn nested_pipeline_opts(input: PathBuf, output: PathBuf) -> PipelineOptions {
    let mut opts = PipelineOptions::new(input, output);
    opts.pack = pack_opts();
    opts.verify = true;
    opts.solid_single_pass = true;
    opts.passthrough_nonsolid = true;
    opts.prefer_streaming = true;
    opts.nested_concurrency = 1;
    opts.outer_format = archiveconverter::codec::OuterFormat::SevenZ;
    opts
}

#[test]
fn nested_outer_store_preserves_member_mtime_native() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let outer = make_nested_outer_with_mtimes(root.path());
    let out = root.path().join("out-native.7z");

    let backend = open_backend(BackendKind::Native).unwrap();
    let mut opts = nested_pipeline_opts(outer.clone(), out.clone());
    opts.prefer_streaming = true;
    opts.temp_dir = Some(root.path().join("tmp"));
    run(backend.as_ref(), &opts).unwrap();

    // Outer-layer members.
    assert_mtimes_match(&outer, &out, &["readme.txt", "nest.7z"]);

    // Inner files inside converted nest must keep solid source times.
    let b = NativeSevenZ::new();
    let nest_out = root.path().join("nest-out.7z");
    b.extract_member(&out, "nest.7z", &nest_out).unwrap();
    let nest_src = root.path().join("inners/nest.7z");
    assert_mtimes_match(&nest_src, &nest_out, &["old_a.txt", "old_b.txt"]);
}

#[test]
fn nested_outer_store_preserves_member_mtime_cli() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let outer = make_nested_outer_with_mtimes(root.path());
    let out = root.path().join("out-cli.7z");

    let backend = open_backend(BackendKind::Cli).unwrap();
    let mut opts = nested_pipeline_opts(outer.clone(), out.clone());
    opts.prefer_streaming = false;
    opts.temp_dir = Some(root.path().join("tmp"));
    run(backend.as_ref(), &opts).unwrap();

    assert_mtimes_match(&outer, &out, &["readme.txt", "nest.7z"]);
    assert_attrs_preserved(&outer, &out, &["readme.txt", "nest.7z"]);

    // Inner via CLI convert-single path should also preserve.
    let b = backend.as_ref();
    let nest_out = root.path().join("nest-out-cli.7z");
    b.extract_member(&out, "nest.7z", &nest_out).unwrap();
    let nest_src = root.path().join("inners/nest.7z");
    assert_mtimes_match(&nest_src, &nest_out, &["old_a.txt", "old_b.txt"]);
}

#[test]
fn convert_single_api_native_preserves_mtime() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let solid = make_solid_with_mtimes(root.path());
    let out = root.path().join("cs-native.7z");

    let backend = open_backend(BackendKind::Native).unwrap();
    convert_single(
        backend.as_ref(),
        &solid,
        &out,
        &MemberFilter::new(),
        &pack_opts(),
        true,
        Some(&root.path().join("tmp")),
        false,
    )
    .unwrap();

    assert_mtimes_match(&solid, &out, &["old_a.txt", "old_b.txt"]);
}
