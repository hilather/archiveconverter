use archiveconverter::archive::native::NativeOptions;
use archiveconverter::archive::{open_backend_with, PackOptions};
use archiveconverter::cli::{Cli, Commands};
use archiveconverter::convert::ConverterRegistry;
use archiveconverter::error::Result;
use archiveconverter::pipeline::{self, convert_single_ex};
use clap::Parser;
use tracing_subscriber::EnvFilter;

fn main() {
    if let Err(e) = try_main() {
        eprintln!("error: {e}");
        std::process::exit(exit_code(&e));
    }
}

fn exit_code(e: &archiveconverter::Error) -> i32 {
    match e {
        archiveconverter::Error::BackendMissing(_) => 2,
        archiveconverter::Error::InvalidRegex { .. }
        | archiveconverter::Error::InvalidFilter { .. }
        | archiveconverter::Error::FilterFileNotFound(_) => 2,
        archiveconverter::Error::NameCollision(_) => 2,
        _ => 1,
    }
}

fn try_main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Commands::Backend => {
            match archiveconverter::archive::sevenz::SevenZCli::discover() {
                Ok(c) => {
                    println!("cli: {}", c.version_line()?);
                    println!("cli binary: {}", c.binary().display());
                }
                Err(e) => println!("cli: unavailable ({e})"),
            }
            let n = NativeOptions::default();
            println!(
                "native: sevenz-rust2 (pipeline={:?}, large_threshold={}, decode_threads={})",
                n.pipeline, n.large_file_threshold, n.decode_threads
            );
            Ok(())
        }
        Commands::ListConverters => {
            let reg = ConverterRegistry::with_builtins();
            for id in reg.list_ids() {
                if let Some(c) = reg.get(id) {
                    println!("{id}: {}", c.description());
                }
            }
            Ok(())
        }
        Commands::Convert(args) => {
            let backend =
                open_backend_with(args.backend_kind(), args.native_options()?)?;
            let opts = args.to_pipeline_options()?;
            tracing::info!(backend = args.backend_kind().as_str(), "using archive backend");
            let plan = pipeline::run(backend.as_ref(), &opts)?;
            if !opts.dry_run {
                let skipped = plan.skip_count() + plan.runtime.runtime_skipped();
                println!(
                    "Wrote {} (outer={}, nested={}, passthrough={}, skipped={}, backend={})",
                    opts.output.display(),
                    opts.outer_format.as_str(),
                    plan.runtime.nested_converted,
                    plan.runtime.passthrough_written,
                    skipped,
                    args.backend_kind().as_str(),
                );
            }
            Ok(())
        }
        Commands::ConvertSingle(args) => {
            let backend =
                open_backend_with(args.backend_kind(), args.native_options()?)?;
            let exclude = args.member_filter()?;
            let pack = PackOptions {
                non_solid: true,
                threads: args.threads,
                level: args.level,
            };
            convert_single_ex(
                backend.as_ref(),
                &args.input,
                &args.output,
                &exclude,
                &pack,
                args.verify,
                args.temp_dir.as_deref(),
                args.keep_temp,
                true,
            )?;
            println!(
                "Wrote {} (backend={})",
                args.output.display(),
                args.backend_kind().as_str()
            );
            Ok(())
        }
    }
}

fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
