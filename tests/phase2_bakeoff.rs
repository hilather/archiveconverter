//! Phase 2 bake-off: CLI vs native sequential vs native decode-ahead pipeline.

mod common;

use archiveconverter::archive::native::{NativeOptions, NativePipeline, NativeSevenZ};
use archiveconverter::archive::sevenz::SevenZCli;
use archiveconverter::archive::{ArchiveBackend, PackOptions};
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
            &format!("id={i}\n{}", "compressible line data for bakeoff\n".repeat(8)),
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

fn time_native(
    solid: &std::path::Path,
    out: &std::path::Path,
    pipeline: NativePipeline,
    encode_threads: Option<u32>,
) -> f64 {
    let mut opts = NativeOptions::default();
    opts.pipeline = pipeline;
    opts.encode_threads = encode_threads;
    opts.decode_threads = 1;
    let native = NativeSevenZ::with_options(opts);
    let filter = MemberFilter::with_excludes([r"(?i)\.tmp$"]).unwrap();
    let pack = PackOptions {
        non_solid: true,
        threads: encode_threads.or(Some(1)),
        level: 1,
    };
    let t0 = Instant::now();
    native
        .stream_convert_to_nonsolid(solid, out, &filter, &pack)
        .unwrap();
    t0.elapsed().as_secs_f64()
}

#[test]
fn bakeoff_cli_vs_native_pipelines() {
    ensure_7z();
    let root = tempfile::tempdir().unwrap();
    let files = 8_000u32;
    let solid = make_solid_fixture(root.path(), files);

    let cli_out = root.path().join("cli.7z");
    let seq_out = root.path().join("seq.7z");
    let pipe_out = root.path().join("pipe.7z");

    let cli_s = time_cli(&solid, &cli_out, &root.path().join("tmp-cli"));
    let seq_s = time_native(
        &solid,
        &seq_out,
        NativePipeline::Sequential,
        Some(1),
    );
    let pipe_s = time_native(
        &solid,
        &pipe_out,
        NativePipeline::DecodeAhead { depth: 2 },
        Some(1),
    );
    // Pipeline + multi-thread encode (helps more when members are larger)
    let pipe_mt_s = time_native(
        &solid,
        &root.path().join("pipe-mt.7z"),
        NativePipeline::DecodeAhead { depth: 2 },
        Some(4),
    );

    let cli = SevenZCli::discover().unwrap();
    let n_cli = list_file_paths(&cli, &cli_out).unwrap().len();
    let n_seq = list_file_paths(&NativeSevenZ::new(), &seq_out)
        .unwrap()
        .len();
    let n_pipe = list_file_paths(&NativeSevenZ::new(), &pipe_out)
        .unwrap()
        .len();
    assert_eq!(n_cli, n_seq);
    assert_eq!(n_cli, n_pipe);
    assert!(!cli.is_solid(&cli_out).unwrap());
    assert!(!NativeSevenZ::new().is_solid(&seq_out).unwrap());

    println!("\n=== Phase 2 bake-off ({files} files, solid→non-solid, exclude .tmp) ===");
    println!(
        "{:<28} {:>8}  {:>10}",
        "engine", "seconds", "vs CLI"
    );
    println!(
        "{:<28} {:>8.3}  {:>9}",
        "CLI 7zz extract+pack",
        cli_s,
        "1.00x"
    );
    println!(
        "{:<28} {:>8.3}  {:>9.2}x",
        "native sequential",
        seq_s,
        seq_s / cli_s.max(1e-9)
    );
    println!(
        "{:<28} {:>8.3}  {:>9.2}x",
        "native decode-ahead(2)",
        pipe_s,
        pipe_s / cli_s.max(1e-9)
    );
    println!(
        "{:<28} {:>8.3}  {:>9.2}x",
        "native pipeline+MT(4)",
        pipe_mt_s,
        pipe_mt_s / cli_s.max(1e-9)
    );
    println!("===============================================================\n");
}
