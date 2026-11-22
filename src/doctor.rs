//! Configuration, runtime environment, AGY hook, storage, and live preview diagnostics (`doctor`).

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::types::ValueRef;
use serde::Deserialize;
use serde_json::Value;

use crate::apply::read_settings_object;
use crate::calibration::{
    BucketCalibration, CACHE_FILENAME as CALIBRATION_CACHE_FILENAME,
    SCHEMA_VERSION as CALIBRATION_SCHEMA_VERSION,
};
use crate::db::{
    inspect_sqlite_path, is_absent_or_real_file, is_real_dir, is_safe_sqlite_file, is_valid_uuid,
    open_readonly_db, SqlitePathStatus,
};
use crate::layout::try_load_layout_config;
use crate::paths::{resolve_agy_cli_dir, xdg_cache_dir, xdg_data_dir};
use crate::procutil::{run_command_output_bounded, BoundedCommandError};
use crate::render::write_statusline;
use crate::stats::{ARCHIVE_DB_FILENAME, DEFAULT_PRICING_TTL_SECS, MODELS_DEV_CACHE_FILE};
use crate::statusline::{sample_preview_git, sample_preview_metadata, sample_preview_payload};
use crate::timeutil::unix_now_secs;

const AGY_VERSION_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_AGY_VERSION_BYTES: u64 = 4096;
const MAX_STDERR_SUMMARY_CHARS: usize = 120;
const MAX_RECENT_SESSION_SCANS: usize = 20;
const STEP_TYPE_ERROR: i64 = 17;
const MAX_CACHE_INSPECT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Default)]
struct DoctorEnv<'a> {
    conv_dir: Option<&'a Path>,
    data_dir: Option<&'a Path>,
    cache_dir: Option<&'a Path>,
    path_env: Option<&'a OsStr>,
    now_unix_secs: u64,
    no_color: bool,
    kernel_page_size_bytes: Option<u64>,
    ipv6_available: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RecentSessionHealth {
    checked: usize,
    poisoned_sessions: Vec<String>,
    historical_signature_errors: usize,
    quota_exhausted_errors: usize,
    other_step17_errors: usize,
}

#[derive(Deserialize)]
struct PricingCacheSummary {
    fetched_at: u64,
    #[serde(default)]
    models: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct CalibrationCacheSummary {
    schema_version: u32,
    #[serde(default)]
    buckets: BTreeMap<String, BucketCalibration>,
}

fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| {
        if !meta.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
}

fn extract_command_executable(cmd: &str) -> Option<String> {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix('"') {
        let mut out = String::new();
        let mut escaped = false;
        for (idx, ch) in rest.char_indices() {
            if escaped {
                if ch != '"' && ch != '\\' {
                    out.push('\\');
                }
                out.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                let after = &rest[idx + 1..];
                if !after.is_empty() && !after.starts_with(char::is_whitespace) {
                    return None;
                }
                return (!out.is_empty()).then_some(out);
            } else {
                out.push(ch);
            }
        }
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix('\'') {
        let (quoted, after) = rest.split_once('\'')?;
        if quoted.is_empty() || (!after.is_empty() && !after.starts_with(char::is_whitespace)) {
            return None;
        }
        return Some(quoted.to_string());
    }
    let trimmed_path = Path::new(trimmed);
    if trimmed_path.components().count() > 1 && is_executable_file(trimmed_path) {
        return Some(trimmed.to_string());
    }
    trimmed.split_whitespace().next().map(str::to_string)
}

fn command_executable_exists_in_path(cmd: &str, path_env: Option<&OsStr>) -> bool {
    let Some(exe) = extract_command_executable(cmd) else {
        return false;
    };
    let exe_path = Path::new(&exe);
    if exe_path.components().count() > 1 {
        return is_executable_file(exe_path);
    }
    path_env.is_some_and(|paths| which::which_in(&exe, Some(paths), ".").is_ok())
}

fn sanitize_single_line(line: &str) -> String {
    line.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

fn sanitized_nonempty_lines(bytes: &[u8]) -> Vec<String> {
    let raw = String::from_utf8_lossy(bytes);
    let stripped = strip_ansi_escapes::strip_str(&raw);
    stripped
        .lines()
        .map(sanitize_single_line)
        .filter(|line| !line.is_empty())
        .collect()
}

fn format_version_output(bytes: &[u8]) -> String {
    sanitized_nonempty_lines(bytes)
        .into_iter()
        .next()
        .unwrap_or_else(|| "unknown (empty output)".to_string())
}

/// Parses the first `KernelPageSize: <N> kB` line from a `/proc/self/smaps` stream
/// and stops reading immediately.
fn parse_smaps_kernel_page_size_bytes<R: BufRead>(mut reader: R) -> Option<u64> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let Some(rest) = line.strip_prefix("KernelPageSize:") else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let kib = parts.next()?.parse::<u64>().ok()?;
        let unit = parts.next()?;
        if !unit.eq_ignore_ascii_case("kb") || parts.next().is_some() {
            return None;
        }
        return kib.checked_mul(1024);
    }
}

fn format_page_size_warning(page_size_bytes: u64) -> Option<String> {
    if page_size_bytes <= 4096 {
        return None;
    }
    let kib = page_size_bytes / 1024;
    Some(format!(
        "{kib} KiB page size (> 4 KiB; incompatible with agy on Asahi Linux / RPi 5)"
    ))
}

fn probe_kernel_page_size_bytes() -> Option<u64> {
    if !cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        return None;
    }
    let file = fs::File::open("/proc/self/smaps").ok()?;
    parse_smaps_kernel_page_size_bytes(io::BufReader::new(file))
}

fn probe_ipv6_loopback() -> Result<(), io::Error> {
    UdpSocket::bind("[::1]:0").map(|_| ())
}

