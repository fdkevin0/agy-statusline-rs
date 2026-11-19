#![forbid(unsafe_code)]
//! CLI entrypoint for `agy-statusline`.

use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::path::Path;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

use agy_statusline::apply::{
    resolve_invoked_binary_path, run_apply_command, set_statusline_enabled,
};
use agy_statusline::db::SqliteSpeedReader;
use agy_statusline::doctor::run_doctor;
use agy_statusline::git::SystemGitProbe;
use agy_statusline::layout::{load_layout_config, save_layout_config, LayoutConfig, Preset};
use agy_statusline::paths::{
    agy_settings_path, resolve_agy_cli_dir, resolve_home_dir, xdg_cache_dir, xdg_config_dir,
};
use agy_statusline::stats::{run_pricing_command, run_stats_command, SyncMode};
use agy_statusline::statusline::{process_statusline, PricingSource, StatuslineRuntime};
use agy_statusline::tui::run_tui_config;

const MAX_STDIN_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum PresetArg {
    Full,
    Powerline,
    Minimal,
    Addon,
}

impl PresetArg {
    const fn to_preset(self) -> Preset {
        match self {
            Self::Full => Preset::Full,
            Self::Powerline => Preset::Powerline,
            Self::Minimal => Preset::Minimal,
            Self::Addon => Preset::Addon,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "agy-statusline",
    version,
    about = "Safe Rust statusline for Antigravity CLI (agy)",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Render with the built-in full preset (overrides saved config)
    #[arg(long, conflicts_with = "preset")]
    full: bool,

    /// Render with a built-in preset (overrides saved config)
    #[arg(long, value_enum, value_name = "NAME")]
    preset: Option<PresetArg>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Configure statusline layout (launches 3-pane interactive TUI when no flag is given)
    Config(ConfigArgs),

    /// Register or disable statusLine in AGY settings.json
    Apply {
        /// Skip confirmation prompt
        #[arg(short = 'y', long = "yes")]
        yes: bool,

        /// Stack statusline below AGY's built-in bar
        #[arg(long, conflicts_with = "replace")]
        stack: bool,

        /// Replace AGY's built-in statusline
        #[arg(long, conflicts_with = "stack")]
        replace: bool,

        /// Disable statusLine in settings.json while retaining hook configuration
        #[arg(long)]
        disable: bool,
    },

    /// Check configuration, AGY hook, and render a live sample preview
    Doctor,

    /// Show local token usage, estimated API cost, and speed stats
    Stats {
        /// Restrict summary to the last N days (>= 1)
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
        days: Option<u32>,

        /// Output machine-readable JSON
        #[arg(long)]
        json: bool,

        /// Use pure 7-bit ASCII characters
        #[arg(long)]
        ascii: bool,
    },

    /// Show or force-refresh cached models.dev pricing (24h TTL)
    Pricing {
        #[command(subcommand)]
        action: Option<PricingCommand>,
    },

    /// Print version information
    Version,
}

#[derive(Debug, Args)]
#[group(multiple = false)]
struct ConfigArgs {
    /// Print current XDG layout config
    #[arg(long)]
    show: bool,

    /// Remove XDG layout config (restore defaults)
    #[arg(long)]
    reset: bool,

    /// Apply a built-in preset non-interactively
    #[arg(long, value_enum, value_name = "NAME")]
    preset: Option<PresetArg>,
}

#[derive(Debug, Subcommand)]
enum PricingCommand {
    /// Show current pricing cache status (default)
    Status,

    /// Force-refresh cached models.dev pricing
    Sync {
        /// Only refresh if the cache is missing or past its 24h TTL
        #[arg(long)]
        if_stale: bool,
    },
}

fn unix_now_secs() -> u64 {
    jiff::Timestamp::now().as_second().max(0) as u64
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let no_color = env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());

    if let Some(command) = cli.command {
        match command {
            Command::Apply {
                yes,
                stack,
                replace,
                disable,
            } => {
                let settings_path = agy_settings_path()?;
                if disable {
                    set_statusline_enabled(&settings_path, false)?;
                    println!("Disabled statusLine in {}", settings_path.display());
                    return Ok(());
                }
                let stack_override = if stack {
                    Some(true)
                } else if replace {
                    Some(false)
                } else {
                    None
                };
                let stack_with_default = stack_override.unwrap_or_else(|| {
                    xdg_config_dir()
                        .map(|d| load_layout_config(&d.join("config.json")).stack_with_default)
                        .unwrap_or(false)
                });
                let command_path = resolve_invoked_binary_path()?;
                let mut stdin = io::stdin().lock();
                let mut stdout = io::stdout().lock();
                return run_apply_command(
                    yes,
                    stack_with_default,
                    &settings_path,
                    &command_path,
                    &mut stdin,
                    &mut stdout,
                );
            }
            Command::Doctor => {
                let config_path = xdg_config_dir()?.join("config.json");
                let settings_path = agy_settings_path()?;
                let mut stdout = io::stdout().lock();
                return run_doctor(&config_path, &settings_path, no_color, &mut stdout);
            }
            Command::Stats { days, json, ascii } => {
                let home = resolve_home_dir();
                if home.is_none() && resolve_agy_cli_dir().is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "neither HOME, USERPROFILE, ANTIGRAVITY_CLI_HOME, nor AGY_HOME environment variable is set",
                    ));
                }
                let mut stdout = io::stdout().lock();
                return run_stats_command(
                    home.as_deref().unwrap_or_else(|| Path::new("")),
                    days,
                    json,
                    no_color,
                    ascii,
                    unix_now_secs(),
                    &mut stdout,
                );
            }
            Command::Config(args) => {
                let config_path = xdg_config_dir()?.join("config.json");
                if args.show {
                    let cfg = load_layout_config(&config_path);
                    let json = serde_json::to_string_pretty(&cfg)
                        .map_err(|e| io::Error::other(e.to_string()))?;
                    println!("{json}");
                    return Ok(());
                }
                if args.reset {
                    if config_path.exists() {
                        fs::remove_file(&config_path)?;
                        println!("Removed {}", config_path.display());
                    } else {
                        println!("No config file at {}", config_path.display());
                    }
                    return Ok(());
                }
                if let Some(preset_arg) = args.preset {
                    let preset = preset_arg.to_preset();
                    save_layout_config(&config_path, &preset.to_layout())?;
                    println!(
                        "Applied preset '{}' to {}",
                        preset.name(),
                        config_path.display()
                    );
                    return Ok(());
                }
                return run_tui_config(&config_path);
            }
            Command::Pricing { action } => {
                let (sync, if_stale) = match action {
                    None | Some(PricingCommand::Status) => (false, false),
                    Some(PricingCommand::Sync { if_stale }) => (true, if_stale),
                };
                let mut stdout = io::stdout().lock();
                return run_pricing_command(sync, if_stale, &mut stdout);
            }
            Command::Version => {
                println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
        }
    }

    let preset_override: Option<LayoutConfig> = if cli.full {
        Some(LayoutConfig::preset_full())
    } else {
        cli.preset.map(|p| p.to_preset().to_layout())
    };

    if preset_override.is_none() && io::stdin().is_terminal() {
        Cli::command().print_help()?;
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
    let agy_cli_dir = resolve_agy_cli_dir();
    let runtime = StatuslineRuntime {
        reader: &reader,
        git_probe: &git_probe,
        calibration_dir: calibration_dir.as_deref(),
        agy_cli_dir: agy_cli_dir.as_deref(),
        pricing: PricingSource::Effective(SyncMode::SpawnIfStale),
        now_unix_secs: unix_now_secs(),
    };
    let mut stdout = io::stdout().lock();
    process_statusline(&input, &runtime, &cfg, &mut stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_presets_subcommands_and_rejects_invalid_combinations() {
        let top_eq = Cli::try_parse_from(["agy-statusline", "--preset=minimal"]).unwrap();
        assert_eq!(top_eq.preset, Some(PresetArg::Minimal));
        assert!(!top_eq.full);
        assert!(top_eq.command.is_none());

        let cfg_eq =
            Cli::try_parse_from(["agy-statusline", "config", "--preset=powerline"]).unwrap();
        assert!(matches!(
            cfg_eq.command,
            Some(Command::Config(ConfigArgs {
                show: false,
                reset: false,
                preset: Some(PresetArg::Powerline),
            }))
        ));

        let pricing_default = Cli::try_parse_from(["agy-statusline", "pricing"]).unwrap();
        assert!(matches!(
            pricing_default.command,
            Some(Command::Pricing { action: None })
        ));

        let pricing_sync =
            Cli::try_parse_from(["agy-statusline", "pricing", "sync", "--if-stale"]).unwrap();
        assert!(matches!(
            pricing_sync.command,
            Some(Command::Pricing {
                action: Some(PricingCommand::Sync { if_stale: true }),
            })
        ));

        assert!(Cli::try_parse_from(["agy-statusline", "--full", "--preset", "minimal"]).is_err());
        assert!(Cli::try_parse_from(["agy-statusline", "--full", "doctor"]).is_err());
        assert!(Cli::try_parse_from(["agy-statusline", "--preset=minimal", "extra"]).is_err());
        assert!(Cli::try_parse_from(["agy-statusline", "config", "--show", "--reset"]).is_err());
        assert!(Cli::try_parse_from(["agy-statusline", "config", "--show", "extra"]).is_err());
        assert!(Cli::try_parse_from(["agy-statusline", "apply", "--stack", "--replace"]).is_err());
        assert!(Cli::try_parse_from(["agy-statusline", "stats", "--days", "0"]).is_err());
        assert!(Cli::try_parse_from(["agy-statusline", "pricing", "--if-stale"]).is_err());
        assert!(Cli::try_parse_from(["agy-statusline", "pricing", "status", "--if-stale"]).is_err());
    }
}
