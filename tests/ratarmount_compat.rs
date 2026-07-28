//! Open our custom-header 7z archives with ratarmount-rs (if available).
//!
//! Requires sibling checkout: `../ratarmount-rs` (not used on CI unless present).
//! Run:
//! ```text
//! cargo test --test ratarmount_compat -- --nocapture
//! ```

use archiveconverter::codec::{
    open_codec, write_nonsolid_lzma2, CodecKind, NonsolidStoreWriter, PackedEntry,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn ratarmount_rs_root() -> Option<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/home/mbrewer/projects/ratarmount-rs"),
        PathBuf::from("../ratarmount-rs"),
    ];
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        candidates.insert(0, PathBuf::from(manifest).join("../ratarmount-rs"));
    }
    candidates.into_iter().find_map(|p| {
        let p = p.canonicalize().ok()?;
        if p.join("ratarmount-formats-sevenzip").is_dir() && p.join("Cargo.toml").is_file() {
            Some(p)
        } else {
            None
        }
    })
}

fn ensure_open_helper(rs_root: &Path) -> PathBuf {
    let helper_dir = std::env::temp_dir().join("archiveconverter-ratarmount-open");
    let src_dir = helper_dir.join("src");
    let _ = fs::create_dir_all(&src_dir);
    let cargo = helper_dir.join("Cargo.toml");
    let sevenzip = rs_root
        .join("ratarmount-formats-sevenzip")
        .canonicalize()
        .expect("sevenzip crate path");
    let core = rs_root
        .join("ratarmount-core")
        .canonicalize()
        .expect("core crate path");
    fs::write(
        &cargo,
        format!(
            r#"[package]
name = "ac_ratar_open"
version = "0.1.0"
edition = "2021"
[dependencies]
ratarmount-formats-sevenzip = {{ path = "{}" }}
ratarmount-core = {{ path = "{}" }}
tempfile = "3"
"#,
            sevenzip.display(),
            core.display()
        ),
    )
    .unwrap();
    fs::write(
        src_dir.join("main.rs"),
        r#"
use std::io::Read;
use ratarmount_core::{MountSource, OpenOptions};
use ratarmount_formats_sevenzip::SevenZipMountSource;

fn main() {
    let path = std::env::args().nth(1).expect("path");
    let dir = tempfile::tempdir().unwrap();
    let idx = dir.path().join("i.sqlite");
    let m = SevenZipMountSource::open(&path, Some(&idx), &OpenOptions::default(), "0.1.0", true)
        .unwrap_or_else(|e| {
            eprintln!("OPEN_FAIL {e}");
            std::process::exit(2);
        });
    let mut n = 0u32;
    let mut bytes = 0u64;
    if let Some(ratarmount_core::ListResult::Infos(infos)) = m.list("/") {
        for (_name, fi) in infos {
            if fi.size == 0 { continue; }
            let mut r = m.open(&fi, 0).unwrap_or_else(|e| {
                eprintln!("MEMBER_OPEN_FAIL {e}");
                std::process::exit(3);
            });
            let mut buf = Vec::new();
            r.read_to_end(&mut buf).unwrap();
            if buf.len() as u64 != fi.size {
                eprintln!("SIZE_MISMATCH {} vs {}", buf.len(), fi.size);
                std::process::exit(4);
            }
            n += 1;
            bytes += buf.len() as u64;
        }
    }
    println!("OK members_read={n} bytes={bytes}");
}
"#,
    )
    .unwrap();

    let status = Command::new("cargo")
        .args(["build", "--release", "-q"])
        .current_dir(&helper_dir)
        .status()
        .expect("cargo");
    assert!(status.success(), "failed to build ratarmount open helper");
    helper_dir.join("target/release/ac_ratar_open")
}

fn assert_ratarmount_opens(archive: &Path) {
    let Some(rs) = ratarmount_rs_root() else {
        eprintln!("skip ratarmount-rs compat (no ../ratarmount-rs checkout)");
        return;
    };
    let helper = ensure_open_helper(&rs);
    let out = Command::new(&helper)
        .arg(archive)
        .output()
        .expect("run helper");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "ratarmount-rs failed on {}: status={:?}\nstdout={stdout}\nstderr={stderr}",
        archive.display(),
        out.status
    );
    assert!(
        stdout.contains("OK members_read="),
        "unexpected helper output: {stdout}"
    );
}

#[test]
fn ratarmount_rs_opens_store_writer_archive() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.bin");
    let b = dir.path().join("b.txt");
    fs::write(&a, b"payload-aaa").unwrap();
    fs::write(&b, b"payload-bbb").unwrap();
    let out = dir.path().join("store.7z");
    let mut w = NonsolidStoreWriter::create(&out).unwrap();
    w.push_path("nested/a.7z".into(), &a).unwrap();
    w.push_path("readme.txt".into(), &b).unwrap();
    w.push_bytes("empty.dat".into(), b"").unwrap();
    w.finish().unwrap();
    assert_ratarmount_opens(&out);
}

#[test]
fn ratarmount_rs_opens_lzma2_writer_archive() {
    let dir = tempfile::tempdir().unwrap();
    let codec = open_codec(CodecKind::LibLzma);
    let out = dir.path().join("lzma2.7z");
    let mut entries = Vec::new();
    for i in 0..10 {
        let data = format!("line {i} {}", "z".repeat(40));
        let c = codec.compress(data.as_bytes(), 1).unwrap();
        entries.push(PackedEntry {
            name: format!("d{i}/f.txt"),
            compressed: c,
        });
    }
    write_nonsolid_lzma2(&out, &entries).unwrap();
    assert_ratarmount_opens(&out);
}