fn format_ipv6_status(available: bool, is_linux: bool) -> Option<String> {
    if available {
        return None;
    }
    if is_linux {
        Some(
            "IPv6 loopback [::1] unavailable (AF_INET6 disabled — e.g. WSL ipv6.disable=1)"
                .to_string(),
        )
    } else {
        Some("IPv6 loopback [::1] unavailable (AF_INET6 unsupported or disabled)".to_string())
    }
}

fn diagnose_agy_stderr(stderr: &[u8]) -> Option<String> {
    let lines = sanitized_nonempty_lines(stderr);
    if lines.is_empty() {
        return None;
    }
    for line in &lines {
        let lower = line.to_ascii_lowercase();
        if lower.contains("page-size.cc") {
            return Some(
                "kernel page size > 4 KiB unsupported by agy (e.g. Asahi Linux / RPi 5)".to_string(),
            );
        }
        if lower.contains("af_inet6") || lower.contains("address family not supported") {
            return Some("IPv6 loopback unavailable (e.g. WSL ipv6.disable=1)".to_string());
        }
    }
    Some(lines[0].chars().take(MAX_STDERR_SUMMARY_CHARS).collect())
}

fn probe_agy_version(path_env: Option<&OsStr>, timeout: Duration) -> String {
    let Some(paths) = path_env else {
        return "not found".to_string();
    };
    let Ok(agy_bin) = which::which_in("agy", Some(paths), ".") else {
        return "not found".to_string();
    };

    let mut cmd = Command::new(&agy_bin);
    cmd.arg("--version");
    match run_command_output_bounded(&mut cmd, timeout, MAX_AGY_VERSION_BYTES, MAX_AGY_VERSION_BYTES)
    {
        Ok(out) if out.status.is_some_and(|s| s.success()) => {
            match sanitized_nonempty_lines(&out.stdout).into_iter().next() {
                Some(version) => version,
                None => match diagnose_agy_stderr(&out.stderr) {
                    Some(cause) => format!("unavailable (empty output — {cause})"),
                    None => format_version_output(&out.stdout),
                },
            }
        }
        Ok(out) if out.stdout_truncated && !out.stdout.is_empty() => {
            match sanitized_nonempty_lines(&out.stdout).into_iter().next() {
                Some(version) => version,
                None => match diagnose_agy_stderr(&out.stderr) {
                    Some(cause) => format!("unavailable (truncated — {cause})"),
                    None => "unavailable (truncated)".to_string(),
                },
            }
        }
        Ok(out) => {
            let base = match out.status {
                Some(status) => status.to_string(),
                None => "truncated".to_string(),
            };
            match diagnose_agy_stderr(&out.stderr) {
                Some(cause) => format!("unavailable ({base} — {cause})"),
                None => format!("unavailable ({base})"),
            }
        }
        Err(BoundedCommandError::TimedOut) => "unavailable (timed out)".to_string(),
        Err(BoundedCommandError::Io(e)) => format!("unavailable ({e})"),
    }
}

fn collect_safe_session_dbs(conv_dir: &Path) -> io::Result<Vec<(SystemTime, String, PathBuf)>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(conv_dir)?.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Some(stem) = name_str.strip_suffix(".db") else {
            continue;
        };
        if !is_valid_uuid(stem) {
            continue;
        }
        let path = entry.path();
        if entry
            .file_type()
            .is_ok_and(|ft| !ft.is_symlink() && ft.is_file())
            && is_safe_sqlite_file(&path)
        {
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            found.push((mtime, stem.to_string(), path));
        }
    }
    Ok(found)
}

fn inspect_conversations_dir(conv_dir: &Path) -> String {
    let meta = match fs::symlink_metadata(conv_dir) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return "not found".to_string(),
        Err(e) => return format!("error: {e}"),
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        return "symlink (ignored)".to_string();
    }
    if !ft.is_dir() {
        return "not a directory".to_string();
    }
    let db_count = match collect_safe_session_dbs(conv_dir) {
        Ok(dbs) => dbs.len(),
        Err(e) => return format!("unreadable: {e}"),
    };
    let noun = if db_count == 1 { "file" } else { "files" };
    format!("found, {db_count} session .db {noun}")
}

fn value_ref_bytes(val: ValueRef<'_>) -> &[u8] {
    match val {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes,
        _ => &[],
    }
}

