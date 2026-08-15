//! CLI smoke tests via assert_cmd.

mod common;

use assert_cmd::Command;
use common::*;
use predicates::prelude::*;
use std::fs;

fn bin() -> Command {
    Command::cargo_bin("archiveconverter").unwrap()
}

#[test]
fn backend_command_prints_version() {
    ensure_7z();
    bin()
        .arg("backend")
        .assert()
        .success()
        .stdout(predicate::str::contains("7-Zip").or(predicate::str::contains("7z")));
}

#[test]
fn list_converters() {
    bin()
        .arg("list-converters")
        .assert()
        .success()
        .stdout(predicate::str::contains("7z-solid-to-nonsolid"))
        .stdout(predicate::str::contains("zip-stub"));
}

#[test]
fn convert_dry_run_cli() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let outer = make_nested_outer(root.path());
    let out = root.path().join("out.7z");

    bin()
        .args([
            "convert",
            outer.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--dry-run",
            "--exclude-outer",
            r"^skip_me\.7z$",
            "--rename",
            r"_old\.7z$=.7z",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("CONVERT"))
        .stdout(predicate::str::contains("alpha"));

    assert!(!out.exists());
}

#[test]
fn convert_full_cli() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let outer = make_nested_outer(root.path());
    let out = root.path().join("out.7z");

    bin()
        .args([
            "convert",
            outer.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--exclude-outer",
            r"^skip_me\.7z$",
            "--exclude-inner",
            r"(?i)\.tmp$",
            "--exclude-inner",
            r"^__MACOSX/",
            "--rename",
            r"_old\.7z$=.7z",
            "--verify",
            "--level",
            "1",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Wrote"))
        .stdout(predicate::str::contains("outer=7z"));

    assert!(out.is_file());
    assert!(fs::metadata(&out).unwrap().len() > 0);
    assert_archive_ok(&out);
}

#[test]
fn convert_outer_tar_cli() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let outer = make_nested_outer(root.path());
    let out = root.path().join("out.tar");

    bin()
        .args([
            "convert",
            outer.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--outer-format",
            "tar",
            "--exclude-outer",
            r"^skip_me\.7z$",
            "--rename",
            r"_old\.7z$=.7z",
            "--verify",
            "--level",
            "1",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Wrote"))
        .stdout(predicate::str::contains("outer=tar"));

    assert!(out.is_file());
    assert!(fs::metadata(&out).unwrap().len() > 0);
    assert_eq!(
        archiveconverter::codec::count_tar_files(&out).unwrap(),
        3 // alpha.7z, beta.7z, readme
    );
}

#[test]
fn convert_outer_dir_default_name_cli() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let outer = make_nested_outer(root.path());
    // No -o: default dir = stem of outer next to it
    let expected = archiveconverter::codec::default_dir_from_input(&outer);

    bin()
        .args([
            "convert",
            outer.to_str().unwrap(),
            "--outer-format",
            "dir",
            "--exclude-outer",
            r"^skip_me\.7z$",
            "--rename",
            r"_old\.7z$=.7z",
            "--verify",
            "--level",
            "1",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Wrote"))
        .stdout(predicate::str::contains("outer=dir"));

    assert!(expected.is_dir(), "expected default dir {}", expected.display());
    assert!(expected.join("alpha.7z").is_file());
    assert!(expected.join("beta.7z").is_file());
    assert!(expected.join("readme.txt").is_file());
}

#[test]
fn convert_single_cli() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let solid = make_inner_solid(root.path(), "s.7z");
    let out = root.path().join("o.7z");

    bin()
        .args([
            "convert-single",
            solid.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--exclude",
            r"\.tmp$",
            "--verify",
            "--level",
            "1",
        ])
        .assert()
        .success();

    assert!(out.is_file());
}

#[test]
fn convert_rsync_filter_from_cli() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let outer = make_nested_outer(root.path());
    let out = root.path().join("out.7z");
    let rules = root.path().join("outer.rules");
    fs::write(&rules, "skip_me.7z\n").unwrap();

    bin()
        .args([
            "convert",
            outer.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--exclude-from-outer",
            rules.to_str().unwrap(),
            "--filter-inner=exclude *.tmp",
            "--filter-inner=exclude __MACOSX/",
            "--rename",
            r"_old\.7z$=.7z",
            "--verify",
            "--level",
            "1",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Wrote"));

    assert!(out.is_file());
    assert_archive_ok(&out);
}
