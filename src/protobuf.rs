//! Zero-copy Protobuf wire-format decoder for AGY generation metadata.
//!
//! Extracts turn timing, token counts, and context window metadata from
//! `CortexStepGeneratorMetadata` (`gen_metadata.data`) without schema codegen
//! or heap allocation.
//!
//! Verified wire paths (from embedded `.proto` descriptors in `agy`):
//! - `CortexStepGeneratorMetadata`:
//!   - `chat_model` (`ChatModelMetadata`): field `1` (bytes)
//!     - `model` (`Model` enum): field `3` (varint)
//!     - `usage` (`ModelUsageStats`): field `4` (bytes)
//!       - `input_tokens`: field `2` (varint `uint64`)
//!       - `output_tokens`: field `3` (varint `uint64`)
//!       - `cache_write_tokens`: field `4` (varint `uint64`)
//!       - `cache_read_tokens`: field `5` (varint `uint64`)
//!       - `thinking_output_tokens`: field `9` (varint `uint64`)
//!       - `response_output_tokens`: field `10` (varint `uint64`)
//!     - `model_cost`: field `5` (fixed32 IEEE-754 `float` USD)
//!     - `chat_start_metadata` (`ChatStartMetadata`): field `9` (bytes)
//!       - `created_at` (`google.protobuf.Timestamp`): field `4` (bytes; `seconds=1`)
//!       - `context_window_metadata` (`ContextWindowMetadata`): field `10` (bytes)
//!         - `estimated_tokens_used`: field `1` (varint `int32`)
//!         - `max_context_tokens`: field `4` (varint `int32`)
//!     - `time_to_first_token` (`google.protobuf.Duration`): field `11` (bytes; `seconds=1`, `nanos=2`)
//!     - `streaming_duration` (`google.protobuf.Duration`): field `12` (bytes; `seconds=1`, `nanos=2`)
//!     - `response_model`: field `19` (UTF-8 string)
//!     - `model_display_name`: field `21` (UTF-8 string)
//!   - `step_indices`: field `2` (repeated `uint32`, packed or unpacked)
//! - `CortexStepMetadata` (`steps.metadata`):
//!   - `created_at` (`google.protobuf.Timestamp`): field `1` (bytes; `seconds=1`)

/// A decoded Protobuf wire field value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldValue<'a> {
    /// Base-128 varint (wire type 0).
    Varint(u64),
    /// 32-bit little-endian fixed-width value (wire type 5).
    Fixed32(u32),
    /// 64-bit little-endian fixed-width value (wire type 1).
    Fixed64(u64),
    /// Length-delimited byte slice (wire type 2).
    Bytes(&'a [u8]),
}

impl FieldValue<'_> {
    /// Returns the numeric payload as `u64` for `Varint`, `Fixed32`, and `Fixed64` variants.
    pub const fn as_u64(self) -> Option<u64> {
        match self {
            Self::Varint(v) | Self::Fixed64(v) => Some(v),
            Self::Fixed32(v) => Some(v as u64),
            Self::Bytes(_) => None,
        }
    }
}

/// Zero-copy iterator over `(field_num, FieldValue)` pairs in a Protobuf wire buffer.
///
/// Yields `Err(())` once and terminates if the wire stream is malformed or truncated.
#[derive(Debug, Clone)]
pub struct WireIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> WireIter<'a> {
    /// Creates a new wire-format iterator over `data`.
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn decode_next(&mut self) -> Result<(u32, FieldValue<'a>), ()> {
        let key = decode_varint(self.data, &mut self.pos).ok_or(())?;
        let field_id = key >> 3;
        if field_id == 0 || field_id > u64::from(u32::MAX) {
            return Err(());
        }
        let field_num = field_id as u32;
        let value = match key & 7 {
            0 => FieldValue::Varint(decode_varint(self.data, &mut self.pos).ok_or(())?),
            1 => {
                let slice = self.data.get(self.pos..self.pos + 8).ok_or(())?;
                self.pos += 8;
                let bits = u64::from_le_bytes([
                    slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
                ]);
                FieldValue::Fixed64(bits)
            }
            2 => {
                let size_u64 = decode_varint(self.data, &mut self.pos).ok_or(())?;
                let remaining = self.data.len() - self.pos;
                if size_u64 > remaining as u64 {
                    return Err(());
                }
                let size = size_u64 as usize;
                let slice = &self.data[self.pos..self.pos + size];
                self.pos += size;
                FieldValue::Bytes(slice)
            }
            5 => {
                let slice = self.data.get(self.pos..self.pos + 4).ok_or(())?;
                self.pos += 4;
                let bits = u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]);
                FieldValue::Fixed32(bits)
            }
            _ => return Err(()),
        };
        Ok((field_num, value))
    }
}