fn contains_ascii(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn scan_session_db_health(db_path: &Path, uuid: &str, health: &mut RecentSessionHealth) {
    health.checked += 1;
    let Ok(conn) = open_readonly_db(db_path) else {
        return;
    };

    let mut is_poisoned = false;
    if let Ok(mut stmt) = conn.prepare(
        "SELECT step_type, substr(step_payload, 1, 16384) FROM steps ORDER BY idx DESC LIMIT 1",
    ) {
        if let Ok(mut rows) = stmt.query([]) {
            if let Ok(Some(row)) = rows.next() {
                let step_type = row.get::<_, i64>(0).ok();
                let payload = row.get_ref(1).map(value_ref_bytes).unwrap_or(&[]);
                if step_type == Some(STEP_TYPE_ERROR)
                    && contains_ascii(payload, b"INVALID_THINKING_SIGNATURE")
                {
                    is_poisoned = true;
                    health.poisoned_sessions.push(uuid[..8].to_string());
                }
            }
        }
    }

    let mut saw_historical_sig = false;
    if let Ok(mut stmt) = conn.prepare(
        "SELECT substr(step_payload, 1, 16384) FROM steps WHERE step_type = 17 ORDER BY idx DESC LIMIT 16",
    ) {
        if let Ok(mut rows) = stmt.query([]) {
            while let Ok(Some(row)) = rows.next() {
                let payload = row.get_ref(0).map(value_ref_bytes).unwrap_or(&[]);
                let is_sig = contains_ascii(payload, b"INVALID_THINKING_SIGNATURE");
                let is_quota = contains_ascii(payload, b"QUOTA_EXHAUSTED")
                    || contains_ascii(payload, b"RESOURCE_EXHAUSTED");
                if is_sig {
                    if !is_poisoned {
                        saw_historical_sig = true;
                    }
                } else if is_quota {
                    health.quota_exhausted_errors += 1;
                } else {
                    health.other_step17_errors += 1;
                }
            }
        }
    }
    if saw_historical_sig {
        health.historical_signature_errors += 1;
    }
}

fn inspect_recent_session_errors(conv_dir: &Path) -> Option<RecentSessionHealth> {
    if !is_real_dir(conv_dir) {
        return None;
    }
    let mut dbs = collect_safe_session_dbs(conv_dir).ok()?;
    if dbs.is_empty() {
        return None;
    }
    dbs.sort_by(|a, b| Reverse((a.0, &a.1)).cmp(&Reverse((b.0, &b.1))));
    let mut health = RecentSessionHealth::default();
    for (_, uuid, path) in dbs.into_iter().take(MAX_RECENT_SESSION_SCANS) {
        scan_session_db_health(&path, &uuid, &mut health);
    }
    Some(health)
}

fn format_session_health(health: &RecentSessionHealth) -> String {
    let noun = if health.checked == 1 {
        "session"
    } else {
        "sessions"
    };
    let mut parts = Vec::new();
    if !health.poisoned_sessions.is_empty() {
        parts.push(format!(
            "{} poisoned INVALID_THINKING_SIGNATURE ({})",
            health.poisoned_sessions.len(),
            health.poisoned_sessions.join(", ")
        ));
    }
    if health.historical_signature_errors > 0 {
        parts.push(format!(
            "{} recovered INVALID_THINKING_SIGNATURE",
            health.historical_signature_errors
        ));
    }
    if health.quota_exhausted_errors > 0 {
        parts.push(format!("{} QUOTA_EXHAUSTED", health.quota_exhausted_errors));
    }
    if health.other_step17_errors > 0 {
        parts.push(format!("{} other step_type=17", health.other_step17_errors));
    }
    if parts.is_empty() {
        format!("checked {} recent {noun} [clean]", health.checked)
    } else {
        format!(
            "checked {} recent {noun} [{}]",
            health.checked,
            parts.join("; ")
        )
    }
}

fn format_byte_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if (bytes as f64) < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else if (bytes as f64) < GIB {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else {
        format!("{:.1} GiB", bytes as f64 / GIB)
    }
}

fn inspect_archive_db(archive_path: &Path) -> String {
    match inspect_sqlite_path(archive_path) {
        SqlitePathStatus::Missing => "not found".to_string(),
        SqlitePathStatus::Unsafe => "symlink or unsafe sidecar (ignored)".to_string(),
        SqlitePathStatus::Valid => match fs::symlink_metadata(archive_path) {
            Ok(meta) => {
                let size = format_byte_size(meta.len());
                let schema = open_readonly_db(archive_path).and_then(|conn| {
                    conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                });
                match schema {
                    Ok(version) => format!("found, {size}, schema v{version}"),
                    Err(_) => format!("found, {size}, unreadable schema"),
                }
            }
            Err(e) => format!("error: {e}"),
        },
    }
}

fn inspect_cache_file_safety(path: &Path) -> Result<bool, String> {
    if !is_absent_or_real_file(path) {
        let is_symlink = fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
        return Err(if is_symlink {
            "symlink (ignored)".to_string()
        } else {
            "not a regular file (ignored)".to_string()
        });
    }
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("unreadable ({e})")),
    }
}

fn read_file_bounded(path: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    let mut buf = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)?;
    if (buf.len() as u64) > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "exceeds maximum inspected size",
        ));
    }
    Ok(buf)
}

fn compact_json_error(err: &serde_json::Error) -> String {
    sanitize_single_line(&err.to_string())
        .chars()
        .take(80)
        .collect()
}

fn inspect_pricing_cache(cache_dir: &Path, now_unix_secs: u64) -> String {
    let path = cache_dir.join(MODELS_DEV_CACHE_FILE);
    match inspect_cache_file_safety(&path) {
        Ok(false) => return "not found".to_string(),
        Err(reason) => return reason,
        Ok(true) => {}
    }
    let bytes = match read_file_bounded(&path, MAX_CACHE_INSPECT_BYTES) {
        Ok(b) => b,
        Err(e) => return format!("corrupt ({e})"),
    };
    let parsed: PricingCacheSummary = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => return format!("corrupt ({})", compact_json_error(&e)),
    };
    let freshness = if now_unix_secs.saturating_sub(parsed.fetched_at) < DEFAULT_PRICING_TTL_SECS {
        "fresh"
    } else {
        "stale"
    };
    format!("found, {} models ({freshness})", parsed.models.len())
}

fn inspect_calibration_cache(cache_dir: &Path) -> String {
    let path = cache_dir.join(CALIBRATION_CACHE_FILENAME);
    match inspect_cache_file_safety(&path) {
        Ok(false) => return "not found".to_string(),
        Err(reason) => return reason,
        Ok(true) => {}
    }
    let bytes = match read_file_bounded(&path, MAX_CACHE_INSPECT_BYTES) {
        Ok(b) => b,
        Err(e) => return format!("corrupt ({e})"),
    };
    let parsed: CalibrationCacheSummary = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => return format!("corrupt ({})", compact_json_error(&e)),
    };
    if parsed.schema_version != CALIBRATION_SCHEMA_VERSION {
        return format!("stale schema v{}", parsed.schema_version);
    }
    let total = parsed.buckets.len();
    let calibrated = parsed
        .buckets
        .values()
        .filter(|b| b.calibrated_total_tokens.is_some())
        .count();
    format!(
        "found, schema v{}, {calibrated}/{total} calibrated",
        parsed.schema_version
    )
}

