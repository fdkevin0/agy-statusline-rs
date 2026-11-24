//! Protobuf decoder for AGY generation, step, and trajectory metadata backed by `prost`.
//!
//! Extracts turn timing, token counts, usage identities, retry attempts, model
//! metadata, timestamps, and context window metadata from
//! `CortexStepGeneratorMetadata` (`gen_metadata.data`), `CortexStepMetadata`
//! (`steps.metadata`), and `trajectory_metadata_blob.data`.
//!
//! Verified wire paths (from embedded `.proto` descriptors in `agy`):
//! - `CortexStepGeneratorMetadata`:
//!   - `chat_model` (`ChatModelMetadata`): field `1`
//!     - `model` (`Model` enum): field `3`
//!     - `usage` (`ModelUsageStats`): field `4`
//!       - `model_enum`: field `1`
//!       - `input_tokens`: field `2`
//!       - `output_tokens`: field `3`
//!       - `cache_write_tokens`: field `4`
//!       - `cache_read_tokens`: field `5`
//!       - `id_7`: field `7` (`string`)
//!       - `thinking_output_tokens`: field `9`
//!       - `response_output_tokens`: field `10`
//!       - `id_11`: field `11` (`string`)
//!       - `id_12`: field `12` (`string`)
//!     - `model_cost`: field `5` (`float` USD)
//!     - `chat_start_metadata` (`ChatStartMetadata`): field `9`
//!       - `created_at` (`google.protobuf.Timestamp`): field `4` (`seconds=1`)
//!       - `context_window_metadata` (`ContextWindowMetadata`): field `10`
//!         - `estimated_tokens_used`: field `1`
//!         - `max_context_tokens`: field `4`
//!     - `time_to_first_token` (`google.protobuf.Duration`): field `11` (`seconds=1`, `nanos=2`)
//!     - `streaming_duration` (`google.protobuf.Duration`): field `12` (`seconds=1`, `nanos=2`)
//!     - `retries` (repeated `RetryAttempt` -> `usage=2`): field `17`
//!     - `response_model`: field `19` (`string`)
//!     - `model_display_name`: field `21` (`string`)
//!   - `step_indices`: field `2` (`repeated uint32`, packed or unpacked)
//! - `CortexStepMetadata` (`steps.metadata`):
//!   - `created_at` (`google.protobuf.Timestamp`): field `1` (`seconds=1`)
//!   - `secondary_created_at` (`google.protobuf.Timestamp`): field `8` (`seconds=1`)
//!   - `usage` (`ModelUsageStats`): field `9`
//!   - `model_info` (`StepModelInfo`): field `24` (`model_enum=1`, `model_display_name=8`, `response_model=12`)
//!   - `retries` (repeated `RetryAttempt` -> `usage=2`): field `28`
//! - `TrajectoryMetadataBlob` (`trajectory_metadata_blob.data`):
//!   - `created_at` (`google.protobuf.Timestamp`): field `2` (`seconds=1`)

use prost::Message;

/// Minimum streaming decode duration (seconds) accepted as a valid turn speed sample.
const MIN_TURN_DECODE_SECS: f64 = 0.01;
/// Maximum decode throughput (tokens/second) accepted as a valid turn speed sample.
pub const MAX_TURN_SPEED_TPS: f64 = 10_000.0;
/// Maximum time-to-first-token (seconds) accepted as a valid TTFT sample.
pub const MAX_TTFT_SECS: f64 = 600.0;
/// Minimum valid `google.protobuf.Timestamp.seconds` (`0001-01-01T00:00:00Z`).
pub const MIN_TIMESTAMP_SECS: i64 = -62_135_596_800;
/// Maximum valid `google.protobuf.Timestamp.seconds` (`9999-12-31T23:59:59Z`).
pub const MAX_TIMESTAMP_SECS: i64 = 253_402_300_799;

#[derive(Clone, PartialEq, Message)]
struct GenMetadataBlob {
    #[prost(message, optional, tag = "1")]
    chat_model: Option<ChatModelMetadata>,
    #[prost(uint64, repeated, tag = "2")]
    step_indices: Vec<u64>,
}

#[derive(Clone, PartialEq, Message)]
struct RetryAttempt {
    #[prost(message, optional, tag = "2")]
    usage: Option<ModelUsage>,
}

#[derive(Clone, PartialEq, Message)]
struct ChatModelMetadata {
    #[prost(uint64, tag = "3")]
    model: u64,
    #[prost(message, optional, tag = "4")]
    usage: Option<ModelUsage>,
    #[prost(float, optional, tag = "5")]
    model_cost: Option<f32>,
    #[prost(message, optional, tag = "9")]
    chat_start_metadata: Option<ChatStartMetadata>,
    #[prost(message, optional, tag = "11")]
    time_to_first_token: Option<ProtoDuration>,
    #[prost(message, optional, tag = "12")]
    streaming_duration: Option<ProtoDuration>,
    #[prost(message, repeated, tag = "17")]
    retries: Vec<RetryAttempt>,
    #[prost(string, optional, tag = "19")]
    response_model: Option<String>,
    #[prost(string, optional, tag = "21")]
    model_display_name: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelUsage {
    #[prost(uint64, tag = "1")]
    model_enum: u64,
    #[prost(uint64, tag = "2")]
    input_tokens: u64,
    #[prost(uint64, tag = "3")]
    output_tokens: u64,
    #[prost(uint64, tag = "4")]
    cache_write_tokens: u64,
    #[prost(uint64, tag = "5")]
    cache_read_tokens: u64,
    #[prost(string, optional, tag = "7")]
    id_7: Option<String>,
    #[prost(uint64, tag = "9")]
    thinking_output_tokens: u64,
    #[prost(uint64, tag = "10")]
    response_output_tokens: u64,
    #[prost(string, optional, tag = "11")]
    id_11: Option<String>,
    #[prost(string, optional, tag = "12")]
    id_12: Option<String>,
}

impl ModelUsage {
    /// Resolves total output tokens and thinking tokens consistently when providers
    /// split thinking (`f9`) and response (`f10`) output tokens or omit `f3`.
    fn resolved_tokens(&self) -> (u64, u64) {
        let output_tokens = self.output_tokens.max(
            self.thinking_output_tokens
                .saturating_add(self.response_output_tokens),
        );
        let thinking_output_tokens = output_tokens.min(self.thinking_output_tokens);
        (output_tokens, thinking_output_tokens)
    }

