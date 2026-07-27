//! Phase 3 bake-off: CLI vs native ParallelCodec (pure-rust / liblzma).

mod common;

use archiveconverter::archive::native::{NativeOptions, NativePipeline, NativeSevenZ};
use archiveconverter::archive::sevenz::SevenZCli;
use archiveconverter::archive::{ArchiveBackend, PackOptions};
use archiveconverter::codec::CodecKind;
use archiveconverter::filter::MemberFilter;
use archiveconverter::pipeline::{convert_single_ex, list_file_paths};
use common::*;
use std::time::Instant;

fn make_solid_fixture(root: &std::path::Path, files: u32) -> std::path::PathBuf {
    let tree = root.join("tree");
    for i in 0..files {
        write_file(
            &tree,
            &format!("d{}/f{i:05}.txt", i % 50),
            &format!("id={i}\n{}", "compressible line data for phase3\n".repeat(8)),
        );
    }
    write_file(&tree, "drop.tmp", "tmp");
    let solid = root.join("solid.7z");
    pack_solid(&tree, &solid);
    let _ = std::fs::remove_dir_all(&tree);
    solid
}

fn time_cli(solid: &std::path::Path, out: &std::path::Path, tmp: &std::path::Path) -> f64 {
    let cli = SevenZCli::discover().unwrap();
    let exclude = MemberFilter::with_excludes([r"(?i)\.tmp$"]).unwrap();
    let pack = PackOptions {
        non_solid: true,
        threads: Some(1),
        level: 1,
    };
    let t0 = Instant::now();
    convert_single_ex(
        &cli,
        solid,
        out,
        &exclude,
        &pack,
        false,
        Some(tmp),
        false,
        true,
    )
    .unwrap();
    t0.elapsed().as_secs_f64()
}

fn time_parallel(
    solid: &std::path::Path,
    out: &std::path::Path,
    codec: CodecKind,
    encode_threads: Option<u32>,
) -> f64 {
    let opts = NativeOptions {
        pipeline: NativePipeline::ParallelCodec,
        codec,
        encode_threads,
        decode_threads: 1,
        ..NativeOptions::default()
    };
    let native = NativeSevenZ::with_options(opts);
    let filter = MemberFilter::with_excludes([r"(?i)\.tmp$"]).unwrap();
    let pack = PackOptions {
        non_solid: true,
        threads: encode_threads.or(Some(4)),
        level: 1,
    };
    let t0 = Instant::now();
    native
        .stream_convert_to_nonsolid(solid, out, &filter, &pack)
        .unwrap();
    t0.elapsed().as_secs_f64()
}

fn time_decode_ahead(solid: &std::path::Path, out: &std::path::Path) -> f64 {
    let opts = NativeOptions {
        pipeline: NativePipeline::DecodeAhead { depth: 2 },
        encode_threads: Some(4),
        decode_threads: 1,
        ..NativeOptions::default()
    };
    let native = NativeSevenZ::with_options(opts);
    let filter = MemberFilter::with_excludes([r"(?i)\.tmp$"]).unwrap();
    let pack = PackOptions {
        non_solid: true,
        threads: Some(4),
        level: 1,
    };
    let t0 = Instant::now();
    native
        .stream_convert_to_nonsolid(solid, out, &filter, &pack)
        .unwrap();
    t0.elapsed().as_secs_f64()
}

#[test]
fn bakeoff_phase3_codecs_vs_cli() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let files = 8_000u32;
    let solid = make_solid_fixture(root.path(), files);

    let cli_out = root.path().join("cli.7z");
    let pure_out = root.path().join("pure.7z");
    let lib_out = root.path().join("liblzma.7z");
    let ahead_out = root.path().join("ahead.7z");

    let cli_s = time_cli(&solid, &cli_out, &root.path().join("tmp-cli"));
    let pure_s = time_parallel(&solid, &pure_out, CodecKind::PureRust, Some(4));
    let lib_s = time_parallel(&solid, &lib_out, CodecKind::LibLzma, Some(4));
    let ahead_s = time_decode_ahead(&solid, &ahead_out);

    let cli = SevenZCli::discover().unwrap();
    let n_cli = list_file_paths(&cli, &cli_out).unwrap().len();
    let native = NativeSevenZ::new();
    let n_pure = list_file_paths(&native, &pure_out).unwrap().len();
    let n_lib = list_file_paths(&native, &lib_out).unwrap().len();
    assert_eq!(n_cli, n_pure, "pure-rust file count");
    assert_eq!(n_cli, n_lib, "liblzma file count");
    assert!(!cli.is_solid(&cli_out).unwrap());
    assert!(!native.is_solid(&pure_out).unwrap());
    assert!(!native.is_solid(&lib_out).unwrap());

    // Integrity: full stream test on Phase 3 outputs
    native.test(&pure_out).unwrap();
    native.test(&lib_out).unwrap();
    // Official 7zz should accept our custom packer
    cli.test(&pure_out).unwrap();
    cli.test(&lib_out).unwrap();

    println!("\n=== Phase 3 bake-off ({files} files, solid→non-solid, exclude .tmp) ===");
    println!(
        "{:<36} {:>8}  {:>10}",
        "engine", "seconds", "vs CLI"
    );
    println!(
        "{:<36} {:>8.3}  {:>9}",
        "CLI 7zz extract+pack",
        cli_s,
        "1.00x"
    );
    println!(
        "{:<36} {:>8.3}  {:>9.2}x",
        "native decode-ahead + MT(4)",
        ahead_s,
        ahead_s / cli_s.max(1e-9)
    );
    println!(
        "{:<36} {:>8.3}  {:>9.2}x",
        "parallel-codec pure-rust (4)",
        pure_s,
        pure_s / cli_s.max(1e-9)
    );
    println!(
        "{:<36} {:>8.3}  {:>9.2}x",
        "parallel-codec liblzma (4)",
        lib_s,
        lib_s / cli_s.max(1e-9)
    );
    println!("================================================================\n");
}
