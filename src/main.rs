use archiveconverter::archive::sevenz::SevenZCli;
use archiveconverter::archive::PackOptions;
use archiveconverter::cli::{Cli, Commands};
use archiveconverter::convert::ConverterRegistry;
use archiveconverter::error::Result;
use archiveconverter::filter::MemberFilter;
use archiveconverter::pipeline::{self, convert_single};
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
        archiveconverter::Error::InvalidRegex { .. } => 2,
        archiveconverter::Error::NameCollision(_) => 2,
        _ => 1,
    }
}

fn try_main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Commands::Backend => {
            let backend = SevenZCli::discover()?;
            println!("{}", backend.version_line()?);
            println!("binary: {}", backend.binary().display());
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
            let backend = SevenZCli::discover()?;
            let opts = args.to_pipeline_options()?;
            let plan = pipeline::run(&backend, &opts)?;
            if !opts.dry_run {
                println!(
                    "Wrote {} (nested={}, passthrough={}, skipped={})",
                    opts.output.display(),
                    plan.nested_count(),
                    plan.passthrough_count(),
                    plan.skip_count()
                );
            }
            Ok(())
        }
        Commands::ConvertSingle(args) => {
            let backend = SevenZCli::discover()?;
            let exclude = MemberFilter::with_excludes(&args.exclude)?;
            let pack = PackOptions {
                non_solid: true,
                threads: args.threads,
                level: args.level,
            };
            convert_single(
                &backend,
                &args.input,
                &args.output,
                &exclude,
                &pack,
                args.verify,
                args.temp_dir.as_deref(),
                args.keep_temp,
            )?;
            println!("Wrote {}", args.output.display());
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