    fn into_attempt_record(self) -> UsageAttemptRecord {
        let (output_tokens, thinking_output_tokens) = self.resolved_tokens();
        UsageAttemptRecord {
            model_enum: self.model_enum,
            input_tokens: self.input_tokens,
            output_tokens,
            cache_write_tokens: self.cache_write_tokens,
            cache_read_tokens: self.cache_read_tokens,
            thinking_output_tokens,
            identity: UsageIdentity {
                id_11: trim_non_empty(self.id_11),
                id_12: trim_non_empty(self.id_12),
                id_7: trim_non_empty(self.id_7),
            },
        }
    }
}

#[derive(Clone, PartialEq, Message)]
struct ChatStartMetadata {
    #[prost(message, optional, tag = "4")]
    created_at: Option<ProtoTimestamp>,
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

impl ProtoDuration {
    fn to_secs(&self) -> Option<f64> {
        if self.seconds < 0 || !(0..1_000_000_000).contains(&self.nanos) {
            return None;
        }
        Some((self.seconds as f64) + (self.nanos as f64) / 1e9)
    }
}

#[derive(Clone, PartialEq, Message)]
struct ProtoTimestamp {
    #[prost(int64, tag = "1")]
    seconds: i64,
}

impl ProtoTimestamp {
    fn valid_secs(&self) -> Option<i64> {
        (self.seconds != 0 && (MIN_TIMESTAMP_SECS..=MAX_TIMESTAMP_SECS).contains(&self.seconds))
            .then_some(self.seconds)
    }
}

#[derive(Clone, PartialEq, Message)]
struct StepModelInfo {
    #[prost(uint64, tag = "1")]
    model_enum: u64,
    #[prost(string, optional, tag = "8")]
    model_display_name: Option<String>,
    #[prost(string, optional, tag = "12")]
    response_model: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct StepMetadataBlob {
    #[prost(message, optional, tag = "1")]
    created_at: Option<ProtoTimestamp>,
    #[prost(message, optional, tag = "8")]
    secondary_created_at: Option<ProtoTimestamp>,
    #[prost(message, optional, tag = "9")]
    usage: Option<ModelUsage>,
    #[prost(message, optional, tag = "24")]
    model_info: Option<StepModelInfo>,
    #[prost(message, repeated, tag = "28")]
    retries: Vec<RetryAttempt>,
}

#[derive(Clone, PartialEq, Message)]
struct TrajectoryMetadataBlob {
    #[prost(message, optional, tag = "2")]
    created_at: Option<ProtoTimestamp>,
}

/// Deduplication identity keys observed on a `ModelUsageStats` protobuf message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageIdentity {
    /// Primary response identifier (`ModelUsageStats` field `11`).
    pub id_11: Option<String>,
    /// Secondary response/message identifier (`ModelUsageStats` field `12`).
    pub id_12: Option<String>,
    /// Fallback request/call identifier (`ModelUsageStats` field `7`).
    pub id_7: Option<String>,
}

impl UsageIdentity {
    /// Returns `true` when all identity keys are absent.
    pub fn is_empty(&self) -> bool {
        self.id_11.is_none() && self.id_12.is_none() && self.id_7.is_none()
    }
}

/// Decoded usage and identity metrics for a single model attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageAttemptRecord {
    /// Numeric `Model` enum reported on `ModelUsageStats` (field `1`).
    pub model_enum: u64,
    /// Uncached input tokens (field `2`).
    pub input_tokens: u64,
    /// Resolved total output tokens (`max(f3, f9 + f10)`).
    pub output_tokens: u64,
    /// Prompt cache write tokens (field `4`).
    pub cache_write_tokens: u64,
    /// Prompt cache read tokens (field `5`).
    pub cache_read_tokens: u64,
    /// Resolved thinking output tokens (`min(output_tokens, f9)`).
    pub thinking_output_tokens: u64,
    /// Non-empty trimmed deduplication identity keys (`f11`, `f12`, `f7`).
    pub identity: UsageIdentity,
}

/// Timing and output token metrics extracted from one `gen_metadata` turn.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct TurnMetrics {
    /// Resolved total output tokens (`max(f3, f9 + f10)`).
    pub(crate) output_tokens: u64,
    /// Optional thinking output tokens (`min(output_tokens, f9)`).
    pub(crate) thinking_tokens: Option<u64>,
    /// Streaming decode duration in seconds (`chat(1).streaming_duration(12)`).
    pub(crate) decode_secs: f64,
    /// Optional time-to-first-token in seconds (`chat(1).time_to_first_token(11)`).
    pub(crate) ttft_secs: Option<f64>,
}

