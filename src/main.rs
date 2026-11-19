#![forbid(unsafe_code)]
//! CLI entrypoint for `agy-statusline`.

use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use agy_statusline::apply::{
    resolve_invoked_binary_path, run_apply_command, run_doctor, set_statusline_enabled,
};
use agy_statusline::db::SqliteSpeedReader;
use agy_statusline::git::SystemGitProbe;
use agy_statusline::layout::{load_layout_config, save_layout_config, LayoutConfig, Preset};
use agy_statusline::paths::{
    agy_settings_path, resolve_agy_cli_dir, resolve_home_dir, xdg_cache_dir, xdg_config_dir,
};
use agy_statusline::stats::{run_pricing_command, run_stats_command};
use agy_statusline::statusline::process_statusline_with_calibration;
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
         agy-statusline doctor                        Check configuration, AGY hook, and sample preview\n  \
         agy-statusline stats [--days <N>] [--json] [--ascii]\n                                               Show local token usage, estimated API cost, and speed stats\n  \
         agy-statusline pricing [status|sync]         Show or force-refresh cached models.dev pricing (24h TTL)\n  \
         agy-statusline version | --version | -V      Print version information"
    );
}

fn print_version() {
    println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
}

fn parse_preset(name: &str) -> io::Result<LayoutConfig> {
    Preset::from_name(name).map(Preset::to_layout).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown preset '{name}' (expected full|powerline|minimal|addon)"),
        )
    })
}

fn parse_days_value(val: &str) -> io::Result<u32> {
    let parsed = val.parse::<u32>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid --days value '{val}' (expected positive integer)"),
        )
    })?;
    if parsed == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--days must be >= 1",
        ));
    }
    Ok(parsed)
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
                let settings_path = agy_settings_path()?;
                if disable {
                    set_statusline_enabled(&settings_path, false)?;
                    println!("Disabled statusLine in {}", settings_path.display());
                    return Ok(());
                }
                let stack_with_default = stack_override.unwrap_or_else(|| {
                    xdg_config_dir()
                        .map(|d| load_layout_config(&d.join("config.json")).stack_with_default)
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
                let config_path = xdg_config_dir()?.join("config.json");
                let settings_path = agy_settings_path()?;
                let mut stdout = io::stdout().lock();
                return run_doctor(&config_path, &settings_path, no_color, &mut stdout);
            }
            "stats" => {
                let mut days: Option<u32> = None;
                let mut json = false;
                let mut ascii = false;
                while let Some(arg) = args.next() {
                    match arg.as_str() {
                        "--json" => json = true,
                        "--ascii" => ascii = true,
                        "--days" => {
                            let val = args.next().ok_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    "missing value for --days",
                                )
                            })?;
                            days = Some(parse_days_value(&val)?);
                        }
                        s if s.starts_with("--days=") => {
                            days = Some(parse_days_value(&s["--days=".len()..])?);
                        }
                        "--help" | "-h" => {
                            println!("Usage: agy-statusline stats [--days <N>] [--json] [--ascii]");
                            return Ok(());
                        }
                        other => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("unknown option for stats: {other}"),
                            ));
                        }
                    }
                }
                let home = resolve_home_dir();
                if home.is_none() && resolve_agy_cli_dir().is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "neither HOME, USERPROFILE, ANTIGRAVITY_CLI_HOME, nor AGY_HOME environment variable is set",
                    ));
                }
                let now_unix_secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let mut stdout = io::stdout().lock();
                return run_stats_command(
                    home.as_deref().unwrap_or_else(|| Path::new("")),
                    days,
                    json,
                    no_color,
                    ascii,
                    now_unix_secs,
                    &mut stdout,
                );
            }
            "config" => {
                let config_path = xdg_config_dir()?.join("config.json");
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
            "pricing" => {
                let mut sync = false;
                let mut if_stale = false;
                for arg in args {
                    match arg.as_str() {
                        "sync" => sync = true,
                        "status" => {}
                        "--if-stale" => if_stale = true,
                        "--help" | "-h" => {
                            println!("Usage: agy-statusline pricing [status|sync [--if-stale]]");
                            return Ok(());
                        }
                        other => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("unknown option for pricing: {other}"),
                            ));
                        }
                    }
                }
                if if_stale && !sync {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--if-stale requires 'pricing sync'",
                    ));
                }
                let mut stdout = io::stdout().lock();
                return run_pricing_command(sync, if_stale, &mut stdout);
            }
            "version" | "--version" | "-V" => {
                if let Some(flag) = args.next() {
                    if matches!(flag.as_str(), "--help" | "-h") {
                        println!("Usage: agy-statusline version");
                        return Ok(());
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown option for version: {flag}"),
                    ));
                }
                print_version();
                return Ok(());
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
        None => xdg_config_dir()
            .map(|d| load_layout_config(&d.join("config.json")))
            .unwrap_or_default(),
    };
    cfg.no_color = no_color;

    let mut input = String::new();
    let _ = io::stdin()
        .take(MAX_STDIN_BYTES)
        .read_to_string(&mut input);
    let reader = SqliteSpeedReader::default();
    let git_probe = SystemGitProbe::with_default_cache();
    let calibration_dir = xdg_cache_dir();
    let now_unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut stdout = io::stdout().lock();
    process_statusline_with_calibration(
        &input,
        &reader,
        &git_probe,
        calibration_dir.as_deref(),
        now_unix_secs,
        &cfg,
        &mut stdout,
    )
}
