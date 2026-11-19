//! Protobuf decoder for AGY generation and step metadata backed by `prost`.
//!
//! Extracts turn timing, token counts, model metadata, timestamps, and context
//! window metadata from `CortexStepGeneratorMetadata` (`gen_metadata.data`) and
//! `CortexStepMetadata` (`steps.metadata`).
//!
//! Verified wire paths (from embedded `.proto` descriptors in `agy`):
//! - `CortexStepGeneratorMetadata`:
//!   - `chat_model` (`ChatModelMetadata`): field `1`
//!     - `model` (`Model` enum): field `3`
//!     - `usage` (`ModelUsageStats`): field `4`
//!       - `input_tokens`: field `2`
//!       - `output_tokens`: field `3`
//!       - `cache_write_tokens`: field `4`
//!       - `cache_read_tokens`: field `5`
//!       - `thinking_output_tokens`: field `9`
//!     - `model_cost`: field `5` (`float` USD)
//!     - `chat_start_metadata` (`ChatStartMetadata`): field `9`
//!       - `created_at` (`google.protobuf.Timestamp`): field `4` (`seconds=1`)
//!       - `context_window_metadata` (`ContextWindowMetadata`): field `10`
//!         - `estimated_tokens_used`: field `1`
//!         - `max_context_tokens`: field `4`
//!     - `time_to_first_token` (`google.protobuf.Duration`): field `11` (`seconds=1`, `nanos=2`)
//!     - `streaming_duration` (`google.protobuf.Duration`): field `12` (`seconds=1`, `nanos=2`)
//!     - `response_model`: field `19` (`string`)
//!     - `model_display_name`: field `21` (`string`)
//!   - `step_indices`: field `2` (`repeated uint32`, packed or unpacked)
//! - `CortexStepMetadata` (`steps.metadata`):
//!   - `created_at` (`google.protobuf.Timestamp`): field `1` (`seconds=1`)

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
    #[prost(string, optional, tag = "19")]
    response_model: Option<String>,
    #[prost(string, optional, tag = "21")]
    model_display_name: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
struct ModelUsage {
    #[prost(uint64, tag = "2")]
    input_tokens: u64,
    #[prost(uint64, tag = "3")]
    output_tokens: u64,
    #[prost(uint64, tag = "4")]
    cache_write_tokens: u64,
    #[prost(uint64, tag = "5")]
    cache_read_tokens: u64,
    #[prost(uint64, tag = "9")]
    thinking_output_tokens: u64,
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
struct StepMetadataBlob {
    #[prost(message, optional, tag = "1")]
    created_at: Option<ProtoTimestamp>,
}

/// Timing and output token metrics extracted from one `gen_metadata` turn.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct TurnMetrics {
    /// Total output tokens generated in the turn (`chat(1).usage(4).output_tokens(3)`).
    pub(crate) output_tokens: u64,
    /// Optional thinking output tokens (`chat(1).usage(4).thinking_output_tokens(9)`).
    pub(crate) thinking_tokens: Option<u64>,
    /// Streaming decode duration in seconds (`chat(1).streaming_duration(12)`).
    pub(crate) decode_secs: f64,
    /// Optional time-to-first-token in seconds (`chat(1).time_to_first_token(11)`).
    pub(crate) ttft_secs: Option<f64>,
}

/// Full generation record decoded from `CortexStepGeneratorMetadata` (`gen_metadata.data`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GenMetadataRecord {
    /// First step index associated with this generation (`step_indices[0]`, field `2`).
    pub first_step_index: Option<u64>,
    /// Numeric `Model` enum (`chat(1).model(3)`).
    pub model_enum: u64,
    /// Uncached input tokens (`chat(1).usage(4).input_tokens(2)`).
    pub input_tokens: u64,
    /// Total output tokens (`chat(1).usage(4).output_tokens(3)`).
    pub output_tokens: u64,
    /// Prompt cache write tokens (`chat(1).usage(4).cache_write_tokens(4)`).
    pub cache_write_tokens: u64,
    /// Prompt cache read tokens (`chat(1).usage(4).cache_read_tokens(5)`).
    pub cache_read_tokens: u64,
    /// Thinking output tokens (`chat(1).usage(4).thinking_output_tokens(9)`).
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