impl<'a> Iterator for WireIter<'a> {
    type Item = Result<(u32, FieldValue<'a>), ()>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.data.len() {
            return None;
        }
        let item = self.decode_next();
        if item.is_err() {
            self.pos = self.data.len();
        }
        Some(item)
    }
}

/// Decodes a base-128 varint starting at `*pos`, advancing `*pos` past it.
///
/// Returns `None` on truncated input or 64-bit overflow.
///
/// # Examples
///
/// ```
/// use agy_statusline::protobuf::decode_varint;
///
/// let mut pos = 0;
/// assert_eq!(decode_varint(&[0xac, 0x02], &mut pos), Some(300));
/// assert_eq!(pos, 2);
/// ```
pub fn decode_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    for _ in 0..10 {
        let &b = data.get(*pos)?;
        *pos += 1;
        if shift == 63 && b > 1 {
            return None;
        }
        value |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(value);
        }
        if shift == 63 {
            return None;
        }
        shift += 7;
    }
    None
}

/// Scans a Protobuf message slice for the last occurrence of field `wanted`.
///
/// Supports wire types 0 (varint), 1 (64-bit), 2 (length-delimited), and 5 (32-bit).
/// Returns `None` if the wire stream is malformed or the field is absent.
///
/// # Examples
///
/// ```
/// use agy_statusline::protobuf::{find_field, FieldValue};
///
/// let msg = [0x08, 0x2a]; // field 1 = 42
/// assert_eq!(find_field(&msg, 1), Some(FieldValue::Varint(42)));
/// assert_eq!(find_field(&msg, 2), None);
/// ```
pub fn find_field(data: &[u8], wanted: u64) -> Option<FieldValue<'_>> {
    let mut result = None;
    for item in WireIter::new(data) {
        let (field_id, field) = item.ok()?;
        if u64::from(field_id) == wanted {
            result = Some(field);
        }
    }
    result
}

/// Returns the byte slice for length-delimited field `id`, if present.
pub fn find_bytes(data: &[u8], id: u64) -> Option<&[u8]> {
    match find_field(data, id)? {
        FieldValue::Bytes(bytes) => Some(bytes),
        _ => None,
    }
}

/// Returns the numeric value for field `id`, defaulting to `0` if absent or non-numeric.
pub fn find_number(data: &[u8], id: u64) -> u64 {
    find_field(data, id)
        .and_then(FieldValue::as_u64)
        .unwrap_or(0)
}

const FIELD_CHAT: u32 = 1;
const FIELD_STEP_INDICES: u32 = 2;
const FIELD_CHAT_MODEL_ENUM: u32 = 3;
const FIELD_CHAT_USAGE: u32 = 4;
const FIELD_CHAT_MODEL_COST: u32 = 5;
const FIELD_USAGE_INPUT_TOKENS: u32 = 2;
const FIELD_USAGE_OUTPUT_TOKENS: u32 = 3;
const FIELD_USAGE_CACHE_WRITE_TOKENS: u32 = 4;
const FIELD_USAGE_CACHE_READ_TOKENS: u32 = 5;
const FIELD_USAGE_THINKING_OUTPUT_TOKENS: u32 = 9;
const FIELD_USAGE_RESPONSE_OUTPUT_TOKENS: u32 = 10;
const FIELD_CHAT_START: u32 = 9;
const FIELD_CHAT_START_CREATED_AT: u32 = 4;
const FIELD_START_CTX_WINDOW: u32 = 10;
const FIELD_CTX_ESTIMATED_TOKENS: u32 = 1;
const FIELD_CTX_MAX_TOKENS: u32 = 4;
const FIELD_CHAT_TTFT: u32 = 11;
const FIELD_CHAT_DECODE_DURATION: u32 = 12;
const FIELD_CHAT_RESPONSE_MODEL: u32 = 19;
const FIELD_CHAT_MODEL_DISPLAY_NAME: u32 = 21;
const FIELD_DURATION_SECS: u32 = 1;
const FIELD_DURATION_NANOS: u32 = 2;
const FIELD_STEP_CREATED_AT: u32 = 1;
const FIELD_TIMESTAMP_SECS: u32 = 1;

