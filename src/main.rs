#![forbid(unsafe_code)]
//! CLI entrypoint for `agy-statusline`.

use std::env;
use std::io::{self, Read};

use agy_statusline::config::{default_settings_path, run_config_command};
use agy_statusline::db::SqliteSpeedReader;
use agy_statusline::statusline::process_statusline;

const MAX_STDIN_BYTES: u64 = 1024 * 1024;

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    if let Some(first) = args.next() {
        if first == "config" {
            let mut assume_yes = false;
            for arg in args {
                match arg.as_str() {
                    "--yes" | "-y" => assume_yes = true,
                    "--help" | "-h" => {
                        println!("Usage: agy-statusline config [--yes|-y]");
                        return Ok(());
                    }
                    other => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown option for config: {other}"),
                        ));
                    }
                }
            }
            let settings_path = default_settings_path()?;
            let exe_path = env::current_exe()?;
            let command_path = exe_path.canonicalize().unwrap_or(exe_path);
            let mut stdin = io::stdin().lock();
            let mut stdout = io::stdout().lock();
            return run_config_command(
                assume_yes,
                &settings_path,
                &command_path,
                &mut stdin,
                &mut stdout,
            );
        }
        if first == "--help" || first == "-h" {
            println!("Usage:\n  agy-statusline\n  agy-statusline config [--yes|-y]");
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown argument: {first}"),
        ));
    }

    let mut input = String::new();
    let _ = io::stdin()
        .take(MAX_STDIN_BYTES)
        .read_to_string(&mut input);
    let reader = SqliteSpeedReader::default();
    let mut stdout = io::stdout().lock();
    process_statusline(&input, &reader, &mut stdout)
}