/// Single-pass summary extracted from one `gen_metadata.data` Protobuf blob.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct GenMetadataSummary {
    /// Completed turn timing/output metrics when sanity bounds are satisfied.
    pub(crate) turn: Option<TurnMetrics>,
    /// Server-issued `max_context_tokens` when `> 0`.
    pub(crate) max_context_tokens: Option<u64>,
    /// Server-estimated `estimated_tokens_used` when `> 0`.
    pub(crate) estimated_context_tokens: Option<u64>,
}

/// Full generation record decoded from `CortexStepGeneratorMetadata` (`gen_metadata.data`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GenMetadataRecord {
    /// All step indices covered by this generation (`step_indices`, field `2`).
    pub step_indices: Vec<u64>,
    /// Numeric `Model` enum (prefers `usage.model_enum(1)` when non-zero, else `chat.model(3)`).
    pub model_enum: u64,
    /// Uncached input tokens (`chat(1).usage(4).input_tokens(2)`).
    pub input_tokens: u64,
    /// Resolved total output tokens (`max(f3, f9 + f10)`).
    pub output_tokens: u64,
    /// Prompt cache write tokens (`chat(1).usage(4).cache_write_tokens(4)`).
    pub cache_write_tokens: u64,
    /// Prompt cache read tokens (`chat(1).usage(4).cache_read_tokens(5)`).
    pub cache_read_tokens: u64,
    /// Resolved thinking output tokens (`min(output_tokens, f9)`).
    pub thinking_output_tokens: u64,
    /// Native server-recorded turn cost in USD (`chat(1).model_cost(5)`), if `> 0` and finite.
    pub model_cost_usd: Option<f64>,
    /// Optional chat start timestamp seconds (`chat(1).chat_start_metadata(9).created_at(4).seconds(1)`).
    pub chat_start_created_at_secs: Option<i64>,
    /// Time-to-first-token in seconds (`chat(1).time_to_first_token(11)`), if in `(0, MAX_TTFT_SECS)`.
    pub ttft_secs: Option<f64>,
    /// Streaming decode duration in seconds (`chat(1).streaming_duration(12)`).
    pub decode_secs: Option<f64>,
    /// Wire response model string (`chat(1).response_model(19)`).
    pub response_model: Option<String>,
    /// Human-readable display model name (`chat(1).model_display_name(21)`).
    pub model_display_name: Option<String>,
    /// Non-empty trimmed deduplication identity keys from `chat(1).usage(4)`.
    pub identity: UsageIdentity,
    /// Usage attempts recorded on `chat(1).retries(17)`.
    pub retry_usages: Vec<UsageAttemptRecord>,
}

impl GenMetadataRecord {
    /// Returns decode throughput in tokens/second when output tokens and duration
    /// satisfy the minimum decode duration and [`MAX_TURN_SPEED_TPS`] bounds.
    pub fn decode_speed_tps(&self) -> Option<f64> {
        let secs = self.decode_secs?;
        if !is_sane_decode_speed(self.output_tokens, secs) {
            return None;
        }
        Some((self.output_tokens as f64) / secs)
    }
}

/// Decoded step timestamps, model info, and usage attempts from `CortexStepMetadata` (`steps.metadata`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepMetadataRecord {
    /// Primary step creation timestamp (`created_at`, field `1`).
    pub created_at_secs: Option<i64>,
    /// Secondary step timestamp (`field 8`), used when primary `created_at` is absent.
    pub secondary_created_at_secs: Option<i64>,
    /// Numeric `Model` enum from `model_info(24).model_enum(1)` (falling back to `usage(9).model_enum(1)`).
    pub model_enum: u64,
    /// Wire response model string (`model_info(24).response_model(12)`).
    pub response_model: Option<String>,
    /// Human-readable display model name (`model_info(24).model_display_name(8)`).
    pub model_display_name: Option<String>,
    /// Step-level usage record (`field 9`), e.g. compaction turns (`step_type = 23`).
    pub usage: Option<UsageAttemptRecord>,
    /// Step-level retry attempts (`field 28 -> usage(2)`).
    pub retry_usages: Vec<UsageAttemptRecord>,
}

pub(crate) fn is_sane_decode_speed(output_tokens: u64, decode_secs: f64) -> bool {
    if decode_secs < MIN_TURN_DECODE_SECS || output_tokens == 0 {
        return false;
    }
    let speed = (output_tokens as f64) / decode_secs;
    speed.is_finite() && speed > 0.0 && speed < MAX_TURN_SPEED_TPS
}

fn trim_non_empty(value: Option<String>) -> Option<String> {
    let raw = value?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else if trimmed.len() == raw.len() {
        Some(raw)
    } else {
        Some(trimmed.to_string())
    }
}