/// Minimum streaming decode duration (seconds) accepted as a valid turn speed sample.
pub const MIN_TURN_DECODE_SECS: f64 = 0.01;
/// Maximum decode throughput (tokens/second) accepted as a valid turn speed sample.
pub const MAX_TURN_SPEED_TPS: f64 = 10_000.0;
/// Maximum time-to-first-token (seconds) accepted as a valid TTFT sample.
pub const MAX_TTFT_SECS: f64 = 600.0;

/// Timing and output token metrics extracted from one `gen_metadata` turn.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TurnMetrics {
    /// Total output tokens generated in the turn (`chat(1).usage(4).output_tokens(3)`).
    pub output_tokens: u64,
    /// Optional thinking output tokens (`chat(1).usage(4).thinking_output_tokens(9)`).
    pub thinking_tokens: Option<u64>,
    /// Streaming decode duration in seconds (`chat(1).streaming_duration(12)`).
    pub decode_secs: f64,
    /// Optional time-to-first-token in seconds (`chat(1).time_to_first_token(11)`).
    pub ttft_secs: Option<f64>,
}

/// Full generation record decoded from `CortexStepGeneratorMetadata` (`gen_metadata.data`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GenMetadataRecord<'a> {
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
    /// Visible response output tokens (`chat(1).usage(4).response_output_tokens(10)`).
    pub response_output_tokens: u64,
    /// Native server-recorded turn cost in USD (`chat(1).model_cost(5)`), if `> 0` and finite.
    pub model_cost_usd: Option<f64>,
    /// Optional chat start timestamp seconds (`chat(1).chat_start_metadata(9).created_at(4).seconds(1)`).
    pub chat_start_created_at_secs: Option<i64>,
    /// Time-to-first-token in seconds (`chat(1).time_to_first_token(11)`), if in `(0, MAX_TTFT_SECS)`.
    pub ttft_secs: Option<f64>,
    /// Streaming decode duration in seconds (`chat(1).streaming_duration(12)`).
    pub decode_secs: Option<f64>,
    /// Wire response model string (`chat(1).response_model(19)`).
    pub response_model: Option<&'a str>,
    /// Human-readable display model name (`chat(1).model_display_name(21)`).
    pub model_display_name: Option<&'a str>,
}

impl GenMetadataRecord<'_> {
    /// Returns decode throughput in tokens/second when output tokens and duration
    /// satisfy [`MIN_TURN_DECODE_SECS`] and [`MAX_TURN_SPEED_TPS`].
    pub fn decode_speed_tps(&self) -> Option<f64> {
        let secs = self.decode_secs?;
        if !is_sane_decode_speed(self.output_tokens, secs) {
            return None;
        }
        Some((self.output_tokens as f64) / secs)
    }
}

fn find_nonzero_number(data: &[u8], id: u32) -> Option<u64> {
    find_field(data, u64::from(id))?
        .as_u64()
        .filter(|&val| val > 0)
}

