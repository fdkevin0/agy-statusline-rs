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

/// Decodes a `gen_metadata.data` Protobuf blob once to extract turn speed/TTFT metrics
/// and server-reported context window metadata.
///
/// Enforces unified speed sanity bounds: `decode_secs >= 0.01`, `output_tokens > 0`,
/// and `0.0 < output_tokens / decode_secs < 10_000.0`.
pub(crate) fn extract_gen_metadata_summary(data: &[u8]) -> Option<GenMetadataSummary> {
    let chat = GenMetadataBlob::decode(data).ok()?.chat_model?;
    let turn = match (chat.usage, chat.streaming_duration.and_then(|d| d.to_secs())) {
        (Some(usage), Some(decode_secs))
            if is_sane_decode_speed(usage.output_tokens, decode_secs) =>
        {
            Some(TurnMetrics {
                output_tokens: usage.output_tokens,
                thinking_tokens: (usage.thinking_output_tokens > 0)
                    .then_some(usage.thinking_output_tokens),
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
        .filter(|&d| d > 0.0);

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
    let metrics = extract_gen_metadata_summary(data)?.turn?;
    Some((metrics.output_tokens as f64) / metrics.decode_secs)
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct GenBlobSpec<'a> {
    pub(crate) step_indices: Vec<u64>,
    pub(crate) model_enum: u64,
    pub(crate) resp_model: Option<&'a str>,
    pub(crate) disp_name: Option<&'a str>,
    pub(crate) model_cost: Option<f32>,
    pub(crate) input_tok: u64,
    pub(crate) output_tok: u64,
    pub(crate) cache_write_tok: u64,
    pub(crate) cache_read_tok: u64,
    pub(crate) thinking_tok: u64,
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
                input_tokens: spec.input_tok,
                output_tokens: spec.output_tok,
                cache_write_tokens: spec.cache_write_tok,
                cache_read_tokens: spec.cache_read_tok,
                thinking_output_tokens: spec.thinking_tok,
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
            response_model: spec.resp_model.map(str::to_string),
            model_display_name: spec.disp_name.map(str::to_string),
        }),
        step_indices: spec.step_indices,
    }
    .encode_to_vec()
}

#[cfg(test)]
pub(crate) fn encode_step_meta_blob(created_at_secs: i64) -> Vec<u8> {
    StepMetadataBlob {
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
        assert_eq!(extract_metadata_speed(&valid), Some(50.0));
        assert_eq!(extract_metadata_speed(&[0x0a, 0xff]), None);
        assert_eq!(extract_metadata_speed(&[]), None);
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
            output_tok: 100,
            thinking_tok: 250,
            dur_secs: 2,
            ..Default::default()
        });
        let m = extract_gen_metadata_summary(&blob).unwrap().turn.unwrap();
        assert_eq!(m.output_tokens, 100);
        assert_eq!(m.thinking_tokens, Some(250));
        assert!((m.decode_secs - 2.0).abs() < 1e-9);
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
        assert_eq!(extract_metadata_speed(&too_short), None);

        // decode_secs = 0.05s (50_000_000 nanos) -> accepted (10 / 0.05 = 200.0 tok/s)
        let fast_50ms = encode_gen_blob(GenBlobSpec {
            output_tok: 10,
            dur_nanos: 50_000_000,
            ..Default::default()
        });
        assert!((extract_metadata_speed(&fast_50ms).unwrap() - 200.0).abs() < 1e-6);

        // output = 1000, decode_secs = 1s -> 1000.0 tok/s -> accepted (< 10_000.0)
        let fast_1000 = encode_speed_1s_blob(1_000);
        assert_eq!(extract_metadata_speed(&fast_1000), Some(1000.0));

        // output = 10_000, decode_secs = 1s -> 10_000.0 tok/s -> rejected (>= 10_000.0)
        let too_fast = encode_speed_1s_blob(10_000);
        assert_eq!(
            extract_gen_metadata_summary(&too_fast).and_then(|s| s.turn),
            None
        );
        assert_eq!(extract_metadata_speed(&too_fast), None);
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

        // Wire-compatibility: unpacked repeated step_indices = [14, 15] (prost emits packed by default).
        // chat(1) -> { model(3)=1 }, step_indices(2)=14, step_indices(2)=15
        let root_unpacked: &[u8] = &[0x0a, 0x02, 0x18, 0x01, 0x10, 0x0e, 0x10, 0x0f];
        let rec_unpacked = extract_gen_metadata_record(root_unpacked).unwrap();
        assert_eq!(rec_unpacked.first_step_index, Some(14));
        assert_eq!(rec_unpacked.model_enum, 1);
    }

    #[test]
    fn step_created_at_secs_rejects_out_of_range_years() {
        assert_eq!(
            extract_step_created_at_secs(&encode_step_meta_blob(-62_135_596_800)),
            Some(-62_135_596_800)
        );
        assert_eq!(
            extract_step_created_at_secs(&encode_step_meta_blob(253_402_300_799)),
            Some(253_402_300_799)
        );
        assert_eq!(
            extract_step_created_at_secs(&encode_step_meta_blob(-62_135_596_801)),
            None
        );
        assert_eq!(
            extract_step_created_at_secs(&encode_step_meta_blob(253_402_300_800)),
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
        assert_eq!(ProtoDuration::decode(&neg_secs[..]).unwrap().to_secs(), None);

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
        assert_eq!(extract_gen_metadata_record(&root).unwrap().decode_secs, None);
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
