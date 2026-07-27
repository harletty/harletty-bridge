// Carried over from upstream. Four helpers are unreachable today
// (CAFWriter::{close_and_drop,flush}, create_caf_writer_from_existing_file,
// describe_codec, Input::read_all); they are kept rather than deleted because
// they are the extension points the DTS path plugs into, and because keeping
// this tree diffable against reference-sources/truehdd is worth more than the
// four lints.
#![allow(dead_code)]

use anyhow::Result;
use clap::Parser as ClapParser;
use cli::command::{Cli, Commands, LogFormat};
use cli::decode::cmd_decode;
use cli::info::cmd_info;
use indicatif::MultiProgress;
use indicatif_log_bridge::LogWrapper;
use log::info;

// The DAMF/CAF/WAV writers these used to sit next to now live in the `damf`
// crate; what stays here is CLI-shaped: argument parsing, input probing, the
// decode pipeline, and the codec->OAMD mapping that feeds the writers.
mod cli;
mod codec_probe;
mod eac3_to_oamd;
mod input;
pub(crate) mod timestamp;

/// Identifies this binary in the `creationTool` field of every master set it
/// writes. Must stay "truehdd": Atmos Ranker and the reference masters in
/// `adm/` key off it.
pub(crate) const CREATION_TOOL: damf::CreationTool = damf::CreationTool {
    name: env!("CARGO_PKG_NAME"),
    version: env!("CARGO_PKG_VERSION"),
};

fn main() -> Result<()> {
    let cli = Cli::parse();

    let base_level = cli.loglevel.to_level_filter();

    let multi = MultiProgress::new();

    let mut env_builder = env_logger::Builder::from_default_env();
    env_builder.filter_level(base_level);
    match cli.log_format {
        LogFormat::Plain => {
            env_builder.format_timestamp_secs();
        }
        LogFormat::Json => {
            env_builder.format(|buf, record| {
                use std::io::Write;
                writeln!(
                    buf,
                    "{{\"ts\":{},\"lvl\":\"{}\",\"msg\":\"{}\"}}",
                    buf.timestamp(),
                    record.level(),
                    record.args()
                )
            });
        }
    }

    let pb = if cli.progress {
        let logger = env_builder.build();
        LogWrapper::new(multi.clone(), logger).try_init()?;
        Some(&multi)
    } else {
        env_builder.try_init()?;
        None
    };

    info!("{}", cli::command::VERSION_INFO);

    match cli.command {
        Commands::Decode(ref args) => cmd_decode(args, &cli, pb)?,
        Commands::Info(ref args) => cmd_info(args, &cli, pb)?,
    }

    Ok(())
}
