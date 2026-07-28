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
fn nested_convert_outer_as_directory() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let outer = make_nested_outer(root.path());
    // Default naming: stem of outer path next to it
    let expected_dir = archiveconverter::codec::default_dir_from_input(&outer);
    assert_eq!(
        expected_dir,
        root.path().join(
            outer
                .file_stem()
                .unwrap()
                .to_str()
                .unwrap()
        )
    );

    let mut opts = PipelineOptions::new(outer, expected_dir.clone());
    opts.outer_format = archiveconverter::codec::OuterFormat::Dir;
    opts.exclude_outer = MemberFilter::with_excludes([r"^skip_me\.7z$"]).unwrap();
    opts.exclude_inner = MemberFilter::with_excludes([r"(?i)\.tmp$", r"^__MACOSX/"]).unwrap();
    opts.rename = NameTransformer::from_pairs([r"_old\.7z$=.7z"]).unwrap();
    opts.verify = true;
    opts.pack = default_pack();
    opts.temp_dir = Some(root.path().join("tmp"));

    let plan = pipeline::run(&backend(), &opts).expect("convert to dir");
    assert_eq!(plan.nested_count(), 2);
    assert_eq!(plan.passthrough_count(), 1);
    assert!(expected_dir.is_dir());
    assert!(expected_dir.join("alpha.7z").is_file());
    assert!(expected_dir.join("beta.7z").is_file());
    assert!(expected_dir.join("readme.txt").is_file());
    assert!(!expected_dir.join("skip_me.7z").exists());
    assert_eq!(
        archiveconverter::codec::count_dir_files(&expected_dir).unwrap(),
        3
    );
    assert_archive_ok(&expected_dir.join("alpha.7z"));
}

#[test]
fn nested_convert_outer_as_uncompressed_tar() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let outer = make_nested_outer(root.path());
    let out = root.path().join("converted.tar");

    let mut opts = PipelineOptions::new(outer, out.clone());
    opts.outer_format = archiveconverter::codec::OuterFormat::Tar;
    opts.exclude_outer = MemberFilter::with_excludes([r"^skip_me\.7z$"]).unwrap();
    opts.exclude_inner = MemberFilter::with_excludes([r"(?i)\.tmp$", r"^__MACOSX/"]).unwrap();
    opts.rename = NameTransformer::from_pairs([r"_old\.7z$=.7z"]).unwrap();
    opts.verify = true;
    opts.pack = default_pack();
    opts.temp_dir = Some(root.path().join("tmp"));

    let plan = pipeline::run(&backend(), &opts).expect("convert to tar");
    assert_eq!(plan.nested_count(), 2);
    assert_eq!(plan.passthrough_count(), 1);
    assert!(out.is_file(), "tar output missing");

    // Uncompressed tar of nested .7z + readme
    assert_eq!(
        archiveconverter::codec::count_tar_files(&out).unwrap(),
        3,
        "alpha.7z + beta.7z + readme.txt"
    );

    // Extract tar members with system tar and check nested 7z is listable.
    let extract = root.path().join("tar-out");
    fs::create_dir_all(&extract).unwrap();
    let status = std::process::Command::new("tar")
        .args(["-xf", out.to_str().unwrap(), "-C", extract.to_str().unwrap()])
        .status()
        .expect("spawn tar");
    assert!(status.success(), "system tar extract failed");
    assert!(extract.join("alpha.7z").is_file() || extract.join("alpha.7z").exists());
    // rename alpha_old → alpha
    let alpha = if extract.join("alpha.7z").is_file() {
        extract.join("alpha.7z")
    } else {
        // walk
        walkdir::WalkDir::new(&extract)
            .into_iter()
            .filter_map(|e| e.ok())
            .find(|e| e.file_name() == "alpha.7z")
            .map(|e| e.path().to_path_buf())
            .expect("alpha.7z in tar")
    };
    assert_archive_ok(&alpha);
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

/// Corrupt nested .7z is skipped; good members still appear in the output.
#[test]
fn corrupt_nested_is_skipped_others_succeed() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let stage = root.path().join("stage");
    fs::create_dir_all(&stage).unwrap();

    // Valid nested
    let good = make_inner_solid(root.path(), "good.7z");
    fs::copy(&good, stage.join("good.7z")).unwrap();
    // Corrupt nested (not a real 7z)
    fs::write(stage.join("corrupt.7z"), b"this is not a valid 7z archive!!!!").unwrap();
    // Passthrough text
    write_file(&stage, "readme.txt", "hello\n");

    let outer = root.path().join("outer.7z");
    pack_solid(&stage, &outer);

    let out = root.path().join("converted.7z");
    let mut opts = PipelineOptions::new(outer, out.clone());
    opts.pack = default_pack();
    opts.verify = true;
    opts.nested_concurrency = 1; // serial path
    opts.temp_dir = Some(root.path().join("tmp"));

    // Must succeed overall despite corrupt nest.
    let plan = pipeline::run(&backend(), &opts).expect("convert should succeed with skip");
    assert_eq!(plan.nested_count(), 2, "plan still lists both nests");

    let paths = list_file_paths(&backend(), &out).unwrap();
    assert!(
        paths.iter().any(|p| p.ends_with("good.7z")),
        "good nested should be present: {paths:?}"
    );
    assert!(
        paths.iter().any(|p| p == "readme.txt"),
        "passthrough should be present: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.contains("corrupt")),
        "corrupt nested must not be in output: {paths:?}"
    );
}
