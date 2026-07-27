//! End-to-end: nested solid 7z conversion, excludes, renames, dry-run.

mod common;

use archiveconverter::archive::ArchiveBackend;
use archiveconverter::filter::{MemberFilter, NameTransformer};
use archiveconverter::pipeline::{self, list_file_paths, PipelineOptions};
use common::*;
use std::fs;

#[test]
fn nested_convert_one_at_a_time_with_filters_and_rename() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let outer = make_nested_outer(root.path());
    let out = root.path().join("converted.7z");

    let mut opts = PipelineOptions::new(outer.clone(), out.clone());
    opts.exclude_outer = MemberFilter::with_excludes([r"^skip_me\.7z$"]).unwrap();
    opts.exclude_inner = MemberFilter::with_excludes([r"(?i)\.tmp$", r"^__MACOSX/"]).unwrap();
    opts.rename = NameTransformer::from_pairs([r"_old\.7z$=.7z"]).unwrap();
    opts.verify = true;
    opts.pack = default_pack();
    opts.temp_dir = Some(root.path().join("tmp"));

    let backend = backend();
    let plan = pipeline::run(&backend, &opts).expect("convert");

    assert_eq!(plan.nested_count(), 2); // alpha + beta
    assert_eq!(plan.passthrough_count(), 1); // readme
    assert_eq!(plan.skip_count(), 1); // skip_me

    assert_archive_ok(&out);

    let outer_paths = list_file_paths(&backend, &out).unwrap();
    assert!(outer_paths.iter().any(|p| p == "alpha.7z"), "{outer_paths:?}");
    assert!(outer_paths.iter().any(|p| p == "beta.7z"), "{outer_paths:?}");
    assert!(outer_paths.iter().any(|p| p == "readme.txt"), "{outer_paths:?}");
    assert!(
        !outer_paths.iter().any(|p| p.contains("skip_me")),
        "{outer_paths:?}"
    );
    assert!(
        !outer_paths.iter().any(|p| p.contains("alpha_old")),
        "rename should strip _old: {outer_paths:?}"
    );

    // Inspect converted alpha for inner excludes.
    let alpha_out = root.path().join("alpha-extracted.7z");
    backend
        .extract_member(&out, "alpha.7z", &alpha_out)
        .unwrap();
    let inner_paths = list_file_paths(&backend, &alpha_out).unwrap();
    assert!(
        inner_paths.iter().any(|p| p.ends_with("hello.txt")),
        "{inner_paths:?}"
    );
    assert!(
        !inner_paths.iter().any(|p| p.ends_with(".tmp")),
        "tmp should be excluded: {inner_paths:?}"
    );
    assert!(
        !inner_paths.iter().any(|p| p.contains("__MACOSX")),
        "macosx should be excluded: {inner_paths:?}"
    );

    // Content preserved
    let tree = root.path().join("alpha-tree");
    backend.extract_all(&alpha_out, &tree).unwrap();
    let hello = fs::read_to_string(tree.join("data/hello.txt")).unwrap();
    assert!(hello.contains("hello from alpha_old.7z") || hello.contains("hello from"));
}

#[test]
fn dry_run_does_not_write_output() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let outer = make_nested_outer(root.path());
    let out = root.path().join("should-not-exist.7z");

    let mut opts = PipelineOptions::new(outer, out.clone());
    opts.dry_run = true;
    opts.pack = default_pack();

    let plan = pipeline::run(&backend(), &opts).unwrap();
    assert!(plan.nested_count() >= 2);
    assert!(!out.exists(), "dry-run must not create output");
}

#[test]
fn rename_collision_fails_plan() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let outer = make_nested_outer(root.path());
    let out = root.path().join("out.7z");

    let mut opts = PipelineOptions::new(outer, out);
    // Map everything to the same name
    opts.rename = NameTransformer::from_pairs([r".*=same.7z"]).unwrap();
    opts.pack = default_pack();

    let err = pipeline::run(&backend(), &opts).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("collision") || msg.contains("same.7z"),
        "{msg}"
    );
}