/// Decodes a `gen_metadata.data` Protobuf blob once to extract turn speed/TTFT metrics
/// and server-reported context window metadata.
///
/// Enforces unified speed sanity bounds: `decode_secs >= 0.01`, `output_tokens > 0`,
/// and `0.0 < output_tokens / decode_secs < 10_000.0`.
pub(crate) fn extract_gen_metadata_summary(data: &[u8]) -> Option<GenMetadataSummary> {
    let chat = GenMetadataBlob::decode(data).ok()?.chat_model?;
    let turn = match (
        chat.usage,
        chat.streaming_duration.and_then(|d| d.to_secs()),
    ) {
        (Some(usage), Some(decode_secs)) => {
            let (output_tokens, thinking_output_tokens) = usage.resolved_tokens();
            is_sane_decode_speed(output_tokens, decode_secs).then(|| TurnMetrics {
                output_tokens,
                thinking_tokens: (thinking_output_tokens > 0).then_some(thinking_output_tokens),
                decode_secs,
                ttft_secs: chat
                    .time_to_first_token
                    .and_then(|d| d.to_secs())
                    .filter(|&t| t > 0.0 && t < MAX_TTFT_SECS),
            })
        }
        _ => None,
    };
    let ctx_window = chat
        .chat_start_metadata
        .and_then(|start| start.context_window_metadata);
    let max_context_tokens = ctx_window
        .as_ref()
        .map(|cw| cw.max_context_tokens)
        .filter(|&v| v > 0)
        .map(|v| v as u64);
    let estimated_context_tokens = ctx_window
        .map(|cw| cw.estimated_tokens_used)
        .filter(|&v| v > 0)
        .map(|v| v as u64);
    Some(GenMetadataSummary {
        turn,
        max_context_tokens,
        estimated_context_tokens,
    })
}

/// Extracts a complete [`GenMetadataRecord`] from `CortexStepGeneratorMetadata` (`gen_metadata.data`).
///
/// Returns `None` if `chat_model` (field `1`) is missing or malformed.
///
/// # Examples
///
/// ```
/// use agy_statusline::agy::protobuf::extract_gen_metadata_record;
///
/// // chat(1) -> { usage(4) -> { output(3)=100 }, duration(12) -> { seconds(1)=2 } }
/// let blob = [0x0a, 0x08, 0x22, 0x02, 0x18, 0x64, 0x62, 0x02, 0x08, 0x02];
/// let rec = extract_gen_metadata_record(&blob).unwrap();
/// assert_eq!(rec.decode_speed_tps(), Some(50.0));
/// ```
pub fn extract_gen_metadata_record(data: &[u8]) -> Option<GenMetadataRecord> {
    let blob = GenMetadataBlob::decode(data).ok()?;
    let step_indices = blob.step_indices;
    let chat = blob.chat_model?;
    let usage = chat
        .usage
        .map(ModelUsage::into_attempt_record)
        .unwrap_or_default();
    let model_enum = if usage.model_enum != 0 {
        usage.model_enum
    } else {
        chat.model
    };
    let retry_usages = chat
        .retries
        .into_iter()
        .filter_map(|r| r.usage.map(ModelUsage::into_attempt_record))
        .collect();

    let model_cost_usd = chat
        .model_cost
        .map(f64::from)
        .filter(|&cost| cost.is_finite() && cost > 0.0);

    let chat_start_created_at_secs = chat
        .chat_start_metadata
        .and_then(|start| start.created_at)
        .and_then(|ts| ts.valid_secs());

    let ttft_secs = chat
        .time_to_first_token
        .and_then(|d| d.to_secs())
        .filter(|&t| t > 0.0 && t < MAX_TTFT_SECS);

    let decode_secs = chat
        .streaming_duration
        .and_then(|d| d.to_secs())
        .filter(|&d| d > 0.0);

    Some(GenMetadataRecord {
        step_indices,
        model_enum,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        thinking_output_tokens: usage.thinking_output_tokens,
        model_cost_usd,
        chat_start_created_at_secs,
        ttft_secs,
        decode_secs,
        response_model: trim_non_empty(chat.response_model),
        model_display_name: trim_non_empty(chat.model_display_name),
        identity: usage.identity,
        retry_usages,
    })
}

/// Extracts [`StepMetadataRecord`] from `CortexStepMetadata` (`steps.metadata`).
pub fn extract_step_metadata_record(data: &[u8]) -> Option<StepMetadataRecord> {
    let blob = StepMetadataBlob::decode(data).ok()?;
    let created_at_secs = blob.created_at.and_then(|ts| ts.valid_secs());
    let secondary_created_at_secs = blob.secondary_created_at.and_then(|ts| ts.valid_secs());
    let usage = blob.usage.map(ModelUsage::into_attempt_record);
    let retry_usages = blob
        .retries
        .into_iter()
        .filter_map(|r| r.usage.map(ModelUsage::into_attempt_record))
        .collect();
    let (info_model_enum, response_model, model_display_name) = match blob.model_info {
        Some(info) => (
            info.model_enum,
            trim_non_empty(info.response_model),
            trim_non_empty(info.model_display_name),
        ),
        None => (0, None, None),
    };
    let usage_model_enum = usage.as_ref().map_or(0, |u| u.model_enum);
    let model_enum = if info_model_enum != 0 {
        info_model_enum
    } else {
        usage_model_enum
    };
    Some(StepMetadataRecord {
        created_at_secs,
        secondary_created_at_secs,
        model_enum,
        response_model,
        model_display_name,
        usage,
        retry_usages,
    })
}

/// Extracts `TrajectoryMetadataBlob.created_at` (`field 2 -> google.protobuf.Timestamp.seconds = 1`)
/// from a `trajectory_metadata_blob.data` Protobuf blob.
pub fn extract_trajectory_created_at_secs(data: &[u8]) -> Option<i64> {
    TrajectoryMetadataBlob::decode(data)
        .ok()?
        .created_at?
        .valid_secs()
}

