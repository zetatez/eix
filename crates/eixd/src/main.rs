use std::path::PathBuf;

use anyhow::{bail, Result};
use eix_core::{config, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse()?;
    if args.help {
        print_help();
        return Ok(());
    }
    if args.version {
        println!("eixd {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let config = Config::load(args.config.as_deref())?;
    let socket = args.socket.unwrap_or_else(config::socket_path);
    let data_dir = args.data_dir.unwrap_or_else(config::data_dir);
    eixd::run(config, socket, data_dir).await
}

#[derive(Default)]
struct Args {
    socket: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    config: Option<PathBuf>,
    help: bool,
    version: bool,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = Args::default();
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--socket" => args.socket = Some(next_path(&mut it, "--socket")?),
                "--data-dir" => args.data_dir = Some(next_path(&mut it, "--data-dir")?),
                "--config" => args.config = Some(next_path(&mut it, "--config")?),
                "-h" | "--help" => args.help = true,
                "-V" | "--version" => args.version = true,
                other => bail!("unknown argument {other:?} (try --help)"),
            }
        }
        Ok(args)
    }
}

fn next_path(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<PathBuf> {
    let value = it
        .next()
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a path"))?;
    Ok(PathBuf::from(value))
}

fn print_help() {
    println!(
        "\
eixd — eix daemon

USAGE:
    eixd [OPTIONS]

OPTIONS:
    --socket <path>     Unix socket path (default: $XDG_RUNTIME_DIR/eix.sock;
                        macOS: ~/Library/Caches/eix/eix.sock)
    --data-dir <path>   state directory (default: ~/.local/share/eix)
    --config <path>     config file (default: ~/.config/eix/config.yaml)
    -h, --help          show this help
    -V, --version       show version

ENVIRONMENT:
    EIX_BASE_URL, EIX_MODEL, EIX_API_KEY, EIX_SOCKET, EIX_DATA_DIR,
    EIX_CONFIG, EIX_CONFIG_DIR, EIX_TCP, EIX_TCP_TOKEN, EIX_HTTP

The daemon runs in the foreground. `eix` starts it automatically."
    );
}
