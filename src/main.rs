#![forbid(unsafe_code)]
//! CLI entrypoint for `agy-statusline`.

use std::env;
use std::fs;
use std::io::{self, Read};

use agy_statusline::apply::{default_settings_path, run_apply_command};
use agy_statusline::db::SqliteSpeedReader;
use agy_statusline::layout::{
    default_config_path, load_layout_config, save_layout_config, LayoutConfig,
};
use agy_statusline::statusline::process_statusline_with_config;
use agy_statusline::tui::run_tui_config;

const MAX_STDIN_BYTES: u64 = 1024 * 1024;

fn print_help() {
    println!(
        "Usage:\n  \
         agy-statusline [--full|--preset <name>]      Render statusline from stdin JSON\n  \
         agy-statusline config                        Launch 3-pane interactive TUI editor\n  \
         agy-statusline config --preset <name>        Apply preset (full|powerline|minimal|addon)\n  \
         agy-statusline config --show                 Print current XDG layout config\n  \
         agy-statusline config --reset                Remove XDG layout config (restore defaults)\n  \
         agy-statusline apply [--yes|-y] [--stack|--replace]\n                                               Register statusLine in AGY settings.json"
    );
}

fn parse_preset(name: &str) -> io::Result<LayoutConfig> {
    match name {
        "full" => Ok(LayoutConfig::preset_full()),
        "powerline" => Ok(LayoutConfig::preset_powerline()),
        "minimal" => Ok(LayoutConfig::preset_minimal()),
        "addon" => Ok(LayoutConfig::preset_addon()),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown preset '{other}' (expected full|powerline|minimal|addon)"),
        )),
    }
}

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let mut preset_override: Option<LayoutConfig> = env::var_os("AGY_STATUSLINE_FULL")
        .filter(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .map(|_| LayoutConfig::preset_full());

    if let Some(first) = args.next() {
        match first.as_str() {
            "apply" => {
                let mut assume_yes = false;
                let mut stack_override: Option<bool> = None;
                for arg in args {
                    match arg.as_str() {
                        "--yes" | "-y" => assume_yes = true,
                        "--stack" => stack_override = Some(true),
                        "--replace" => stack_override = Some(false),
                        "--help" | "-h" => {
                            println!("Usage: agy-statusline apply [--yes|-y] [--stack|--replace]");
                            return Ok(());
                        }
                        other => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("unknown option for apply: {other}"),
                            ));
                        }
                    }
                }
                let stack_with_default = stack_override.unwrap_or_else(|| {
                    default_config_path()
                        .map(|p| load_layout_config(&p).stack_with_default)
                        .unwrap_or(false)
                });
                let settings_path = default_settings_path()?;
                let exe_path = env::current_exe()?;
                let command_path = exe_path.canonicalize().unwrap_or(exe_path);
                let mut stdin = io::stdin().lock();
                let mut stdout = io::stdout().lock();
                return run_apply_command(
                    assume_yes,
                    stack_with_default,
                    &settings_path,
                    &command_path,
                    &mut stdin,
                    &mut stdout,
                );
            }
            "config" => {
                let config_path = default_config_path()?;
                if let Some(flag) = args.next() {
                    match flag.as_str() {
                        "--show" => {
                            let cfg = load_layout_config(&config_path);
                            let json = serde_json::to_string_pretty(&cfg)
                                .map_err(|e| io::Error::other(e.to_string()))?;
                            println!("{json}");
                            return Ok(());
                        }
                        "--reset" => {
                            if config_path.exists() {
                                fs::remove_file(&config_path)?;
                                println!("Removed {}", config_path.display());
                            } else {
                                println!("No config file at {}", config_path.display());
                            }
                            return Ok(());
                        }
                        "--preset" => {
                            let name = args.next().ok_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    "missing preset name (full|powerline|minimal|addon)",
                                )
                            })?;
                            let cfg = parse_preset(&name)?;
                            save_layout_config(&config_path, &cfg)?;
                            println!("Applied preset '{name}' to {}", config_path.display());
                            return Ok(());
                        }
                        "--help" | "-h" => {
                            println!(
                                "Usage: agy-statusline config [--show|--reset|--preset <full|powerline|minimal|addon>]"
                            );
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
                return run_tui_config(&config_path);
            }
            "--full" => preset_override = Some(LayoutConfig::preset_full()),
            "--preset" => {
                let name = args.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "missing preset name (full|powerline|minimal|addon)",
                    )
                })?;
                preset_override = Some(parse_preset(&name)?);
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument: {other}"),
                ));
            }
        }
    }

    let cfg = match preset_override {
        Some(c) => c,
        None => default_config_path()
            .map(|p| load_layout_config(&p))
            .unwrap_or_default(),
    };

    let mut input = String::new();
    let _ = io::stdin()
        .take(MAX_STDIN_BYTES)
        .read_to_string(&mut input);
    let reader = SqliteSpeedReader::default();
    let mut stdout = io::stdout().lock();
    process_statusline_with_config(&input, &reader, &cfg, &mut stdout)
}