fn run_doctor_with<W: Write>(
    config_path: &Path,
    settings_path: &Path,
    env: &DoctorEnv<'_>,
    out: &mut W,
) -> io::Result<()> {
    writeln!(out, "agy-statusline v{}", env!("CARGO_PKG_VERSION"))?;

    let (mut cfg, cfg_state) = match try_load_layout_config(config_path) {
        Ok(cfg) => (cfg, "custom file".into()),
        Err(e) => {
            let state = match e.kind() {
                io::ErrorKind::NotFound => "not found (using built-in defaults)".into(),
                io::ErrorKind::InvalidData => {
                    format!("invalid config: {e} (using built-in defaults)")
                }
                _ => format!("unreadable config: {e} (using built-in defaults)"),
            };
            (Default::default(), state)
        }
    };
    cfg.no_color = env.no_color;
    writeln!(out, "Config:   {} [{cfg_state}]", config_path.display())?;
    writeln!(
        out,
        "Active:   style={} · theme={} · glyphs={} · stack={}",
        cfg.style.name(),
        cfg.theme.name(),
        cfg.glyph_mode.name(),
        cfg.stack_with_default
    )?;
    writeln!(
        out,
        "NO_COLOR: {}",
        if env.no_color { "active" } else { "unset" }
    )?;

    if let Some(warn) = env.kernel_page_size_bytes.and_then(format_page_size_warning) {
        writeln!(out, "Kernel:   warning: {warn}")?;
    }
    if let Some(warn) = format_ipv6_status(env.ipv6_available, cfg!(target_os = "linux")) {
        writeln!(out, "Network:  warning: {warn}")?;
    }

    writeln!(
        out,
        "AGY CLI:  {}",
        probe_agy_version(env.path_env, AGY_VERSION_TIMEOUT)
    )?;

    match read_settings_object(settings_path, false) {
        Ok(root) => match root.get("statusLine").and_then(Value::as_object) {
            Some(sl) => {
                let cmd_opt = sl.get("command").and_then(Value::as_str);
                let cmd = cmd_opt.unwrap_or("<missing>");
                let enabled = sl
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let stack = sl
                    .get("stack_with_default")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let cmd_exists =
                    cmd_opt.is_some_and(|c| command_executable_exists_in_path(c, env.path_env));
                writeln!(
                    out,
                    "AGY Hook: {} [enabled={enabled}, stack={stack}, bin_exists={cmd_exists}]",
                    settings_path.display()
                )?;
                writeln!(out, "Command:  {cmd}")?;
            }
            None => {
                writeln!(
                    out,
                    "AGY Hook: {} [missing statusLine entry — run `agy-statusline apply`]",
                    settings_path.display()
                )?;
            }
        },
        Err(e) => {
            writeln!(out, "AGY Hook: {} [error: {e}]", settings_path.display())?;
        }
    }

    match env.conv_dir {
        Some(dir) => {
            let state = inspect_conversations_dir(dir);
            writeln!(out, "Sessions: {} [{state}]", dir.display())?;
            if let Some(health) = inspect_recent_session_errors(dir) {
                writeln!(out, "Health:   {}", format_session_health(&health))?;
            }
        }
        None => {
            writeln!(out, "Sessions: unavailable [AGY CLI directory unresolved]")?;
        }
    }

    match env.data_dir {
        Some(dir) => {
            let archive_path = dir.join(ARCHIVE_DB_FILENAME);
            let state = inspect_archive_db(&archive_path);
            writeln!(out, "Archive:  {} [{state}]", archive_path.display())?;
        }
        None => {
            writeln!(
                out,
                "Archive:  unavailable [neither XDG_DATA_HOME nor HOME is set]"
            )?;
        }
    }

    match env.cache_dir {
        Some(dir) => {
            let pricing_path = dir.join(MODELS_DEV_CACHE_FILE);
            let pricing_state = inspect_pricing_cache(dir, env.now_unix_secs);
            writeln!(out, "Pricing:  {} [{pricing_state}]", pricing_path.display())?;

            let calib_path = dir.join(CALIBRATION_CACHE_FILENAME);
            let calib_state = inspect_calibration_cache(dir);
            writeln!(out, "Calib:    {} [{calib_state}]", calib_path.display())?;
        }
        None => {
            writeln!(
                out,
                "Pricing:  unavailable [neither XDG_CACHE_HOME nor HOME is set]"
            )?;
            writeln!(
                out,
                "Calib:    unavailable [neither XDG_CACHE_HOME nor HOME is set]"
            )?;
        }
    }

    writeln!(out, "Preview:")?;
    let sample = sample_preview_payload(100);
    let meta = sample_preview_metadata();
    let git = sample_preview_git();
    write_statusline(&sample, &meta, &git, &cfg, out)?;
    Ok(())
}