fn parse_utf8_trimmed(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes)
        .ok()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn decode_duration_secs(msg: &[u8]) -> Option<f64> {
    let mut secs: i64 = 0;
    let mut nanos: i64 = 0;
    for item in WireIter::new(msg) {
        let (field_num, value) = item.ok()?;
        match (field_num, value) {
            (FIELD_DURATION_SECS, FieldValue::Varint(bits)) => secs = bits as i64,
            (FIELD_DURATION_NANOS, FieldValue::Varint(bits)) => nanos = bits as i64,
            _ => {}
        }
    }
    if secs < 0 || !(0..1_000_000_000).contains(&nanos) {
        return None;
    }
    Some((secs as f64) + (nanos as f64) / 1e9)
}

fn decode_timestamp_secs(ts: &[u8]) -> Option<i64> {
    let secs = find_nonzero_number(ts, FIELD_TIMESTAMP_SECS)? as i64;
    (MIN_TIMESTAMP_SECS..=MAX_TIMESTAMP_SECS)
        .contains(&secs)
        .then_some(secs)
}

fn is_sane_decode_speed(output_tokens: u64, decode_secs: f64) -> bool {
    if decode_secs < MIN_TURN_DECODE_SECS || output_tokens == 0 {
        return false;
    }
    let speed = (output_tokens as f64) / decode_secs;
    speed.is_finite() && speed > 0.0 && speed < MAX_TURN_SPEED_TPS
}

