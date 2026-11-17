#![forbid(unsafe_code)]
//! CLI entrypoint for `agy-statusline`.

use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read};

use agy_statusline::apply::{
    default_settings_path, resolve_invoked_binary_path, run_apply_command, run_doctor,
    set_statusline_enabled,
};
use agy_statusline::db::SqliteSpeedReader;
use agy_statusline::git::SystemGitProbe;
use agy_statusline::layout::{
    default_config_path, load_layout_config, save_layout_config, LayoutConfig,
};
use agy_statusline::statusline::process_statusline_with_probes;
use agy_statusline::tui::run_tui_config;

const MAX_STDIN_BYTES: u64 = 1024 * 1024;

fn print_help() {
    println!(
        "Usage:\n  \
         agy-statusline [--full|--preset <name>]      Render statusline from stdin JSON (respects NO_COLOR)\n  \
         agy-statusline config                        Launch 3-pane interactive TUI editor\n  \
         agy-statusline config --preset <name>        Apply preset (full|powerline|minimal|addon)\n  \
         agy-statusline config --show                 Print current XDG layout config\n  \
         agy-statusline config --reset                Remove XDG layout config (restore defaults)\n  \
         agy-statusline apply [--yes|-y] [--stack|--replace|--disable]\n                                               Register or disable statusLine in AGY settings.json\n  \
         agy-statusline doctor                        Check configuration, AGY hook, and sample preview"
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
    let no_color = env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());

    if let Some(first) = args.next() {
        match first.as_str() {
            "apply" => {
                let mut assume_yes = false;
                let mut disable = false;
                let mut stack_override: Option<bool> = None;
                for arg in args {
                    match arg.as_str() {
                        "--yes" | "-y" => assume_yes = true,
                        "--stack" => stack_override = Some(true),
                        "--replace" => stack_override = Some(false),
                        "--disable" => disable = true,
                        "--help" | "-h" => {
                            println!("Usage: agy-statusline apply [--yes|-y] [--stack|--replace|--disable]");
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
                let settings_path = default_settings_path()?;
                if disable {
                    set_statusline_enabled(&settings_path, false)?;
                    println!("Disabled statusLine in {}", settings_path.display());
                    return Ok(());
                }
                let stack_with_default = stack_override.unwrap_or_else(|| {
                    default_config_path()
                        .map(|p| load_layout_config(&p).stack_with_default)
                        .unwrap_or(false)
                });
                let command_path = resolve_invoked_binary_path()?;
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
            "doctor" => {
                if let Some(flag) = args.next() {
                    if matches!(flag.as_str(), "--help" | "-h") {
                        println!("Usage: agy-statusline doctor");
                        return Ok(());
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown option for doctor: {flag}"),
                    ));
                }
                let config_path = default_config_path()?;
                let settings_path = default_settings_path()?;
                let mut stdout = io::stdout().lock();
                return run_doctor(&config_path, &settings_path, no_color, &mut stdout);
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

    if preset_override.is_none() && io::stdin().is_terminal() {
        print_help();
        return Ok(());
    }

    let mut cfg = match preset_override {
        Some(c) => c,
        None => default_config_path()
            .map(|p| load_layout_config(&p))
            .unwrap_or_default(),
    };
    cfg.no_color = no_color;

    let mut input = String::new();
    let _ = io::stdin()
        .take(MAX_STDIN_BYTES)
        .read_to_string(&mut input);
    let reader = SqliteSpeedReader::default();
    let git_probe = SystemGitProbe::with_default_cache();
    let mut stdout = io::stdout().lock();
    process_statusline_with_probes(&input, &reader, &git_probe, &cfg, &mut stdout)
}