/// Extracts [`TurnMetrics`] (output tokens, optional thinking tokens, decode duration,
/// and optional TTFT) from a `gen_metadata.data` Protobuf blob.
///
/// Enforces unified sanity bounds: `decode_secs >= 0.01`, `output_tokens > 0`,
/// and `0.0 < output_tokens / decode_secs < 10_000.0`.
pub(crate) fn extract_turn_metrics(data: &[u8]) -> Option<TurnMetrics> {
    let blob = GenMetadataBlob::decode(data).ok()?;
    let chat = blob.chat_model?;
    let usage = chat.usage?;
    let decode_secs = chat.streaming_duration?.to_secs()?;
    let output_tokens = usage.output_tokens;
    if !is_sane_decode_speed(output_tokens, decode_secs) {
        return None;
    }
    let thinking_tokens = (usage.thinking_output_tokens > 0).then_some(usage.thinking_output_tokens);
    let ttft_secs = chat
        .time_to_first_token
        .and_then(|d| d.to_secs())
        .filter(|&t| t > 0.0 && t < MAX_TTFT_SECS);
    Some(TurnMetrics {
        output_tokens,
        thinking_tokens,
        decode_secs,
        ttft_secs,
    })
}

/// Extracts a complete [`GenMetadataRecord`] from `CortexStepGeneratorMetadata` (`gen_metadata.data`).
///
/// Returns `None` if `chat_model` (field `1`) is missing or malformed.
pub fn extract_gen_metadata_record(data: &[u8]) -> Option<GenMetadataRecord> {
    let blob = GenMetadataBlob::decode(data).ok()?;
    let first_step_index = blob.step_indices.first().copied();
    let chat = blob.chat_model?;
    let usage = chat.usage.unwrap_or_default();

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
        .filter(|&d| d.is_finite() && d > 0.0);

    Some(GenMetadataRecord {
        first_step_index,
        model_enum: chat.model,
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
    })
}

/// Extracts `CortexStepMetadata.created_at` (`field 1 -> google.protobuf.Timestamp.seconds = 1`)
/// from a `steps.metadata` Protobuf blob.
///
/// Rejects timestamps outside proleptic UTC years `0001..=9999`
/// (`-62_135_596_800..=253_402_300_799`).
pub fn extract_step_created_at_secs(data: &[u8]) -> Option<i64> {
    StepMetadataBlob::decode(data)
        .ok()?
        .created_at?
        .valid_secs()
}

/// Extracts output generation speed (tokens/second) from a `gen_metadata` Protobuf blob.
///
/// Returns `None` if required nested fields are missing, `nanos >= 1_000_000_000`,
/// `seconds < 0.01`, `output == 0`, or the resulting speed is not in `(0.0, 10_000.0)`.
///
/// # Examples
///
/// ```
/// use agy_statusline::protobuf::extract_metadata_speed;
///
/// // chat(1) -> { usage(4) -> { output(3)=100 }, duration(12) -> { seconds(1)=2 } }
/// let blob = [0x0a, 0x08, 0x22, 0x02, 0x18, 0x64, 0x62, 0x02, 0x08, 0x02];
/// assert_eq!(extract_metadata_speed(&blob), Some(50.0));
/// ```
pub fn extract_metadata_speed(data: &[u8]) -> Option<f64> {
    let metrics = extract_turn_metrics(data)?;
    Some((metrics.output_tokens as f64) / metrics.decode_secs)
}

fn extract_ctx_window(data: &[u8]) -> Option<ContextWindowMetadata> {
    GenMetadataBlob::decode(data)
        .ok()?
        .chat_model?
        .chat_start_metadata?
        .context_window_metadata
}

/// Extracts server-issued `max_context_tokens` from `gen_metadata.data`
/// along verified wire path `chat(1) -> chat_start_metadata(9) -> context_window_metadata(10) -> max_context_tokens(4)`.
///
/// Returns `None` if any nested field is missing, malformed, or `<= 0`.
pub(crate) fn extract_max_context_tokens(data: &[u8]) -> Option<u64> {
    let v = extract_ctx_window(data)?.max_context_tokens;
    (v > 0).then_some(v as u64)
}

