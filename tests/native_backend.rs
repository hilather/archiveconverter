//! Native (sevenz-rust2) backend correctness + speed vs CLI.

mod common;

use archiveconverter::archive::native::NativeSevenZ;
use archiveconverter::archive::sevenz::SevenZCli;
use archiveconverter::archive::{ArchiveBackend, PackOptions};
use archiveconverter::filter::MemberFilter;
use archiveconverter::pipeline::{convert_single_ex, list_file_paths};
use common::*;
use std::collections::BTreeSet;
use std::fs;
use std::time::Instant;

#[test]
fn native_matches_cli_convert_single() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let solid = make_inner_solid(root.path(), "solid.7z");

    let exclude = MemberFilter::with_excludes([r"(?i)\.tmp$", r"^__MACOSX/"]).unwrap();
    let pack = PackOptions {
        non_solid: true,
        threads: Some(1),
        level: 1,
    };

    let cli = SevenZCli::discover().unwrap();
    let native = NativeSevenZ::new();

    let cli_out = root.path().join("cli.7z");
    let native_out = root.path().join("native.7z");

    let t0 = Instant::now();
    convert_single_ex(
        &cli,
        &solid,
        &cli_out,
        &exclude,
        &pack,
        false,
        Some(&root.path().join("tmp-cli")),
        false,
        true,
    )
    .unwrap();
    let cli_secs = t0.elapsed().as_secs_f64();

    let t1 = Instant::now();
    convert_single_ex(
        &native,
        &solid,
        &native_out,
        &exclude,
        &pack,
        false,
        Some(&root.path().join("tmp-native")),
        false,
        true,
    )
    .unwrap();
    let native_secs = t1.elapsed().as_secs_f64();

    assert!(!native.is_solid(&native_out).unwrap());
    assert!(!cli.is_solid(&cli_out).unwrap());

    let cli_names: BTreeSet<_> = list_file_paths(&cli, &cli_out).unwrap().into_iter().collect();
    let nat_names: BTreeSet<_> = list_file_paths(&native, &native_out)
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(cli_names, nat_names, "member sets differ");
    assert!(!cli_names.iter().any(|p| p.ends_with(".tmp")));
    assert!(!cli_names.iter().any(|p| p.contains("__MACOSX")));

    // Content of a known file
    let ct = root.path().join("ct");
    let nt = root.path().join("nt");
    cli.extract_all(&cli_out, &ct).unwrap();
    native.extract_all(&native_out, &nt).unwrap();
    assert_eq!(
        fs::read(ct.join("data/keep.bin")).unwrap(),
        fs::read(nt.join("data/keep.bin")).unwrap()
    );

    println!(
        "\n=== native vs cli convert-single ===\n\
         cli:    {cli_secs:.3}s\n\
         native: {native_secs:.3}s\n\
         ratio native/cli: {:.2}x  (<1 means native faster)\n",
        native_secs / cli_secs.max(1e-9)
    );
}

#[test]
fn native_streaming_no_tree_for_medium_archive() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    // Build a denser solid archive (5k files) via CLI for realism.
    let tree = root.path().join("tree");
    for i in 0..5_000 {
        write_file(
            &tree,
            &format!("d{}/f{i:05}.txt", i % 50),
            &format!("id={i}\n{}", "line\n".repeat(10)),
        );
    }
    write_file(&tree, "drop.tmp", "x");
    let solid = root.path().join("solid.7z");
    pack_solid(&tree, &solid);
    fs::remove_dir_all(&tree).unwrap();

    let native = NativeSevenZ::new();
    let out = root.path().join("out.7z");
    let filter = MemberFilter::with_excludes([r"(?i)\.tmp$"]).unwrap();
    let pack = PackOptions {
        non_solid: true,
        threads: Some(1),
        level: 1,
    };

    let t0 = Instant::now();
    native
        .stream_convert_to_nonsolid(&solid, &out, &filter, &pack)
        .unwrap();
    let secs = t0.elapsed().as_secs_f64();

    assert!(!native.is_solid(&out).unwrap());
    let names = list_file_paths(&native, &out).unwrap();
    assert!(!names.iter().any(|n| n.ends_with(".tmp")));
    assert!(names.len() >= 5_000);

    // CLI path for comparison
    let cli = SevenZCli::discover().unwrap();
    let cli_out = root.path().join("cli.7z");
    let t1 = Instant::now();
    convert_single_ex(
        &cli,
        &solid,
        &cli_out,
        &filter,
        &pack,
        false,
        Some(&root.path().join("tmp")),
        false,
        true,
    )
    .unwrap();
    let cli_secs = t1.elapsed().as_secs_f64();

    println!(
        "\n=== streaming native vs cli extract+pack (5k files) ===\n\
         native streaming: {secs:.3}s\n\
         cli extract+pack: {cli_secs:.3}s\n\
         ratio native/cli: {:.2}x\n",
        secs / cli_secs.max(1e-9)
    );
}
