#![forbid(unsafe_code)]
#![allow(dead_code)]
//! Shared test fixtures and isolated CLI runners for integration tests.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use prost::Message;
use unicode_width::UnicodeWidthStr;

pub const BIN: &str = env!("CARGO_BIN_EXE_agy-statusline");

pub use agy_statusline::unix_now_secs;

/// Seeds a fresh empty `models_dev_pricing.json` under `cache_dir/agy-statusline` when absent
/// so offline CLI tests never trigger detached background `pricing sync` child processes while
/// preserving existing fixtures.
pub fn seed_offline_cache_stamps_if_missing(cache_dir: &Path) {
    let app_cache = cache_dir.join("agy-statusline");
    fs::create_dir_all(&app_cache).unwrap();
    let now_secs = unix_now_secs();
    let payload = format!("{{\"fetched_at\":{now_secs},\"models\":{{}}}}");
    let _ = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(app_cache.join("models_dev_pricing.json"))
        .and_then(|mut f| f.write_all(payload.as_bytes()));
}

/// Returns a `Command` for `program` isolated inside `dir` with a cleared environment,
/// `XDG_CONFIG_HOME = dir/config`, `XDG_CACHE_HOME = dir/cache`,
/// `ANTIGRAVITY_CLI_HOME = dir/.gemini/antigravity-cli`, `TZ = UTC`,
/// and an offline pricing cache pre-seeded if missing.
pub fn isolated_cmd(program: impl AsRef<Path>, dir: &Path) -> Command {
    let xdg_cache = dir.join("cache");
    seed_offline_cache_stamps_if_missing(&xdg_cache);

    let mut cmd = Command::new(program.as_ref());
    cmd.current_dir(dir)
        .env_clear()
        .env("HOME", dir)
        .env("USERPROFILE", dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_CACHE_HOME", xdg_cache)
        .env("ANTIGRAVITY_CLI_HOME", dir.join(".gemini/antigravity-cli"))
        .env("TZ", "UTC");
    cmd
}

/// Returns an isolated `Command` for the compiled `agy-statusline` binary inside `dir`.
pub fn isolated_agy_cmd(dir: &Path) -> Command {
    isolated_cmd(BIN, dir)
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

#[derive(Clone, PartialEq, Message)]
struct GenMetadataBlob {
    #[prost(message, optional, tag = "1")]
    chat_model: Option<ChatModelMetadata>,
    #[prost(uint64, repeated, tag = "2")]
    step_indices: Vec<u64>,
}

#[derive(Clone, PartialEq, Message)]
struct RetryAttempt {
    #[prost(string, optional, tag = "3")]
    error: Option<String>,
    #[prost(uint32, tag = "5")]
    reason_code: u32,
    #[prost(message, optional, tag = "7")]
    retry_delay: Option<ProtoDuration>,
}

#[derive(Clone, PartialEq, Message)]
struct ChatModelMetadata {
    #[prost(uint64, tag = "3")]
    model: u64,
    #[prost(message, optional, tag = "4")]
    usage: Option<ModelUsage>,
    #[prost(message, optional, tag = "9")]
    chat_start_metadata: Option<ChatStartMetadata>,
    #[prost(message, optional, tag = "11")]
    time_to_first_token: Option<ProtoDuration>,
    #[prost(message, optional, tag = "12")]
    streaming_duration: Option<ProtoDuration>,
    #[prost(message, repeated, tag = "17")]
    retries: Vec<RetryAttempt>,
    #[prost(string, tag = "19")]
    response_model: String,
    #[prost(string, tag = "21")]
    model_display_name: String,
}

#[derive(Clone, PartialEq, Message)]
struct ModelUsage {
    #[prost(uint64, tag = "2")]
    input_tokens: u64,
    #[prost(uint64, tag = "3")]
    output_tokens: u64,
    #[prost(uint64, tag = "5")]
    cache_read_tokens: u64,
    #[prost(uint64, tag = "9")]
    thinking_output_tokens: u64,
    #[prost(uint64, tag = "10")]
    response_output_tokens: u64,
}

#[derive(Clone, PartialEq, Message)]
struct ChatStartMetadata {
    #[prost(message, optional, tag = "10")]
    context_window_metadata: Option<ContextWindowMetadata>,
}

#[derive(Clone, PartialEq, Message)]
struct ContextWindowMetadata {
    #[prost(int64, tag = "1")]
    estimated_tokens_used: i64,
    #[prost(int64, tag = "4")]
    max_context_tokens: i64,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoDuration {
    #[prost(int64, tag = "1")]
    seconds: i64,
    #[prost(int64, tag = "2")]
    nanos: i64,
}

#[derive(Clone, PartialEq, Message)]
struct ProtoTimestamp {
    #[prost(int64, tag = "1")]
    seconds: i64,
}

#[derive(Clone, PartialEq, Message)]
struct StepMetadataBlob {
    #[prost(message, optional, tag = "1")]
    created_at: Option<ProtoTimestamp>,
}

pub fn build_step_meta_blob(secs: u64) -> Vec<u8> {
    StepMetadataBlob {
        created_at: Some(ProtoTimestamp {
            seconds: secs as i64,
        }),
    }
    .encode_to_vec()
}

#[derive(Debug, Clone, Copy, Default)]
pub struct TestGenSpec<'a> {
    pub step_idx: u64,
    pub model_enum: u64,
    pub resp_model: &'a str,
    pub disp_name: &'a str,
    pub input_tok: u64,
    pub cache_read_tok: u64,
    pub output_tok: u64,
    pub thinking_tok: u64,
    pub resp_out_tok: u64,
    pub max_context_tokens: i64,
    pub estimated_context_tokens: i64,
    pub dur_secs: u64,
    pub ttft_ms: u64,
    pub retry_error: Option<&'a str>,
    pub retry_delay_secs: i64,
}

pub fn build_gen_blob(spec: TestGenSpec<'_>) -> Vec<u8> {
    let retries = spec
        .retry_error
        .map(|err| RetryAttempt {
            error: Some(err.to_owned()),
            reason_code: 1,
            retry_delay: (spec.retry_delay_secs > 0).then_some(ProtoDuration {
                seconds: spec.retry_delay_secs,
                nanos: 0,
            }),
        })
        .into_iter()
        .collect();
    GenMetadataBlob {
        chat_model: Some(ChatModelMetadata {
            model: spec.model_enum,
            usage: Some(ModelUsage {
                input_tokens: spec.input_tok,
                output_tokens: spec.output_tok,
                cache_read_tokens: spec.cache_read_tok,
                thinking_output_tokens: spec.thinking_tok,
                response_output_tokens: spec.resp_out_tok,
            }),
            chat_start_metadata: Some(ChatStartMetadata {
                context_window_metadata: Some(ContextWindowMetadata {
                    estimated_tokens_used: spec.estimated_context_tokens,
                    max_context_tokens: spec.max_context_tokens,
                }),
            }),
            time_to_first_token: Some(ProtoDuration {
                seconds: (spec.ttft_ms / 1_000) as i64,
                nanos: ((spec.ttft_ms % 1_000) * 1_000_000) as i64,
            }),
            streaming_duration: Some(ProtoDuration {
                seconds: spec.dur_secs as i64,
                nanos: 0,
            }),
            retries,
            response_model: spec.resp_model.to_owned(),
            model_display_name: spec.disp_name.to_owned(),
        }),
        step_indices: vec![spec.step_idx],
    }
    .encode_to_vec()
}
