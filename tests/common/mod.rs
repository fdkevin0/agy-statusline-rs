#![forbid(unsafe_code)]
#![allow(dead_code)]
//! Shared test fixtures and isolated CLI runners for integration tests.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use unicode_width::UnicodeWidthStr;

pub const BIN: &str = env!("CARGO_BIN_EXE_agy-statusline");

pub fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Seeds a fresh empty `models_dev_pricing.json` under `cache_dir/agy-statusline`
/// so offline CLI tests never trigger `SyncMode::SpawnIfStale` / `curl`.
pub fn seed_offline_pricing(cache_dir: &Path) {
    let app_cache = cache_dir.join("agy-statusline");
    fs::create_dir_all(&app_cache).unwrap();
    let now_secs = unix_now_secs();
    fs::write(
        app_cache.join("models_dev_pricing.json"),
        format!("{{\"fetched_at\":{now_secs},\"models\":{{}}}}"),
    )
    .unwrap();
}

/// Returns a `Command` for `agy-statusline` isolated inside `dir` with
/// `XDG_CONFIG_HOME = dir/config`, `XDG_CACHE_HOME = dir/cache`,
/// `ANTIGRAVITY_CLI_HOME = dir/.gemini/antigravity-cli`, and offline pricing pre-seeded.
pub fn isolated_agy_cmd(dir: &Path) -> Command {
    let xdg_cache = dir.join("cache");
    seed_offline_pricing(&xdg_cache);

    let mut cmd = Command::new(BIN);
    cmd.env("HOME", dir)
        .env("USERPROFILE", dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_CACHE_HOME", xdg_cache)
        .env("ANTIGRAVITY_CLI_HOME", dir.join(".gemini/antigravity-cli"))
        .env_remove("AGY_HOME")
        .env_remove("AGY_STATUSLINE_FULL")
        .env_remove("NO_COLOR");
    cmd
}

/// Runs `agy-statusline` with `NO_COLOR=1` and `payload` piped to stdin, returning stdout.
pub fn run_statusline_stdin(dir: &Path, args: &[&str], payload: &str) -> String {
    let mut child = isolated_agy_cmd(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// Returns the visible terminal width of the first line of `rendered` after stripping ANSI escapes.
pub fn visible_line_width(rendered: &str) -> usize {
    rendered
        .lines()
        .next()
        .map_or(0, |line| strip_ansi_escapes::strip_str(line).width())
}

pub fn push_varint(buf: &mut Vec<u8>, mut val: u64) {
    loop {
        let mut byte = (val & 0x7f) as u8;
        val >>= 7;
        if val != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if val == 0 {
            break;
        }
    }
}

pub fn push_field_varint(buf: &mut Vec<u8>, id: u64, val: u64) {
    push_varint(buf, id << 3);
    push_varint(buf, val);
}

pub fn push_field_bytes(buf: &mut Vec<u8>, id: u64, bytes: &[u8]) {
    push_varint(buf, (id << 3) | 2);
    push_varint(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

pub fn build_step_meta_blob(secs: u64) -> Vec<u8> {
    let mut ts = Vec::new();
    push_field_varint(&mut ts, 1, secs);
    let mut step = Vec::new();
    push_field_bytes(&mut step, 1, &ts);
    step
}

pub struct TestGenSpec<'a> {
    pub step_idx: u64,
    pub model_enum: u64,
    pub resp_model: &'a str,
    pub disp_name: &'a str,
    pub input_tok: u64,
    pub cache_read_tok: u64,
    pub output_tok: u64,
    pub thinking_tok: u64,
    pub dur_secs: u64,
    pub ttft_ms: u64,
}

pub fn build_gen_blob(spec: TestGenSpec<'_>) -> Vec<u8> {
    let mut usage = Vec::new();
    push_field_varint(&mut usage, 2, spec.input_tok);
    push_field_varint(&mut usage, 3, spec.output_tok);
    push_field_varint(&mut usage, 5, spec.cache_read_tok);
    push_field_varint(&mut usage, 9, spec.thinking_tok);

    let mut ttft = Vec::new();
    push_field_varint(&mut ttft, 1, spec.ttft_ms / 1_000);
    push_field_varint(&mut ttft, 2, (spec.ttft_ms % 1_000) * 1_000_000);

    let mut dur = Vec::new();
    push_field_varint(&mut dur, 1, spec.dur_secs);

    let mut chat = Vec::new();
    push_field_varint(&mut chat, 3, spec.model_enum);
    push_field_bytes(&mut chat, 4, &usage);
    push_field_bytes(&mut chat, 11, &ttft);
    push_field_bytes(&mut chat, 12, &dur);
    push_field_bytes(&mut chat, 19, spec.resp_model.as_bytes());
    push_field_bytes(&mut chat, 21, spec.disp_name.as_bytes());

    let mut packed = Vec::new();
    push_varint(&mut packed, spec.step_idx);

    let mut root = Vec::new();
    push_field_bytes(&mut root, 1, &chat);
    push_field_bytes(&mut root, 2, &packed);
    root
}

pub fn speed_1s_blob(output_tokens: u8) -> [u8; 10] {
    [
        0x0a,
        0x08,
        0x22,
        0x02,
        0x18,
        output_tokens,
        0x62,
        0x02,
        0x08,
        0x01,
    ]
}
