//! Single-archive solid → non-solid conversion tests.

mod common;

use archiveconverter::archive::ArchiveBackend;
use archiveconverter::filter::MemberFilter;
use archiveconverter::pipeline::convert_single;
use common::*;
use std::fs;

#[test]
fn convert_single_solid_with_excludes() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let solid = make_inner_solid(root.path(), "solid.7z");
    let out = root.path().join("nonsolid.7z");

    let exclude = MemberFilter::with_excludes([r"(?i)\.tmp$", r"^__MACOSX/"]).unwrap();
    convert_single(
        &backend(),
        &solid,
        &out,
        &exclude,
        &default_pack(),
        true,
        Some(&root.path().join("tmp")),
        false,
    )
    .unwrap();

    assert_archive_ok(&out);
    let paths = list_paths(&out);
    assert!(paths.iter().any(|p| p.contains("hello.txt")), "{paths:?}");
    assert!(!paths.iter().any(|p| p.ends_with(".tmp")), "{paths:?}");
    assert!(!paths.iter().any(|p| p.contains("__MACOSX")), "{paths:?}");

    let b = backend();
    let tree = root.path().join("tree");
    b.extract_all(&out, &tree).unwrap();
    assert_eq!(
        fs::read_to_string(tree.join("data/keep.bin")).unwrap(),
        "binary-keep"
    );
}

#[test]
fn pack_and_list_roundtrip() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let tree = root.path().join("t");
    write_file(&tree, "a.txt", "aaa");
    write_file(&tree, "b/c.txt", "ccc");
    let arch = root.path().join("x.7z");
    pack_nonsolid(&tree, &arch);
    let paths = list_paths(&arch);
    assert!(paths.iter().any(|p| p == "a.txt" || p.ends_with("a.txt")));
    assert!(paths.iter().any(|p| p.contains("c.txt")));
}