/// Extracts [`TurnMetrics`] (output tokens, optional thinking tokens, decode duration,
/// and optional TTFT) from a `gen_metadata.data` Protobuf blob.
///
/// Enforces unified sanity bounds: `decode_secs >= 0.01`, `output_tokens > 0`,
/// and `0.0 < output_tokens / decode_secs < 10_000.0`.
pub fn extract_turn_metrics(data: &[u8]) -> Option<TurnMetrics> {
    let chat = find_bytes(data, u64::from(FIELD_CHAT))?;
    let usage = find_bytes(chat, u64::from(FIELD_CHAT_USAGE))?;
    let duration = find_bytes(chat, u64::from(FIELD_CHAT_DECODE_DURATION))?;
    let decode_secs = decode_duration_secs(duration)?;
    let output_tokens = find_number(usage, u64::from(FIELD_USAGE_OUTPUT_TOKENS));
    if !is_sane_decode_speed(output_tokens, decode_secs) {
        return None;
    }
    let thinking_tokens = find_nonzero_number(usage, FIELD_USAGE_THINKING_OUTPUT_TOKENS);
    let ttft_secs = find_bytes(chat, u64::from(FIELD_CHAT_TTFT))
        .and_then(decode_duration_secs)
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
pub fn extract_gen_metadata_record(data: &[u8]) -> Option<GenMetadataRecord<'_>> {
    let mut chat: Option<&[u8]> = None;
    let mut first_step_index: Option<u64> = None;

    for item in WireIter::new(data) {
        let (field_num, value) = item.ok()?;
        match field_num {
            FIELD_CHAT => {
                chat = match value {
                    FieldValue::Bytes(bytes) => Some(bytes),
                    _ => None,
                };
            }
            FIELD_STEP_INDICES if first_step_index.is_none() => match value {
                FieldValue::Varint(v) => first_step_index = Some(v),
                FieldValue::Bytes(slice) if !slice.is_empty() => {
                    let mut p = 0;
                    first_step_index = decode_varint(slice, &mut p);
                }
                _ => {}
            },
            _ => {}
        }
    }

    let chat = chat?;
    let mut model_enum: u64 = 0;
    let mut usage_bytes: Option<&[u8]> = None;
    let mut model_cost_usd: Option<f64> = None;
    let mut chat_start_bytes: Option<&[u8]> = None;
    let mut ttft_bytes: Option<&[u8]> = None;
    let mut decode_dur_bytes: Option<&[u8]> = None;
    let mut response_model: Option<&str> = None;
    let mut model_display_name: Option<&str> = None;

    for item in WireIter::new(chat) {
        let (field_num, value) = item.ok()?;
        match field_num {
            FIELD_CHAT_MODEL_ENUM => {
                model_enum = value.as_u64().unwrap_or(0);
            }
            FIELD_CHAT_USAGE => {
                usage_bytes = match value {
                    FieldValue::Bytes(b) => Some(b),
                    _ => None,
                };
            }
            FIELD_CHAT_MODEL_COST => {
                model_cost_usd = match value {
                    FieldValue::Fixed32(bits) => {
                        let cost = f64::from(f32::from_bits(bits));
                        (cost.is_finite() && cost > 0.0).then_some(cost)
                    }
                    _ => None,
                };
            }
            FIELD_CHAT_START => {
                chat_start_bytes = match value {
                    FieldValue::Bytes(b) => Some(b),
                    _ => None,
                };
            }
            FIELD_CHAT_TTFT => {
                ttft_bytes = match value {
                    FieldValue::Bytes(b) => Some(b),
                    _ => None,
                };
            }
            FIELD_CHAT_DECODE_DURATION => {
                decode_dur_bytes = match value {
                    FieldValue::Bytes(b) => Some(b),
                    _ => None,
                };
            }
            FIELD_CHAT_RESPONSE_MODEL => {
                response_model = match value {
                    FieldValue::Bytes(b) => parse_utf8_trimmed(b),
                    _ => None,
                };
            }
            FIELD_CHAT_MODEL_DISPLAY_NAME => {
                model_display_name = match value {
                    FieldValue::Bytes(b) => parse_utf8_trimmed(b),
                    _ => None,
                };
            }
            _ => {}
        }
    }

    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cache_write_tokens = 0u64;
    let mut cache_read_tokens = 0u64;
    let mut thinking_output_tokens = 0u64;
    let mut response_output_tokens = 0u64;

    if let Some(usage) = usage_bytes {
        let mut usage_ok = true;
        for item in WireIter::new(usage) {
            let Ok((field_num, value)) = item else {
                usage_ok = false;
                break;
            };
            let v = value.as_u64().unwrap_or(0);
            match field_num {
                FIELD_USAGE_INPUT_TOKENS => input_tokens = v,
                FIELD_USAGE_OUTPUT_TOKENS => output_tokens = v,
                FIELD_USAGE_CACHE_WRITE_TOKENS => cache_write_tokens = v,
                FIELD_USAGE_CACHE_READ_TOKENS => cache_read_tokens = v,
                FIELD_USAGE_THINKING_OUTPUT_TOKENS => thinking_output_tokens = v,
                FIELD_USAGE_RESPONSE_OUTPUT_TOKENS => response_output_tokens = v,
                _ => {}
            }
        }
        if !usage_ok {
            input_tokens = 0;
            output_tokens = 0;
            cache_write_tokens = 0;
            cache_read_tokens = 0;
            thinking_output_tokens = 0;
            response_output_tokens = 0;
        }
    }

    let chat_start_created_at_secs = chat_start_bytes
        .and_then(|start| find_bytes(start, u64::from(FIELD_CHAT_START_CREATED_AT)))
        .and_then(decode_timestamp_secs);

    let ttft_secs = ttft_bytes
        .and_then(decode_duration_secs)
        .filter(|&t| t > 0.0 && t < MAX_TTFT_SECS);
    let decode_secs = decode_dur_bytes
        .and_then(decode_duration_secs)
        .filter(|&d| d.is_finite() && d > 0.0);

    Some(GenMetadataRecord {
        first_step_index,
        model_enum,
        input_tokens,
        output_tokens,
        cache_write_tokens,
        cache_read_tokens,
        thinking_output_tokens,
        response_output_tokens,
        model_cost_usd,
        chat_start_created_at_secs,
        ttft_secs,
        decode_secs,
        response_model,
        model_display_name,
    })
}

/// Minimum valid `google.protobuf.Timestamp.seconds` (`0001-01-01T00:00:00Z`).
const MIN_TIMESTAMP_SECS: i64 = -62_135_596_800;
/// Maximum valid `google.protobuf.Timestamp.seconds` (`9999-12-31T23:59:59Z`).
const MAX_TIMESTAMP_SECS: i64 = 253_402_300_799;