#[cfg(test)]
#[derive(Default, Clone)]
pub(crate) struct UsageSpec<'a> {
    pub(crate) model_enum: u64,
    pub(crate) input_tok: u64,
    pub(crate) output_tok: u64,
    pub(crate) cache_write_tok: u64,
    pub(crate) cache_read_tok: u64,
    pub(crate) thinking_tok: u64,
    pub(crate) resp_out_tok: u64,
    pub(crate) id_11: Option<&'a str>,
    pub(crate) id_12: Option<&'a str>,
    pub(crate) id_7: Option<&'a str>,
}

#[cfg(test)]
impl UsageSpec<'_> {
    fn to_proto(&self) -> ModelUsage {
        ModelUsage {
            model_enum: self.model_enum,
            input_tokens: self.input_tok,
            output_tokens: self.output_tok,
            cache_write_tokens: self.cache_write_tok,
            cache_read_tokens: self.cache_read_tok,
            id_7: self.id_7.map(str::to_string),
            thinking_output_tokens: self.thinking_tok,
            response_output_tokens: self.resp_out_tok,
            id_11: self.id_11.map(str::to_string),
            id_12: self.id_12.map(str::to_string),
        }
    }
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct GenBlobSpec<'a> {
    pub(crate) step_indices: Vec<u64>,
    pub(crate) model_enum: u64,
    pub(crate) usage_model_enum: u64,
    pub(crate) resp_model: Option<&'a str>,
    pub(crate) disp_name: Option<&'a str>,
    pub(crate) model_cost: Option<f32>,
    pub(crate) input_tok: u64,
    pub(crate) output_tok: u64,
    pub(crate) cache_write_tok: u64,
    pub(crate) cache_read_tok: u64,
    pub(crate) thinking_tok: u64,
    pub(crate) resp_out_tok: u64,
    pub(crate) id_11: Option<&'a str>,
    pub(crate) id_12: Option<&'a str>,
    pub(crate) id_7: Option<&'a str>,
    pub(crate) retries: Vec<UsageSpec<'a>>,
    pub(crate) created_at_secs: Option<i64>,
    pub(crate) estimated_context_tokens: i64,
    pub(crate) max_context_tokens: i64,
    pub(crate) ttft_secs: i64,
    pub(crate) ttft_nanos: i64,
    pub(crate) dur_secs: i64,
    pub(crate) dur_nanos: i64,
}

#[cfg(test)]
pub(crate) fn encode_gen_blob(spec: GenBlobSpec<'_>) -> Vec<u8> {
    GenMetadataBlob {
        chat_model: Some(ChatModelMetadata {
            model: spec.model_enum,
            usage: Some(ModelUsage {
                model_enum: spec.usage_model_enum,
                input_tokens: spec.input_tok,
                output_tokens: spec.output_tok,
                cache_write_tokens: spec.cache_write_tok,
                cache_read_tokens: spec.cache_read_tok,
                id_7: spec.id_7.map(str::to_string),
                thinking_output_tokens: spec.thinking_tok,
                response_output_tokens: spec.resp_out_tok,
                id_11: spec.id_11.map(str::to_string),
                id_12: spec.id_12.map(str::to_string),
            }),
            model_cost: spec.model_cost,
            chat_start_metadata: Some(ChatStartMetadata {
                created_at: spec
                    .created_at_secs
                    .map(|seconds| ProtoTimestamp { seconds }),
                context_window_metadata: Some(ContextWindowMetadata {
                    estimated_tokens_used: spec.estimated_context_tokens,
                    max_context_tokens: spec.max_context_tokens,
                }),
            }),
            time_to_first_token: Some(ProtoDuration {
                seconds: spec.ttft_secs,
                nanos: spec.ttft_nanos,
            }),
            streaming_duration: Some(ProtoDuration {
                seconds: spec.dur_secs,
                nanos: spec.dur_nanos,
            }),
            retries: spec
                .retries
                .iter()
                .map(|u| RetryAttempt {
                    usage: Some(u.to_proto()),
                })
                .collect(),
            response_model: spec.resp_model.map(str::to_string),
            model_display_name: spec.disp_name.map(str::to_string),
        }),
        step_indices: spec.step_indices,
    }
    .encode_to_vec()
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct StepBlobSpec<'a> {
    pub(crate) created_at_secs: Option<i64>,
    pub(crate) secondary_created_at_secs: Option<i64>,
    pub(crate) usage: Option<UsageSpec<'a>>,
    pub(crate) model_info_enum: u64,
    pub(crate) model_display_name: Option<&'a str>,
    pub(crate) response_model: Option<&'a str>,
    pub(crate) retries: Vec<UsageSpec<'a>>,
}

#[cfg(test)]
pub(crate) fn encode_step_blob(spec: StepBlobSpec<'_>) -> Vec<u8> {
    let has_info = spec.model_info_enum != 0
        || spec.model_display_name.is_some()
        || spec.response_model.is_some();
    StepMetadataBlob {
        created_at: spec
            .created_at_secs
            .map(|seconds| ProtoTimestamp { seconds }),
        secondary_created_at: spec
            .secondary_created_at_secs
            .map(|seconds| ProtoTimestamp { seconds }),
        usage: spec.usage.map(|u| u.to_proto()),
        model_info: has_info.then(|| StepModelInfo {
            model_enum: spec.model_info_enum,
            model_display_name: spec.model_display_name.map(str::to_string),
            response_model: spec.response_model.map(str::to_string),
        }),
        retries: spec
            .retries
            .iter()
            .map(|u| RetryAttempt {
                usage: Some(u.to_proto()),
            })
            .collect(),
    }
    .encode_to_vec()
}

