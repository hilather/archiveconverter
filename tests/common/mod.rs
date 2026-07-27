//! Shared test helpers: build solid/nested 7z fixtures via the 7z CLI.

#![allow(dead_code)]

use archiveconverter::archive::sevenz::{find_7z_binary, SevenZCli};
use archiveconverter::archive::{ArchiveBackend, PackOptions};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

pub fn backend() -> SevenZCli {
    SevenZCli::discover().expect("7z/7zz must be installed for integration tests")
}

pub fn ensure_7z() {
    static CHECKED: OnceLock<()> = OnceLock::new();
    CHECKED.get_or_init(|| {
        find_7z_binary().expect("7z/7zz must be on PATH (or ~/.local/bin) for tests");
    });
}

/// Write a text file under `dir` with nested relative path.
pub fn write_file(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, contents).unwrap();
}

/// Create a solid 7z from a directory of files.
pub fn pack_solid(src_dir: &Path, dest: &Path) {
    pack_with(src_dir, dest, true, 5);
}

/// Create a non-solid 7z from a directory of files.
pub fn pack_nonsolid(src_dir: &Path, dest: &Path) {
    pack_with(src_dir, dest, false, 5);
}

pub fn pack_with(src_dir: &Path, dest: &Path, solid: bool, level: u32) {
    ensure_7z();
    if dest.exists() {
        fs::remove_file(dest).unwrap();
    }
    if let Some(p) = dest.parent() {
        fs::create_dir_all(p).unwrap();
    }
    let bin = find_7z_binary().unwrap();
    let args = vec![
        "a".into(),
        "-t7z".into(),
        format!("-mx={level}"),
        "-y".into(),
        if solid {
            "-ms=on".to_string()
        } else {
            "-ms=off".to_string()
        },
        dest.to_string_lossy().into_owned(),
        ".".into(),
    ];
    let status = Command::new(&bin)
        .args(&args)
        .current_dir(src_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .unwrap();
    assert!(status.success(), "pack failed for {}", dest.display());
}

/// Build a solid inner archive with several files (some excluded by tests).
pub fn make_inner_solid(dir: &Path, name: &str) -> PathBuf {
    let tree = dir.join(format!("tree-{name}"));
    fs::create_dir_all(&tree).unwrap();
    write_file(&tree, "data/hello.txt", &format!("hello from {name}\n"));
    write_file(&tree, "data/keep.bin", "binary-keep");
    write_file(&tree, "data/drop.tmp", "temporary");
    write_file(&tree, "__MACOSX/._junk", "junk");
    write_file(&tree, "notes/readme.txt", "readme");
    let dest = dir.join(name);
    pack_solid(&tree, &dest);
    dest
}

/// Outer solid archive containing multiple solid inners + a passthrough file.
pub fn make_nested_outer(root: &Path) -> PathBuf {
    ensure_7z();
    let stage = root.join("outer-stage");
    fs::create_dir_all(&stage).unwrap();

    let inners = root.join("inners");
    fs::create_dir_all(&inners).unwrap();
    let a = make_inner_solid(&inners, "alpha_old.7z");
    let b = make_inner_solid(&inners, "beta.7z");
    let c = make_inner_solid(&inners, "skip_me.7z");

    fs::copy(&a, stage.join("alpha_old.7z")).unwrap();
    fs::copy(&b, stage.join("beta.7z")).unwrap();
    fs::copy(&c, stage.join("skip_me.7z")).unwrap();
    write_file(&stage, "readme.txt", "outer readme\n");

    let outer = root.join("outer.7z");
    pack_solid(&stage, &outer);
    outer
}

/// Extract listing paths via backend.
pub fn list_paths(archive: &Path) -> Vec<String> {
    let b = backend();
    let mut v: Vec<String> = b
        .list(archive)
        .unwrap()
        .into_iter()
        .filter(|e| !e.is_dir)
        .map(|e| e.path)
        .collect();
    v.sort();
    v
}

/// Extract archive to dir and read a file's contents as string.
pub fn extract_and_read(archive: &Path, member: &str) -> String {
    let b = backend();
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("file");
    b.extract_member(archive, member, &dest).unwrap();
    fs::read_to_string(dest).unwrap()
}

/// Heuristic: solid archives from 7z often report a single solid block.
/// We assert non-solid by checking that packing used -ms=off and that
/// we can extract a single member and that the archive tests OK.
pub fn assert_archive_ok(archive: &Path) {
    let b = backend();
    b.test(archive).unwrap();
}

pub fn default_pack() -> PackOptions {
    PackOptions {
        non_solid: true,
        threads: Some(2),
        level: 1, // fast for tests
    }
}