/// Inspects XDG config, runtime environment, AGY `settings.json` wiring, local
/// AGY/archive/cache storage, optional `agy --version`, and prints a live sample preview.
pub fn run_doctor<W: Write>(
    config_path: &Path,
    settings_path: &Path,
    no_color: bool,
    out: &mut W,
) -> io::Result<()> {
    let conv_dir: Option<PathBuf> = settings_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.join("conversations"))
        .or_else(|| resolve_agy_cli_dir().map(|d| d.join("conversations")));
    let data_dir = xdg_data_dir();
    let cache_dir = xdg_cache_dir();
    let path_env = env::var_os("PATH");
    let doctor_env = DoctorEnv {
        conv_dir: conv_dir.as_deref(),
        data_dir: data_dir.as_deref(),
        cache_dir: cache_dir.as_deref(),
        path_env: path_env.as_deref(),
        now_unix_secs: unix_now_secs(),
        no_color,
        kernel_page_size_bytes: probe_kernel_page_size_bytes(),
        ipv6_available: probe_ipv6_loopback().is_ok(),
    };
    run_doctor_with(config_path, settings_path, &doctor_env, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Cursor;

    use rusqlite::Connection;

    use crate::apply::apply_statusline_config;
    use crate::paths::agy_settings_path_for_home;

    #[cfg(windows)]
    const FAKE_BIN_NAME: &str = "agy-statusline-fake.exe";
    #[cfg(not(windows))]
    const FAKE_BIN_NAME: &str = "agy-statusline-fake";

    fn create_steps_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE steps (
                idx INTEGER PRIMARY KEY,
                step_type INTEGER,
                step_payload BLOB
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn doctor_reports_config_settings_and_preview() {
        let home = tempfile::tempdir().unwrap();
        let cfg_path = home.path().join("config.json");
        let settings = agy_settings_path_for_home(home.path());
        let conv_dir = settings.parent().unwrap().join("conversations");
        let data_dir = home.path().join("data");
        let cache_dir = home.path().join("cache");
        apply_statusline_config(&settings, Path::new("/nonexistent/agy-statusline"), false).unwrap();

        let doctor_env = DoctorEnv {
            conv_dir: Some(&conv_dir),
            data_dir: Some(&data_dir),
            cache_dir: Some(&cache_dir),
            path_env: None,
            now_unix_secs: 1_700_000_000,
            no_color: true,
            kernel_page_size_bytes: Some(4096),
            ipv6_available: true,
        };
        let mut out = Vec::new();
        run_doctor_with(&cfg_path, &settings, &doctor_env, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("NO_COLOR: active"));
        assert!(text.contains("enabled=true"));
        assert!(text.contains("bin_exists=false"));
        assert!(text.contains("Pricing:"));
        assert!(text.contains("Calib:"));
        assert!(text.contains("Preview:"));
        assert!(!text.contains("Kernel:"));
        assert!(!text.contains("Network:"));
        assert!(!text.contains('\x1b'));
    }

    #[test]
    fn doctor_emits_kernel_and_network_warnings_when_degraded() {
        let home = tempfile::tempdir().unwrap();
        let cfg_path = home.path().join("config.json");
        let settings = agy_settings_path_for_home(home.path());
        let doctor_env = DoctorEnv {
            conv_dir: None,
            data_dir: None,
            cache_dir: None,
            path_env: None,
            now_unix_secs: 1_700_000_000,
            no_color: true,
            kernel_page_size_bytes: Some(16_384),
            ipv6_available: false,
        };
        let mut out = Vec::new();
        run_doctor_with(&cfg_path, &settings, &doctor_env, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Kernel:   warning: 16 KiB page size"));
        assert!(text.contains("Asahi Linux"));
        assert!(text.contains("Network:  warning: IPv6 loopback [::1] unavailable"));
    }

    #[test]
    fn command_executable_lookup_handles_args_spaces_and_bare_path_names() {
        let home = tempfile::tempdir().unwrap();
        let bin_dir = home.path().join("bin");
        let space_dir = home.path().join("dir with spaces");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(&space_dir).unwrap();

        let bin_path = bin_dir.join(FAKE_BIN_NAME);
        let spaced_path = space_dir.join("agy-statusline-spaced");
        fs::write(&bin_path, b"").unwrap();
        fs::write(&spaced_path, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin_path, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&spaced_path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let path_env = env::join_paths([&bin_dir]).unwrap();

        // Explicit path with CLI arguments.
        let with_args = format!("{} --no-color --style minimal", bin_path.display());
        assert!(command_executable_exists_in_path(&with_args, None));

        // Direct path containing spaces.
        assert!(command_executable_exists_in_path(
            &spaced_path.to_string_lossy(),
            None
        ));

        // Bare command resolved via PATH (with and without args).
        assert!(command_executable_exists_in_path(
            "agy-statusline-fake",
            Some(path_env.as_os_str())
        ));
        assert!(command_executable_exists_in_path(
            "agy-statusline-fake --no-color",
            Some(path_env.as_os_str())
        ));

        // Missing bare command and missing explicit path.
        assert!(!command_executable_exists_in_path(
            "definitely-nonexistent-bin",
            Some(path_env.as_os_str())
        ));
        assert!(!command_executable_exists_in_path(
            "/nonexistent/agy-statusline --no-color",
            Some(path_env.as_os_str())
        ));

        // Also verify run_doctor_with reports bin_exists=true for command-with-args.
        let cfg_path = home.path().join("config.json");
        let settings = agy_settings_path_for_home(home.path());
        let conv_dir = settings.parent().unwrap().join("conversations");
        let data_dir = home.path().join("data");
        apply_statusline_config(&settings, Path::new(&with_args), false).unwrap();
        let doctor_env = DoctorEnv {
            conv_dir: Some(&conv_dir),
            data_dir: Some(&data_dir),
            cache_dir: None,
            path_env: Some(path_env.as_os_str()),
            now_unix_secs: 1_700_000_000,
            no_color: true,
            kernel_page_size_bytes: None,
            ipv6_available: true,
        };
        let mut out = Vec::new();
        run_doctor_with(&cfg_path, &settings, &doctor_env, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("bin_exists=true"));
    }

    #[test]
    fn inspect_conversations_and_version_output_handle_edge_cases() {
        let dir = tempfile::tempdir().unwrap();
        let file_as_dir = dir.path().join("conversations");
        fs::write(&file_as_dir, b"not a dir").unwrap();
        assert_eq!(inspect_conversations_dir(&file_as_dir), "not a directory");

        assert_eq!(
            format_version_output(b"\n\x1b[32m1.5.14\x07\x1b[0m\nignored line\n"),
            "1.5.14"
        );
        assert_eq!(format_version_output(b"   \n\t\r\n"), "unknown (empty output)");
    }

    #[test]
    fn parse_smaps_page_size_and_warning_formatting() {
        let smaps_4k = "00400000-00401000 r-xp 00000000 00:00 0\nSize:                  4 kB\nKernelPageSize:        4 kB\nKernelPageSize:       64 kB\n";
        assert_eq!(
            parse_smaps_kernel_page_size_bytes(Cursor::new(smaps_4k)),
            Some(4096)
        );
        assert_eq!(
            parse_smaps_kernel_page_size_bytes(Cursor::new("KernelPageSize:       16 kB\n")),
            Some(16_384)
        );
        assert_eq!(
            parse_smaps_kernel_page_size_bytes(Cursor::new("KernelPageSize:       64 kB\n")),
            Some(65_536)
        );
        assert_eq!(
            parse_smaps_kernel_page_size_bytes(Cursor::new("Size: 4 kB\nRss: 4 kB\n")),
            None
        );
        assert_eq!(
            parse_smaps_kernel_page_size_bytes(Cursor::new("KernelPageSize: nope kB\n")),
            None
        );
        assert_eq!(
            parse_smaps_kernel_page_size_bytes(Cursor::new("KernelPageSize: 16 MB\n")),
            None
        );

        assert_eq!(format_page_size_warning(4096), None);
        let warn_16k = format_page_size_warning(16_384).unwrap();
        assert!(warn_16k.contains("16 KiB"));
        assert!(warn_16k.contains("Asahi Linux"));
        assert!(warn_16k.contains("RPi 5"));
        let warn_64k = format_page_size_warning(65_536).unwrap();
        assert!(warn_64k.contains("64 KiB"));
    }

    #[test]
    fn format_ipv6_status_handles_available_linux_and_other_platforms() {
        assert_eq!(format_ipv6_status(true, true), None);
        assert_eq!(format_ipv6_status(true, false), None);

        let linux_warn = format_ipv6_status(false, true).unwrap();
        assert!(linux_warn.contains("WSL ipv6.disable=1"));
        assert!(linux_warn.contains("AF_INET6"));

        let other_warn = format_ipv6_status(false, false).unwrap();
        assert!(other_warn.contains("IPv6 loopback"));
        assert!(!other_warn.contains("WSL"));
    }

    #[test]
    fn diagnose_agy_stderr_detects_known_causes_and_sanitizes_generic_lines() {
        assert_eq!(diagnose_agy_stderr(b""), None);
        assert_eq!(diagnose_agy_stderr(b"  \n\t\x1b[31m\x1b[0m\n"), None);

        let page_err = diagnose_agy_stderr(
            b"FATAL: third_party/tcmalloc/page-size.cc:42 Check failed: page_size == 4096\n",
        )
        .unwrap();
        assert!(page_err.contains("kernel page size > 4 KiB"));
        assert!(page_err.contains("Asahi Linux"));

        let ipv6_err = diagnose_agy_stderr(
            b"startup log\nsocket(AF_INET6) failed: errno 97\n",
        )
        .unwrap();
        assert!(ipv6_err.contains("IPv6 loopback unavailable"));
        assert!(ipv6_err.contains("WSL ipv6.disable=1"));

        let af_err = diagnose_agy_stderr(b"bind [::1]: Address family not supported by protocol\n")
            .unwrap();
        assert!(af_err.contains("IPv6 loopback unavailable"));

        let generic = diagnose_agy_stderr(b"\n\x1b[31merror:\x1b[0m bad flag\x07\nsecond line\n")
            .unwrap();
        assert_eq!(generic, "error: bad flag");

        let long_line = "x".repeat(200);
        let truncated = diagnose_agy_stderr(long_line.as_bytes()).unwrap();
        assert_eq!(truncated.chars().count(), 120);
    }

    #[cfg(unix)]
    #[test]
    fn inspect_conversations_ignores_symlinked_dir_and_symlinked_dbs() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let real_conv = dir.path().join("real_conv");
        let external = dir.path().join("external.db");
        fs::create_dir_all(&real_conv).unwrap();
        fs::write(&external, b"db").unwrap();
        fs::write(
            real_conv.join("11111111-1111-4111-8111-111111111111.db"),
            b"real",
        )
        .unwrap();
        symlink(
            &external,
            real_conv.join("22222222-2222-4222-8222-222222222222.db"),
        )
        .unwrap();
        // Real .db with symlinked -wal sidecar is also excluded, matching db/stats policy.
        fs::write(
            real_conv.join("33333333-3333-4333-8333-333333333333.db"),
            b"real-with-sym-wal",
        )
        .unwrap();
        symlink(
            &external,
            real_conv.join("33333333-3333-4333-8333-333333333333.db-wal"),
        )
        .unwrap();
        assert_eq!(
            inspect_conversations_dir(&real_conv),
            "found, 1 session .db file"
        );

        let sym_conv = dir.path().join("sym_conv");
        symlink(&real_conv, &sym_conv).unwrap();
        assert_eq!(inspect_conversations_dir(&sym_conv), "symlink (ignored)");
    }

    #[test]
    fn command_executable_lookup_supports_quoted_paths_with_spaces_and_args() {
        let dir = tempfile::tempdir().unwrap();
        let space_dir = dir.path().join("my tools");
        fs::create_dir_all(&space_dir).unwrap();
        let spaced_bin = space_dir.join(FAKE_BIN_NAME);
        fs::write(&spaced_bin, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&spaced_bin, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let double_quoted = format!("\"{}\" --no-color", spaced_bin.display());
        let single_quoted = format!("'{}' --style minimal", spaced_bin.display());
        assert!(command_executable_exists_in_path(&double_quoted, None));
        assert!(command_executable_exists_in_path(&single_quoted, None));
        assert_eq!(
            extract_command_executable(r#""/tmp/a \"b\" \\c/bin" --flag"#).as_deref(),
            Some(r#"/tmp/a "b" \c/bin"#)
        );
        assert_eq!(extract_command_executable("\"/unterminated"), None);
        assert_eq!(extract_command_executable("'/unterminated"), None);
        assert_eq!(extract_command_executable("\"\" --flag"), None);
    }

    #[cfg(unix)]
    #[test]
    fn command_executable_lookup_rejects_non_executable_files_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let space_dir = dir.path().join("dir with spaces");
        fs::create_dir_all(&space_dir).unwrap();
        let plain_file = space_dir.join("not-executable");
        fs::write(&plain_file, b"data").unwrap();
        fs::set_permissions(&plain_file, fs::Permissions::from_mode(0o644)).unwrap();

        assert!(!command_executable_exists_in_path(
            &plain_file.to_string_lossy(),
            None
        ));
        assert!(!command_executable_exists_in_path(
            &format!("\"{}\" --no-color", plain_file.display()),
            None
        ));
    }

    #[cfg(unix)]
    #[test]
    fn inspect_archive_db_rejects_symlinks_and_unsafe_sidecars() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.db");
        assert_eq!(inspect_archive_db(&missing), "not found");

        let real_db = dir.path().join("real.db");
        fs::write(&real_db, b"1234567890").unwrap();
        assert_eq!(
            inspect_archive_db(&real_db),
            "found, 10 B, unreadable schema"
        );

        let sym_db = dir.path().join("sym.db");
        symlink(&real_db, &sym_db).unwrap();
        assert_eq!(
            inspect_archive_db(&sym_db),
            "symlink or unsafe sidecar (ignored)"
        );

        let wal_db = dir.path().join("wal_poisoned.db");
        fs::write(&wal_db, b"db").unwrap();
        symlink(&real_db, dir.path().join("wal_poisoned.db-wal")).unwrap();
        assert_eq!(
            inspect_archive_db(&wal_db),
            "symlink or unsafe sidecar (ignored)"
        );
    }

    #[test]
    fn inspect_archive_db_reports_schema_version_without_mutating_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("stats_history.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA user_version = 2; CREATE TABLE t (id INTEGER);")
            .unwrap();
        drop(conn);

        let bytes_before = fs::read(&db_path).unwrap();
        let entries_before = fs::read_dir(dir.path()).unwrap().count();
        let status = inspect_archive_db(&db_path);
        let bytes_after = fs::read(&db_path).unwrap();
        let entries_after = fs::read_dir(dir.path()).unwrap().count();

        assert!(status.starts_with("found, "), "status: {status}");
        assert!(status.ends_with(", schema v2"), "status: {status}");
        assert_eq!(bytes_before, bytes_after);
        assert_eq!(entries_before, entries_after);
    }

    #[cfg(unix)]
    #[test]
    fn probe_agy_version_extracts_first_line_when_output_exceeds_byte_cap() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let agy_bin = dir.path().join("agy");
        fs::write(
            &agy_bin,
            "#!/bin/sh\nprintf '1.9.9\\n'\nwhile printf '0123456789abcdef'; do :; done\n",
        )
        .unwrap();
        fs::set_permissions(&agy_bin, fs::Permissions::from_mode(0o755)).unwrap();

        let path_env = env::join_paths([dir.path()]).unwrap();
        assert_eq!(
            probe_agy_version(Some(path_env.as_os_str()), Duration::from_secs(5)),
            "1.9.9"
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_agy_version_diagnoses_page_size_and_ipv6_stderr_root_causes() {
        use std::os::unix::fs::PermissionsExt;

        let page_dir = tempfile::tempdir().unwrap();
        let page_bin = page_dir.path().join("agy");
        fs::write(
            &page_bin,
            "#!/bin/sh\nprintf 'Check failed at page-size.cc:18\\n' >&2\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&page_bin, fs::Permissions::from_mode(0o755)).unwrap();
        let page_path = env::join_paths([page_dir.path()]).unwrap();
        let page_status = probe_agy_version(Some(page_path.as_os_str()), Duration::from_secs(5));
        assert!(page_status.contains("unavailable (exit"));
        assert!(page_status.contains("kernel page size > 4 KiB"));

        let ipv6_dir = tempfile::tempdir().unwrap();
        let ipv6_bin = ipv6_dir.path().join("agy");
        fs::write(
            &ipv6_bin,
            "#!/bin/sh\nprintf 'socket AF_INET6 failed\\n' >&2\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&ipv6_bin, fs::Permissions::from_mode(0o755)).unwrap();
        let ipv6_path = env::join_paths([ipv6_dir.path()]).unwrap();
        let ipv6_status = probe_agy_version(Some(ipv6_path.as_os_str()), Duration::from_secs(5));
        assert!(ipv6_status.contains("unavailable (exit"));
        assert!(ipv6_status.contains("IPv6 loopback unavailable"));
    }

    #[test]
    fn inspect_recent_session_errors_detects_clean_poisoned_and_quota_sessions() {
        // Clean session DB.
        let clean_dir = tempfile::tempdir().unwrap();
        let clean_db = clean_dir
            .path()
            .join("11111111-1111-4111-8111-111111111111.db");
        let conn = create_steps_db(&clean_db);
        conn.execute(
            "INSERT INTO steps (idx, step_type, step_payload) VALUES (1, 1, 'ok')",
            [],
        )
        .unwrap();
        drop(conn);
        let clean_health = inspect_recent_session_errors(clean_dir.path()).unwrap();
        assert_eq!(clean_health.checked, 1);
        assert!(clean_health.poisoned_sessions.is_empty());
        assert_eq!(
            format_session_health(&clean_health),
            "checked 1 recent session [clean]"
        );

        // Poisoned tail INVALID_THINKING_SIGNATURE (stored as raw protobuf BLOB).
        let poisoned_dir = tempfile::tempdir().unwrap();
        let poisoned_db = poisoned_dir
            .path()
            .join("aaaaaaaa-1111-4111-8111-111111111111.db");
        let conn = create_steps_db(&poisoned_db);
        conn.execute(
            "INSERT INTO steps (idx, step_type, step_payload) VALUES (1, 17, ?1)",
            [b"\x0a\x20INVALID_THINKING_SIGNATURE\x10\x01".as_slice()],
        )
        .unwrap();
        drop(conn);
        let poisoned_health = inspect_recent_session_errors(poisoned_dir.path()).unwrap();
        assert_eq!(poisoned_health.poisoned_sessions, vec!["aaaaaaaa"]);
        assert_eq!(poisoned_health.historical_signature_errors, 0);
        assert_eq!(
            format_session_health(&poisoned_health),
            "checked 1 recent session [1 poisoned INVALID_THINKING_SIGNATURE (aaaaaaaa)]"
        );

        // Recovered historical INVALID_THINKING_SIGNATURE + QUOTA_EXHAUSTED + other step_type=17.
        let mixed_dir = tempfile::tempdir().unwrap();
        let mixed_db = mixed_dir
            .path()
            .join("bbbbbbbb-2222-4222-8222-222222222222.db");
        let conn = create_steps_db(&mixed_db);
        conn.execute(
            "INSERT INTO steps (idx, step_type, step_payload) VALUES (1, 17, 'INVALID_THINKING_SIGNATURE')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO steps (idx, step_type, step_payload) VALUES (2, 17, 'QUOTA_EXHAUSTED')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO steps (idx, step_type, step_payload) VALUES (3, 17, 'RESOURCE_EXHAUSTED')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO steps (idx, step_type, step_payload) VALUES (4, 17, 'INTERNAL_ERROR')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO steps (idx, step_type, step_payload) VALUES (5, 2, 'recovered ok')",
            [],
        )
        .unwrap();
        drop(conn);
        let mixed_health = inspect_recent_session_errors(mixed_dir.path()).unwrap();
        assert!(mixed_health.poisoned_sessions.is_empty());
        assert_eq!(mixed_health.historical_signature_errors, 1);
        assert_eq!(mixed_health.quota_exhausted_errors, 2);
        assert_eq!(mixed_health.other_step17_errors, 1);
        let summary = format_session_health(&mixed_health);
        assert!(summary.contains("1 recovered INVALID_THINKING_SIGNATURE"));
        assert!(summary.contains("2 QUOTA_EXHAUSTED"));
        assert!(summary.contains("1 other step_type=17"));

        // DB without a `steps` table is tolerated cleanly.
        let empty_schema_dir = tempfile::tempdir().unwrap();
        let empty_schema_db = empty_schema_dir
            .path()
            .join("cccccccc-3333-4333-8333-333333333333.db");
        let conn = Connection::open(&empty_schema_db).unwrap();
        conn.execute_batch("CREATE TABLE unrelated (id INTEGER);")
            .unwrap();
        drop(conn);
        let empty_health = inspect_recent_session_errors(empty_schema_dir.path()).unwrap();
        assert_eq!(
            format_session_health(&empty_health),
            "checked 1 recent session [clean]"
        );
    }

    #[cfg(unix)]
    #[test]
    fn inspect_recent_session_errors_skips_symlinked_dbs() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let conv = dir.path().join("conversations");
        fs::create_dir_all(&conv).unwrap();

        let external = dir.path().join("external.db");
        let conn = create_steps_db(&external);
        conn.execute(
            "INSERT INTO steps (idx, step_type, step_payload) VALUES (1, 17, 'INVALID_THINKING_SIGNATURE')",
            [],
        )
        .unwrap();
        drop(conn);

        symlink(
            &external,
            conv.join("dddddddd-4444-4444-8444-444444444444.db"),
        )
        .unwrap();
        assert_eq!(inspect_recent_session_errors(&conv), None);
    }

    #[test]
    fn inspect_pricing_cache_reports_missing_fresh_stale_and_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_700_100_000u64;
        assert_eq!(inspect_pricing_cache(dir.path(), now), "not found");

        let path = dir.path().join(MODELS_DEV_CACHE_FILE);
        fs::write(
            &path,
            format!(
                r#"{{"fetched_at": {}, "models": {{"gemini-2.5-pro": {{}}, "claude-sonnet": {{}}}}}}"#,
                now - 60
            ),
        )
        .unwrap();
        assert_eq!(
            inspect_pricing_cache(dir.path(), now),
            "found, 2 models (fresh)"
        );

        fs::write(
            &path,
            format!(
                r#"{{"fetched_at": {}, "models": {{"gemini-2.5-pro": {{}}}}}}"#,
                now - DEFAULT_PRICING_TTL_SECS - 1
            ),
        )
        .unwrap();
        assert_eq!(
            inspect_pricing_cache(dir.path(), now),
            "found, 1 models (stale)"
        );

        fs::write(&path, b"{not valid json").unwrap();
        assert!(inspect_pricing_cache(dir.path(), now).starts_with("corrupt ("));
    }

    #[cfg(unix)]
    #[test]
    fn inspect_pricing_cache_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.json");
        fs::write(&target, r#"{"fetched_at": 100, "models": {}}"#).unwrap();
        symlink(&target, dir.path().join(MODELS_DEV_CACHE_FILE)).unwrap();

        assert_eq!(inspect_pricing_cache(dir.path(), 100), "symlink (ignored)");
    }

    #[test]
    fn inspect_calibration_cache_reports_missing_valid_stale_and_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(inspect_calibration_cache(dir.path()), "not found");

        let path = dir.path().join(CALIBRATION_CACHE_FILENAME);
        fs::write(
            &path,
            r#"{
                "schema_version": 3,
                "buckets": {
                    "b1": { "calibrated_total_tokens": 500000.0 },
                    "b2": { "calibrated_total_tokens": null },
                    "b3": { "calibrated_total_tokens": 250000.0 }
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            inspect_calibration_cache(dir.path()),
            "found, schema v3, 2/3 calibrated"
        );

        fs::write(&path, r#"{"schema_version": 2, "buckets": {}}"#).unwrap();
        assert_eq!(inspect_calibration_cache(dir.path()), "stale schema v2");

        fs::write(&path, b"{broken").unwrap();
        assert!(inspect_calibration_cache(dir.path()).starts_with("corrupt ("));
    }

    #[cfg(unix)]
    #[test]
    fn inspect_calibration_cache_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target_cal.json");
        fs::write(&target, r#"{"schema_version": 3, "buckets": {}}"#).unwrap();
        symlink(&target, dir.path().join(CALIBRATION_CACHE_FILENAME)).unwrap();

        assert_eq!(inspect_calibration_cache(dir.path()), "symlink (ignored)");
    }
}