/// Extracts `CortexStepMetadata.created_at` (`field 1 -> google.protobuf.Timestamp.seconds = 1`)
/// from a `steps.metadata` Protobuf blob.
///
/// Rejects timestamps outside proleptic UTC years `0001..=9999`
/// (`-62_135_596_800..=253_402_300_799`).
pub fn extract_step_created_at_secs(data: &[u8]) -> Option<i64> {
    let ts = find_bytes(data, u64::from(FIELD_STEP_CREATED_AT))?;
    decode_timestamp_secs(ts)
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

fn extract_ctx_window_field(data: &[u8], field_id: u32) -> Option<u64> {
    let chat = find_bytes(data, u64::from(FIELD_CHAT))?;
    let chat_start = find_bytes(chat, u64::from(FIELD_CHAT_START))?;
    let ctx_window = find_bytes(chat_start, u64::from(FIELD_START_CTX_WINDOW))?;
    find_nonzero_number(ctx_window, field_id)
}

/// Extracts server-issued `max_context_tokens` from `gen_metadata.data`
/// along verified wire path `chat(1) -> chat_start_metadata(9) -> context_window_metadata(10) -> max_context_tokens(4)`.
///
/// Returns `None` if any nested field is missing, malformed, or `0`.
pub fn extract_max_context_tokens(data: &[u8]) -> Option<u64> {
    extract_ctx_window_field(data, FIELD_CTX_MAX_TOKENS)
}

/// Extracts server-estimated `estimated_tokens_used` from `gen_metadata.data`
/// along verified wire path `chat(1) -> chat_start_metadata(9) -> context_window_metadata(10) -> estimated_tokens_used(1)`.
///
/// Returns `None` if any nested field is missing, malformed, or `0`.
pub fn extract_estimated_context_tokens(data: &[u8]) -> Option<u64> {
    extract_ctx_window_field(data, FIELD_CTX_ESTIMATED_TOKENS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_edge_cases() {
        let mut pos = 0;
        assert_eq!(decode_varint(&[], &mut pos), None);
        pos = 0;
        assert_eq!(decode_varint(&[0x00], &mut pos), Some(0));
        pos = 0;
        assert_eq!(
            decode_varint(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01], &mut pos),
            Some(u64::MAX)
        );
        pos = 0;
        assert_eq!(
            decode_varint(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02], &mut pos),
            None
        );
    }

    #[test]
    fn metadata_speed_valid_and_invalid() {
        let valid = [0x0a, 0x08, 0x22, 0x02, 0x18, 0x64, 0x62, 0x02, 0x08, 0x02];
        assert_eq!(extract_metadata_speed(&valid), Some(50.0));
        assert_eq!(extract_metadata_speed(&[0x0a, 0xff]), None);
        assert_eq!(extract_metadata_speed(&[]), None);
    }

    #[test]
    fn wire_types_1_and_5_preserve_little_endian_bits() {
        // field 2 (64-bit, 8 bytes), then field 1 (varint = 7)
        let msg = [0x11, 1, 2, 3, 4, 5, 6, 7, 8, 0x08, 0x07];
        assert_eq!(find_number(&msg, 1), 7);
        assert_eq!(find_number(&msg, 2), 0x0807_0605_0403_0201);
        // field 2 (32-bit, 4 bytes), then field 1 (varint = 9)
        let msg32 = [0x15, 1, 2, 3, 4, 0x08, 0x09];
        assert_eq!(find_number(&msg32, 1), 9);
        assert_eq!(find_number(&msg32, 2), 0x0403_0201);
        // truncated 64-bit field
        assert_eq!(find_field(&[0x11, 1, 2, 3], 2), None);
        // truncated 32-bit field
        assert_eq!(find_field(&[0x15, 1, 2, 3], 2), None);
        // invalid wire type 3
        assert_eq!(find_field(&[0x0b, 0x01], 1), None);
        // field id 0 is invalid
        assert_eq!(find_field(&[0x00, 0x01], 0), None);
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
        assert_eq!(rec.response_output_tokens, 200);
        assert!((rec.model_cost_usd.unwrap() - 0.0425).abs() < 1e-6);
        assert!((rec.ttft_secs.unwrap() - 0.5).abs() < 1e-6);
        assert!((rec.decode_secs.unwrap() - 2.0).abs() < 1e-6);
        assert!((rec.decode_speed_tps().unwrap() - 300.0).abs() < 1e-6);
        assert_eq!(rec.response_model, Some("model-alpha-v1"));
        assert_eq!(rec.model_display_name, Some("Model Alpha (Medium)"));

        // Unpacked step_indices = [14, 15]
        let mut root_unpacked = Vec::new();
        push_field_bytes(&mut root_unpacked, 1, &chat);
        push_field_varint(&mut root_unpacked, 2, 14);
        push_field_varint(&mut root_unpacked, 2, 15);
        let rec_unpacked = extract_gen_metadata_record(&root_unpacked).unwrap();
        assert_eq!(rec_unpacked.first_step_index, Some(14));
    }

    #[test]
    fn step_created_at_secs_decodes_timestamp() {
        let mut ts = Vec::new();
        push_field_varint(&mut ts, 1, 1_763_400_000);
        let mut step_meta = Vec::new();
        push_field_bytes(&mut step_meta, 1, &ts);
        assert_eq!(extract_step_created_at_secs(&step_meta), Some(1_763_400_000));
        assert_eq!(extract_step_created_at_secs(&[]), None);
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

    fn push_field_fixed64(buf: &mut Vec<u8>, id: u64, val: u64) {
        push_varint(buf, (id << 3) | 1);
        buf.extend_from_slice(&val.to_le_bytes());
    }

    #[test]
    fn duration_rejects_negative_seconds_and_invalid_nanos() {
        let mut neg_secs = Vec::new();
        push_field_varint(&mut neg_secs, 1, (-1_i64) as u64);
        assert_eq!(decode_duration_secs(&neg_secs), None);

        let mut neg_nanos = Vec::new();
        push_field_varint(&mut neg_nanos, 1, 1);
        push_field_varint(&mut neg_nanos, 2, (-1_i64) as u64);
        assert_eq!(decode_duration_secs(&neg_nanos), None);

        let mut huge_nanos = Vec::new();
        push_field_varint(&mut huge_nanos, 1, 1);
        push_field_varint(&mut huge_nanos, 2, 1_000_000_000);
        assert_eq!(decode_duration_secs(&huge_nanos), None);

        // Verify extract_gen_metadata_record also rejects negative duration.
        let mut chat = Vec::new();
        push_field_bytes(&mut chat, 12, &neg_secs);
        let mut root = Vec::new();
        push_field_bytes(&mut root, 1, &chat);
        let rec = extract_gen_metadata_record(&root).unwrap();
        assert_eq!(rec.decode_secs, None);
    }

    #[test]
    fn model_cost_rejects_non_fixed32_wire_types() {
        let mut chat_f64 = Vec::new();
        push_field_fixed64(&mut chat_f64, 5, f64::to_bits(0.05));
        let mut root_f64 = Vec::new();
        push_field_bytes(&mut root_f64, 1, &chat_f64);
        let rec_f64 = extract_gen_metadata_record(&root_f64).unwrap();
        assert_eq!(rec_f64.model_cost_usd, None);

        let mut chat_varint = Vec::new();
        push_field_varint(&mut chat_varint, 5, u64::from(0.05_f32.to_bits()));
        let mut root_varint = Vec::new();
        push_field_bytes(&mut root_varint, 1, &chat_varint);
        let rec_varint = extract_gen_metadata_record(&root_varint).unwrap();
        assert_eq!(rec_varint.model_cost_usd, None);

        // Fixed32 is accepted.
        let mut chat_f32 = Vec::new();
        push_field_f32(&mut chat_f32, 5, 0.05);
        let mut root_f32 = Vec::new();
        push_field_bytes(&mut root_f32, 1, &chat_f32);
        let rec_f32 = extract_gen_metadata_record(&root_f32).unwrap();
        assert!((rec_f32.model_cost_usd.unwrap() - 0.05).abs() < 1e-6);
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