/// Extracts server-estimated `estimated_tokens_used` from `gen_metadata.data`
/// along verified wire path `chat(1) -> chat_start_metadata(9) -> context_window_metadata(10) -> estimated_tokens_used(1)`.
///
/// Returns `None` if any nested field is missing, malformed, or `<= 0`.
pub(crate) fn extract_estimated_context_tokens(data: &[u8]) -> Option<u64> {
    let v = extract_ctx_window(data)?.estimated_tokens_used;
    (v > 0).then_some(v as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_varint(buf: &mut Vec<u8>, mut val: u64) {
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

    fn push_field_varint(buf: &mut Vec<u8>, id: u64, val: u64) {
        push_varint(buf, id << 3);
        push_varint(buf, val);
    }

    fn push_field_bytes(buf: &mut Vec<u8>, id: u64, bytes: &[u8]) {
        push_varint(buf, (id << 3) | 2);
        push_varint(buf, bytes.len() as u64);
        buf.extend_from_slice(bytes);
    }

    fn push_field_f32(buf: &mut Vec<u8>, id: u64, val: f32) {
        push_varint(buf, (id << 3) | 5);
        buf.extend_from_slice(&val.to_le_bytes());
    }

    #[test]
    fn metadata_speed_valid_and_invalid() {
        let valid = [0x0a, 0x08, 0x22, 0x02, 0x18, 0x64, 0x62, 0x02, 0x08, 0x02];
        assert_eq!(extract_metadata_speed(&valid), Some(50.0));
        assert_eq!(extract_metadata_speed(&[0x0a, 0xff]), None);
        assert_eq!(extract_metadata_speed(&[]), None);
    }

    #[test]
    fn max_context_tokens_valid_and_invalid() {
        let valid = [0x0a, 0x08, 0x4a, 0x06, 0x52, 0x04, 0x20, 0x80, 0xa0, 0x1f];
        assert_eq!(extract_max_context_tokens(&valid), Some(512_000));
        let zero = [0x0a, 0x06, 0x4a, 0x04, 0x52, 0x02, 0x20, 0x00];
        assert_eq!(extract_max_context_tokens(&zero), None);
        assert_eq!(extract_max_context_tokens(&[]), None);
    }

    #[test]
    fn turn_metrics_extracts_ttft_and_short_decode() {
        // chat(1) -> { usage(4) -> { output(3)=100 }, ttft(11) -> { sec(1)=1 }, dur(12) -> { sec(1)=2 } }
        let blob = [
            0x0a, 0x0c, 0x22, 0x02, 0x18, 0x64, 0x5a, 0x02, 0x08, 0x01, 0x62, 0x02, 0x08, 0x02,
        ];
        let m = extract_turn_metrics(&blob).unwrap();
        assert_eq!(m.output_tokens, 100);
        assert_eq!(m.thinking_tokens, None);
        assert!((m.decode_secs - 2.0).abs() < 1e-9);
        assert!((m.ttft_secs.unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn turn_metrics_extracts_thinking_tokens() {
        // chat(1) -> { usage(4) -> { output(3)=100, thinking(9)=250 }, dur(12) -> { sec(1)=2 } }
        let blob = [
            0x0a, 0x0b, 0x22, 0x05, 0x18, 0x64, 0x48, 0xfa, 0x01, 0x62, 0x02, 0x08, 0x02,
        ];
        let m = extract_turn_metrics(&blob).unwrap();
        assert_eq!(m.output_tokens, 100);
        assert_eq!(m.thinking_tokens, Some(250));
        assert!((m.decode_secs - 2.0).abs() < 1e-9);
    }

    #[test]
    fn estimated_context_tokens_valid_and_invalid() {
        // chat(1) -> chat_start(9) -> ctx_window(10) -> { estimated(1)=64000, max(4)=512000 }
        let valid = [
            0x0a, 0x0c, 0x4a, 0x0a, 0x52, 0x08, 0x08, 0x80, 0xf4, 0x03, 0x20, 0x80, 0xa0, 0x1f,
        ];
        assert_eq!(extract_estimated_context_tokens(&valid), Some(64_000));
        assert_eq!(extract_max_context_tokens(&valid), Some(512_000));

        // estimated(1)=0 -> None
        let zero = [0x0a, 0x06, 0x4a, 0x04, 0x52, 0x02, 0x08, 0x00];
        assert_eq!(extract_estimated_context_tokens(&zero), None);
        assert_eq!(extract_estimated_context_tokens(&[]), None);
    }

    #[test]
    fn unified_speed_sanity_bounds() {
        // decode_secs = 0.005s (5_000_000 nanos) -> rejected (< 0.01s)
        let too_short = [
            0x0a, 0x0b, 0x22, 0x02, 0x18, 0x0a, 0x62, 0x05, 0x10, 0xc0, 0x96, 0xb1, 0x02,
        ];
        assert_eq!(extract_turn_metrics(&too_short), None);
        assert_eq!(extract_metadata_speed(&too_short), None);

        // decode_secs = 0.05s (50_000_000 nanos) -> accepted (10 / 0.05 = 200.0 tok/s)
        let fast_50ms = [
            0x0a, 0x0b, 0x22, 0x02, 0x18, 0x0a, 0x62, 0x05, 0x10, 0x80, 0xe1, 0xeb, 0x17,
        ];
        assert!((extract_metadata_speed(&fast_50ms).unwrap() - 200.0).abs() < 1e-6);

        // output = 1000, decode_secs = 1s -> 1000.0 tok/s -> accepted (< 10_000.0)
        let fast_1000 = [
            0x0a, 0x09, 0x22, 0x03, 0x18, 0xe8, 0x07, 0x62, 0x02, 0x08, 0x01,
        ];
        assert_eq!(extract_metadata_speed(&fast_1000), Some(1000.0));

        // output = 10_000, decode_secs = 1s -> 10_000.0 tok/s -> rejected (>= 10_000.0)
        let too_fast = [
            0x0a, 0x09, 0x22, 0x03, 0x18, 0x90, 0x4e, 0x62, 0x02, 0x08, 0x01,
        ];
        assert_eq!(extract_turn_metrics(&too_fast), None);
        assert_eq!(extract_metadata_speed(&too_fast), None);
    }

    #[test]
    fn gen_metadata_record_decodes_full_schema_and_packed_or_unpacked_steps() {
        let mut usage = Vec::new();
        push_field_varint(&mut usage, 2, 12_000);
        push_field_varint(&mut usage, 3, 600);
        push_field_varint(&mut usage, 4, 250);
        push_field_varint(&mut usage, 5, 48_000);
        push_field_varint(&mut usage, 9, 400);
        push_field_varint(&mut usage, 10, 200);

        let mut ttft = Vec::new();
        push_field_varint(&mut ttft, 1, 0);
        push_field_varint(&mut ttft, 2, 500_000_000);

        let mut dur = Vec::new();
        push_field_varint(&mut dur, 1, 2);

        let mut chat = Vec::new();
        push_field_varint(&mut chat, 3, 1526);
        push_field_bytes(&mut chat, 4, &usage);
        push_field_f32(&mut chat, 5, 0.0425);
        push_field_bytes(&mut chat, 11, &ttft);
        push_field_bytes(&mut chat, 12, &dur);
        push_field_bytes(&mut chat, 19, b"model-alpha-v1");
        push_field_bytes(&mut chat, 21, b"Model Alpha (Medium)");

        // Packed step_indices = [7, 8]
        let mut packed_steps = Vec::new();
        push_varint(&mut packed_steps, 7);
        push_varint(&mut packed_steps, 8);
        let mut root_packed = Vec::new();
        push_field_bytes(&mut root_packed, 1, &chat);
        push_field_bytes(&mut root_packed, 2, &packed_steps);

        let rec = extract_gen_metadata_record(&root_packed).unwrap();
        assert_eq!(rec.first_step_index, Some(7));
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

        // Unpacked step_indices = [14, 15]
        let mut root_unpacked = Vec::new();
        push_field_bytes(&mut root_unpacked, 1, &chat);
        push_field_varint(&mut root_unpacked, 2, 14);
        push_field_varint(&mut root_unpacked, 2, 15);
        let rec_unpacked = extract_gen_metadata_record(&root_unpacked).unwrap();
        assert_eq!(rec_unpacked.first_step_index, Some(14));
    }

    #[test]
    fn step_created_at_secs_rejects_out_of_range_years() {
        let mut ts_min = Vec::new();
        push_field_varint(&mut ts_min, 1, (-62_135_596_800_i64) as u64);
        let mut meta_min = Vec::new();
        push_field_bytes(&mut meta_min, 1, &ts_min);
        assert_eq!(
            extract_step_created_at_secs(&meta_min),
            Some(-62_135_596_800)
        );

        let mut ts_max = Vec::new();
        push_field_varint(&mut ts_max, 1, 253_402_300_799);
        let mut meta_max = Vec::new();
        push_field_bytes(&mut meta_max, 1, &ts_max);
        assert_eq!(
            extract_step_created_at_secs(&meta_max),
            Some(253_402_300_799)
        );

        let mut ts_too_low = Vec::new();
        push_field_varint(&mut ts_too_low, 1, (-62_135_596_801_i64) as u64);
        let mut meta_too_low = Vec::new();
        push_field_bytes(&mut meta_too_low, 1, &ts_too_low);
        assert_eq!(extract_step_created_at_secs(&meta_too_low), None);

        let mut ts_too_high = Vec::new();
        push_field_varint(&mut ts_too_high, 1, 253_402_300_800);
        let mut meta_too_high = Vec::new();
        push_field_bytes(&mut meta_too_high, 1, &ts_too_high);
        assert_eq!(extract_step_created_at_secs(&meta_too_high), None);
    }

    #[test]
    fn duration_rejects_negative_seconds_and_invalid_nanos() {
        let mut neg_secs = Vec::new();
        push_field_varint(&mut neg_secs, 1, (-1_i64) as u64);
        assert_eq!(ProtoDuration::decode(&neg_secs[..]).unwrap().to_secs(), None);

        let mut neg_nanos = Vec::new();
        push_field_varint(&mut neg_nanos, 1, 1);
        push_field_varint(&mut neg_nanos, 2, (-1_i64) as u64);
        assert_eq!(
            ProtoDuration::decode(&neg_nanos[..]).unwrap().to_secs(),
            None
        );

        let mut huge_nanos = Vec::new();
        push_field_varint(&mut huge_nanos, 1, 1);
        push_field_varint(&mut huge_nanos, 2, 1_000_000_000);
        assert_eq!(
            ProtoDuration::decode(&huge_nanos[..]).unwrap().to_secs(),
            None
        );

        // Verify extract_gen_metadata_record also rejects negative duration.
        let mut chat = Vec::new();
        push_field_bytes(&mut chat, 12, &neg_secs);
        let mut root = Vec::new();
        push_field_bytes(&mut root, 1, &chat);
        let rec = extract_gen_metadata_record(&root).unwrap();
        assert_eq!(rec.decode_secs, None);
    }

    #[test]
    fn chat_start_created_at_rejects_out_of_range_timestamps() {
        let decode_chat_start = |secs: u64| -> Option<i64> {
            let mut ts = Vec::new();
            push_field_varint(&mut ts, 1, secs);
            let mut start = Vec::new();
            push_field_bytes(&mut start, 4, &ts);
            let mut chat = Vec::new();
            push_field_bytes(&mut chat, 9, &start);
            let mut root = Vec::new();
            push_field_bytes(&mut root, 1, &chat);
            extract_gen_metadata_record(&root)
                .unwrap()
                .chat_start_created_at_secs
        };

        assert_eq!(decode_chat_start(1_763_400_000), Some(1_763_400_000));
        assert_eq!(decode_chat_start((-62_135_596_801_i64) as u64), None);
        assert_eq!(decode_chat_start(253_402_300_800), None);
    }
}