#[cfg(test)]
pub(crate) fn encode_step_meta_blob(created_at_secs: i64) -> Vec<u8> {
    encode_step_blob(StepBlobSpec {
        created_at_secs: Some(created_at_secs),
        ..Default::default()
    })
}

#[cfg(test)]
pub(crate) fn encode_trajectory_meta_blob(created_at_secs: i64) -> Vec<u8> {
    TrajectoryMetadataBlob {
        created_at: Some(ProtoTimestamp {
            seconds: created_at_secs,
        }),
    }
    .encode_to_vec()
}

#[cfg(test)]
pub(crate) fn encode_speed_1s_blob(output_tokens: u64) -> Vec<u8> {
    encode_gen_blob(GenBlobSpec {
        output_tok: output_tokens,
        dur_secs: 1,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_speed_valid_and_invalid() {
        let valid = encode_gen_blob(GenBlobSpec {
            output_tok: 100,
            dur_secs: 2,
            ..Default::default()
        });
        assert_eq!(
            extract_gen_metadata_record(&valid).and_then(|r| r.decode_speed_tps()),
            Some(50.0)
        );
        assert_eq!(extract_gen_metadata_record(&[0x0a, 0xff]), None);
        assert_eq!(extract_gen_metadata_record(&[]), None);
    }

    #[test]
    fn max_context_tokens_valid_and_invalid() {
        let valid = encode_gen_blob(GenBlobSpec {
            max_context_tokens: 512_000,
            ..Default::default()
        });
        assert_eq!(
            extract_gen_metadata_summary(&valid).and_then(|s| s.max_context_tokens),
            Some(512_000)
        );
        let zero = encode_gen_blob(GenBlobSpec {
            max_context_tokens: 0,
            ..Default::default()
        });
        assert_eq!(
            extract_gen_metadata_summary(&zero).and_then(|s| s.max_context_tokens),
            None
        );
        assert_eq!(extract_gen_metadata_summary(&[]), None);
    }

    #[test]
    fn turn_metrics_extracts_ttft_and_short_decode() {
        let blob = encode_gen_blob(GenBlobSpec {
            output_tok: 100,
            ttft_secs: 1,
            dur_secs: 2,
            ..Default::default()
        });
        let m = extract_gen_metadata_summary(&blob).unwrap().turn.unwrap();
        assert_eq!(m.output_tokens, 100);
        assert_eq!(m.thinking_tokens, None);
        assert!((m.decode_secs - 2.0).abs() < 1e-9);
        assert!((m.ttft_secs.unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn turn_metrics_extracts_thinking_tokens() {
        let blob = encode_gen_blob(GenBlobSpec {
            output_tok: 350,
            thinking_tok: 250,
            dur_secs: 2,
            ..Default::default()
        });
        let m = extract_gen_metadata_summary(&blob).unwrap().turn.unwrap();
        assert_eq!(m.output_tokens, 350);
        assert_eq!(m.thinking_tokens, Some(250));
        assert!((m.decode_secs - 2.0).abs() < 1e-9);
    }

    #[test]
    fn resolved_output_uses_max_of_f3_and_f9_plus_f10() {
        let split_only = encode_gen_blob(GenBlobSpec {
            output_tok: 0,
            thinking_tok: 40,
            resp_out_tok: 60,
            dur_secs: 2,
            ..Default::default()
        });
        let summary = extract_gen_metadata_summary(&split_only)
            .unwrap()
            .turn
            .unwrap();
        assert_eq!(summary.output_tokens, 100);
        assert_eq!(summary.thinking_tokens, Some(40));

        let rec = extract_gen_metadata_record(&split_only).unwrap();
        assert_eq!(rec.output_tokens, 100);
        assert_eq!(rec.thinking_output_tokens, 40);
        assert_eq!(rec.decode_speed_tps(), Some(50.0));

        let larger_f3 = encode_gen_blob(GenBlobSpec {
            output_tok: 150,
            thinking_tok: 40,
            resp_out_tok: 60,
            dur_secs: 2,
            ..Default::default()
        });
        let rec_larger = extract_gen_metadata_record(&larger_f3).unwrap();
        assert_eq!(rec_larger.output_tokens, 150);
        assert_eq!(rec_larger.thinking_output_tokens, 40);
    }

    #[test]
    fn estimated_context_tokens_valid_and_invalid() {
        let valid = encode_gen_blob(GenBlobSpec {
            estimated_context_tokens: 64_000,
            max_context_tokens: 512_000,
            ..Default::default()
        });
        let summary = extract_gen_metadata_summary(&valid).unwrap();
        assert_eq!(summary.estimated_context_tokens, Some(64_000));
        assert_eq!(summary.max_context_tokens, Some(512_000));
        assert_eq!(summary.turn, None);

        let zero = encode_gen_blob(GenBlobSpec {
            estimated_context_tokens: 0,
            ..Default::default()
        });
        assert_eq!(
            extract_gen_metadata_summary(&zero).and_then(|s| s.estimated_context_tokens),
            None
        );
        assert_eq!(extract_gen_metadata_summary(&[]), None);
    }

    #[test]
    fn unified_speed_sanity_bounds() {
        // decode_secs = 0.005s (5_000_000 nanos) -> rejected (< 0.01s)
        let too_short = encode_gen_blob(GenBlobSpec {
            output_tok: 10,
            dur_nanos: 5_000_000,
            ..Default::default()
        });
        assert_eq!(
            extract_gen_metadata_summary(&too_short).and_then(|s| s.turn),
            None
        );
        assert_eq!(
            extract_gen_metadata_record(&too_short).and_then(|r| r.decode_speed_tps()),
            None
        );

        // decode_secs = 0.05s (50_000_000 nanos) -> accepted (10 / 0.05 = 200.0 tok/s)
        let fast_50ms = encode_gen_blob(GenBlobSpec {
            output_tok: 10,
            dur_nanos: 50_000_000,
            ..Default::default()
        });
        assert!(
            (extract_gen_metadata_record(&fast_50ms)
                .and_then(|r| r.decode_speed_tps())
                .unwrap()
                - 200.0)
                .abs()
                < 1e-6
        );

        // output = 1000, decode_secs = 1s -> 1000.0 tok/s -> accepted (< 10_000.0)
        let fast_1000 = encode_speed_1s_blob(1_000);
        assert_eq!(
            extract_gen_metadata_record(&fast_1000).and_then(|r| r.decode_speed_tps()),
            Some(1000.0)
        );

        // output = 10_000, decode_secs = 1s -> 10_000.0 tok/s -> rejected (>= 10_000.0)
        let too_fast = encode_speed_1s_blob(10_000);
        assert_eq!(
            extract_gen_metadata_summary(&too_fast).and_then(|s| s.turn),
            None
        );
        assert_eq!(
            extract_gen_metadata_record(&too_fast).and_then(|r| r.decode_speed_tps()),
            None
        );
    }

    #[test]
    fn gen_metadata_record_decodes_full_schema_and_packed_or_unpacked_steps() {
        let root_packed = encode_gen_blob(GenBlobSpec {
            step_indices: vec![7, 8],
            model_enum: 1526,
            resp_model: Some("model-alpha-v1"),
            disp_name: Some("Model Alpha (Medium)"),
            model_cost: Some(0.0425),
            input_tok: 12_000,
            output_tok: 600,
            cache_write_tok: 250,
            cache_read_tok: 48_000,
            thinking_tok: 400,
            ttft_nanos: 500_000_000,
            dur_secs: 2,
            ..Default::default()
        });

        let rec = extract_gen_metadata_record(&root_packed).unwrap();
        assert_eq!(rec.step_indices, vec![7, 8]);
        assert_eq!(rec.model_enum, 1526);
        assert_eq!(rec.input_tokens, 12_000);
        assert_eq!(rec.output_tokens, 600);
        assert_eq!(rec.cache_write_tokens, 250);
        assert_eq!(rec.cache_read_tokens, 48_000);
        assert_eq!(rec.thinking_output_tokens, 400);
        assert!((rec.model_cost_usd.unwrap() - 0.0425).abs() < 1e-6);
        assert!((rec.ttft_secs.unwrap() - 0.5).abs() < 1e-6);
        assert!((rec.decode_secs.unwrap() - 2.0).abs() < 1e-6);
        assert!((rec.decode_speed_tps().unwrap() - 300.0).abs() < 1e-6);
        assert_eq!(rec.response_model.as_deref(), Some("model-alpha-v1"));
        assert_eq!(
            rec.model_display_name.as_deref(),
            Some("Model Alpha (Medium)")
        );
        assert!(rec.identity.is_empty());
        assert!(rec.retry_usages.is_empty());

        // Wire-compatibility: unpacked repeated step_indices = [14, 15] (prost emits packed by default).
        // chat(1) -> { model(3)=1 }, step_indices(2)=14, step_indices(2)=15
        let root_unpacked: &[u8] = &[0x0a, 0x02, 0x18, 0x01, 0x10, 0x0e, 0x10, 0x0f];
        let rec_unpacked = extract_gen_metadata_record(root_unpacked).unwrap();
        assert_eq!(rec_unpacked.step_indices, vec![14, 15]);
        assert_eq!(rec_unpacked.model_enum, 1);
    }

    #[test]
    fn gen_metadata_decodes_usage_model_enum_identities_and_f17_retries() {
        let blob = encode_gen_blob(GenBlobSpec {
            step_indices: vec![3, 4],
            model_enum: 1500,
            usage_model_enum: 1669,
            resp_model: Some("gemini-flash-worker-agy"),
            disp_name: Some("Gemini 4 Hydrogen (Orchestrator)"),
            input_tok: 5_000,
            output_tok: 200,
            id_11: Some("  resp-main-11  "),
            id_12: Some("   "),
            id_7: Some("call-main-7"),
            retries: vec![UsageSpec {
                model_enum: 1418,
                input_tok: 2_000,
                output_tok: 0,
                thinking_tok: 30,
                resp_out_tok: 50,
                id_11: Some("resp-retry-11"),
                id_12: Some("msg-retry-12"),
                ..Default::default()
            }],
            dur_secs: 2,
            ..Default::default()
        });

        let rec = extract_gen_metadata_record(&blob).unwrap();
        assert_eq!(rec.model_enum, 1669);
        assert_eq!(rec.identity.id_11.as_deref(), Some("resp-main-11"));
        assert_eq!(rec.identity.id_12, None);
        assert_eq!(rec.identity.id_7.as_deref(), Some("call-main-7"));
        assert!(!rec.identity.is_empty());
        assert_eq!(rec.retry_usages.len(), 1);
        assert_eq!(rec.retry_usages[0].model_enum, 1418);
        assert_eq!(rec.retry_usages[0].input_tokens, 2_000);
        assert_eq!(rec.retry_usages[0].output_tokens, 80);
        assert_eq!(rec.retry_usages[0].thinking_output_tokens, 30);
        assert_eq!(
            rec.retry_usages[0].identity.id_11.as_deref(),
            Some("resp-retry-11")
        );
        assert_eq!(
            rec.retry_usages[0].identity.id_12.as_deref(),
            Some("msg-retry-12")
        );
    }

    #[test]
    fn step_metadata_decodes_f1_f8_f9_f24_f28_and_trajectory_f2() {
        let step_blob = encode_step_blob(StepBlobSpec {
            created_at_secs: None,
            secondary_created_at_secs: Some(1_795_000_050),
            usage: Some(UsageSpec {
                model_enum: 1669,
                input_tok: 8_000,
                cache_read_tok: 12_000,
                output_tok: 0,
                thinking_tok: 25,
                resp_out_tok: 75,
                id_11: Some("step-id-11"),
                id_7: Some("step-id-7"),
                ..Default::default()
            }),
            model_info_enum: 1669,
            model_display_name: Some("Gemini 4 Hydrogen (Orchestrator)"),
            response_model: Some("gemini-flash-worker-agy"),
            retries: vec![UsageSpec {
                model_enum: 1418,
                input_tok: 1_500,
                output_tok: 40,
                id_12: Some("step-retry-12"),
                ..Default::default()
            }],
        });

        let step = extract_step_metadata_record(&step_blob).unwrap();
        assert_eq!(step.created_at_secs, None);
        assert_eq!(step.secondary_created_at_secs, Some(1_795_000_050));
        assert_eq!(step.model_enum, 1669);
        assert_eq!(
            step.model_display_name.as_deref(),
            Some("Gemini 4 Hydrogen (Orchestrator)")
        );
        assert_eq!(
            step.response_model.as_deref(),
            Some("gemini-flash-worker-agy")
        );
        let usage = step.usage.unwrap();
        assert_eq!(usage.input_tokens, 8_000);
        assert_eq!(usage.cache_read_tokens, 12_000);
        assert_eq!(usage.output_tokens, 100);
        assert_eq!(usage.thinking_output_tokens, 25);
        assert_eq!(usage.identity.id_11.as_deref(), Some("step-id-11"));
        assert_eq!(usage.identity.id_7.as_deref(), Some("step-id-7"));
        assert_eq!(step.retry_usages.len(), 1);
        assert_eq!(step.retry_usages[0].model_enum, 1418);
        assert_eq!(step.retry_usages[0].output_tokens, 40);
        assert_eq!(
            step.retry_usages[0].identity.id_12.as_deref(),
            Some("step-retry-12")
        );

        let traj_valid = encode_trajectory_meta_blob(1_795_000_000);
        assert_eq!(
            extract_trajectory_created_at_secs(&traj_valid),
            Some(1_795_000_000)
        );
        let traj_oob = encode_trajectory_meta_blob(253_402_300_800);
        assert_eq!(extract_trajectory_created_at_secs(&traj_oob), None);
    }

    #[test]
    fn step_created_at_secs_rejects_out_of_range_years() {
        assert_eq!(
            extract_step_metadata_record(&encode_step_meta_blob(-62_135_596_800))
                .and_then(|r| r.created_at_secs),
            Some(-62_135_596_800)
        );
        assert_eq!(
            extract_step_metadata_record(&encode_step_meta_blob(253_402_300_799))
                .and_then(|r| r.created_at_secs),
            Some(253_402_300_799)
        );
        assert_eq!(
            extract_step_metadata_record(&encode_step_meta_blob(-62_135_596_801))
                .and_then(|r| r.created_at_secs),
            None
        );
        assert_eq!(
            extract_step_metadata_record(&encode_step_meta_blob(253_402_300_800))
                .and_then(|r| r.created_at_secs),
            None
        );
    }

    #[test]
    fn duration_rejects_negative_seconds_and_invalid_nanos() {
        let neg_secs = ProtoDuration {
            seconds: -1,
            nanos: 0,
        }
        .encode_to_vec();
        assert_eq!(
            ProtoDuration::decode(&neg_secs[..]).unwrap().to_secs(),
            None
        );

        let neg_nanos = ProtoDuration {
            seconds: 1,
            nanos: -1,
        }
        .encode_to_vec();
        assert_eq!(
            ProtoDuration::decode(&neg_nanos[..]).unwrap().to_secs(),
            None
        );

        let huge_nanos = ProtoDuration {
            seconds: 1,
            nanos: 1_000_000_000,
        }
        .encode_to_vec();
        assert_eq!(
            ProtoDuration::decode(&huge_nanos[..]).unwrap().to_secs(),
            None
        );

        let root = encode_gen_blob(GenBlobSpec {
            dur_secs: -1,
            ..Default::default()
        });
        assert_eq!(
            extract_gen_metadata_record(&root).unwrap().decode_secs,
            None
        );
    }

    #[test]
    fn chat_start_created_at_rejects_out_of_range_timestamps() {
        let decode_chat_start = |secs: i64| -> Option<i64> {
            let root = encode_gen_blob(GenBlobSpec {
                created_at_secs: Some(secs),
                ..Default::default()
            });
            extract_gen_metadata_record(&root)
                .unwrap()
                .chat_start_created_at_secs
        };

        assert_eq!(decode_chat_start(1_763_400_000), Some(1_763_400_000));
        assert_eq!(decode_chat_start(-62_135_596_801), None);
        assert_eq!(decode_chat_start(253_402_300_800), None);
    }
}
