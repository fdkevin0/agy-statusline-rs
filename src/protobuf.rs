//! Zero-copy Protobuf wire-format decoder for AGY generation metadata.
//!
//! Extracts turn timing, token counts, and context window metadata from
//! `CortexStepGeneratorMetadata` (`gen_metadata.data`) without schema codegen
//! or heap allocation.
//!
//! Verified wire paths (from embedded `.proto` descriptors in `agy`):
//! - `chat_model` (`ChatModelMetadata`): field `1` (bytes)
//!   - `usage` (`ModelUsageStats`): field `4` (bytes)
//!     - `input_tokens`: field `2` (varint `uint64`)
//!     - `output_tokens`: field `3` (varint `uint64`)
//!     - `cache_read_tokens`: field `5` (varint `uint64`)
//!     - `thinking_output_tokens`: field `9` (varint `uint64`)
//!   - `chat_start_metadata` (`ChatStartMetadata`): field `9` (bytes)
//!     - `context_window_metadata` (`ContextWindowMetadata`): field `10` (bytes)
//!       - `estimated_tokens_used`: field `1` (varint `int32`)
//!       - `max_context_tokens`: field `4` (varint `int32`)
//!   - `time_to_first_token` (`google.protobuf.Duration`): field `11` (bytes; `seconds=1`, `nanos=2`)
//!   - `streaming_duration` (`google.protobuf.Duration`): field `12` (bytes; `seconds=1`, `nanos=2`)

/// A decoded Protobuf wire field value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldValue<'a> {
    /// Varint (wire type 0) or fixed-width placeholder (wire types 1 and 5).
    Number(u64),
    /// Length-delimited byte slice (wire type 2).
    Bytes(&'a [u8]),
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
/// assert_eq!(find_field(&msg, 1), Some(FieldValue::Number(42)));
/// assert_eq!(find_field(&msg, 2), None);
/// ```
pub fn find_field(data: &[u8], wanted: u64) -> Option<FieldValue<'_>> {
    let mut pos = 0;
    let mut result = None;
    while pos < data.len() {
        let key = decode_varint(data, &mut pos)?;
        let field_id = key >> 3;
        if field_id == 0 {
            return None;
        }
        let field = match key & 7 {
            0 => FieldValue::Number(decode_varint(data, &mut pos)?),
            2 => {
                let size_u64 = decode_varint(data, &mut pos)?;
                let remaining = data.len() - pos;
                if size_u64 > remaining as u64 {
                    return None;
                }
                let size = size_u64 as usize;
                let slice = &data[pos..pos + size];
                pos += size;
                FieldValue::Bytes(slice)
            }
            1 => {
                if data.len() - pos < 8 {
                    return None;
                }
                pos += 8;
                FieldValue::Number(0)
            }
            5 => {
                if data.len() - pos < 4 {
                    return None;
                }
                pos += 4;
                FieldValue::Number(0)
            }
            _ => return None,
        };
        if field_id == wanted {
            result = Some(field);
        }
    }
    result
}

/// Returns the byte slice for length-delimited field `id`, if present.
pub fn find_bytes(data: &[u8], id: u64) -> Option<&[u8]> {
    match find_field(data, id)? {
        FieldValue::Bytes(bytes) => Some(bytes),
        FieldValue::Number(_) => None,
    }
}

/// Returns the varint value for numeric field `id`, defaulting to `0` if absent or non-numeric.
pub fn find_number(data: &[u8], id: u64) -> u64 {
    match find_field(data, id) {
        Some(FieldValue::Number(n)) => n,
        _ => 0,
    }
}

const FIELD_CHAT: u64 = 1;
const FIELD_CHAT_USAGE: u64 = 4;
const FIELD_USAGE_OUTPUT_TOKENS: u64 = 3;
const FIELD_USAGE_THINKING_OUTPUT_TOKENS: u64 = 9;
const FIELD_CHAT_START: u64 = 9;
const FIELD_START_CTX_WINDOW: u64 = 10;
const FIELD_CTX_ESTIMATED_TOKENS: u64 = 1;
const FIELD_CTX_MAX_TOKENS: u64 = 4;
const FIELD_CHAT_TTFT: u64 = 11;
const FIELD_CHAT_DECODE_DURATION: u64 = 12;
const FIELD_DURATION_SECS: u64 = 1;
const FIELD_DURATION_NANOS: u64 = 2;

const MIN_DECODE_SECS: f64 = 0.1;
const MAX_SPEED_TPS: f64 = 1000.0;
const MAX_TTFT_SECS: f64 = 600.0;

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

fn find_nonzero_number(data: &[u8], id: u64) -> Option<u64> {
    match find_field(data, id)? {
        FieldValue::Number(val) if val > 0 => Some(val),
        _ => None,
    }
}

fn decode_duration_secs(msg: &[u8]) -> Option<f64> {
    let nanos = find_number(msg, FIELD_DURATION_NANOS);
    if nanos >= 1_000_000_000 {
        return None;
    }
    Some((find_number(msg, FIELD_DURATION_SECS) as f64) + (nanos as f64) / 1e9)
}

fn is_sane_decode_speed(output_tokens: u64, decode_secs: f64) -> bool {
    if decode_secs < MIN_DECODE_SECS || output_tokens == 0 {
        return false;
    }
    let speed = (output_tokens as f64) / decode_secs;
    speed.is_finite() && speed > 0.0 && speed < MAX_SPEED_TPS
}

/// Extracts [`TurnMetrics`] (output tokens, optional thinking tokens, decode duration,
/// and optional TTFT) from a `gen_metadata.data` Protobuf blob.
///
/// Enforces unified sanity bounds: `decode_secs >= 0.1`, `output_tokens > 0`,
/// and `0.0 < output_tokens / decode_secs < 1000.0`.
pub fn extract_turn_metrics(data: &[u8]) -> Option<TurnMetrics> {
    let chat = find_bytes(data, FIELD_CHAT)?;
    let usage = find_bytes(chat, FIELD_CHAT_USAGE)?;
    let duration = find_bytes(chat, FIELD_CHAT_DECODE_DURATION)?;
    let decode_secs = decode_duration_secs(duration)?;
    let output_tokens = find_number(usage, FIELD_USAGE_OUTPUT_TOKENS);
    if !is_sane_decode_speed(output_tokens, decode_secs) {
        return None;
    }
    let thinking_tokens = find_nonzero_number(usage, FIELD_USAGE_THINKING_OUTPUT_TOKENS);
    let ttft_secs = find_bytes(chat, FIELD_CHAT_TTFT)
        .and_then(decode_duration_secs)
        .filter(|&t| t > 0.0 && t < MAX_TTFT_SECS);
    Some(TurnMetrics {
        output_tokens,
        thinking_tokens,
        decode_secs,
        ttft_secs,
    })
}

/// Extracts output generation speed (tokens/second) from a `gen_metadata` Protobuf blob.
///
/// Returns `None` if required nested fields are missing, `nanos >= 1_000_000_000`,
/// `seconds < 0.1`, `output == 0`, or the resulting speed is not in `(0.0, 1000.0)`.
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

fn extract_ctx_window_field(data: &[u8], field_id: u64) -> Option<u64> {
    let chat = find_bytes(data, FIELD_CHAT)?;
    let chat_start = find_bytes(chat, FIELD_CHAT_START)?;
    let ctx_window = find_bytes(chat_start, FIELD_START_CTX_WINDOW)?;
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
    fn wire_types_1_and_5_skipped() {
        // field 2 (64-bit, 8 bytes), then field 1 (varint = 7)
        let msg = [0x11, 1, 2, 3, 4, 5, 6, 7, 8, 0x08, 0x07];
        assert_eq!(find_number(&msg, 1), 7);
        // field 2 (32-bit, 4 bytes), then field 1 (varint = 9)
        let msg32 = [0x15, 1, 2, 3, 4, 0x08, 0x09];
        assert_eq!(find_number(&msg32, 1), 9);
        // truncated 64-bit field
        assert_eq!(find_field(&[0x11, 1, 2, 3], 2), None);
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

    #[test]
    fn unified_speed_sanity_bounds() {
        // decode_secs = 0.05s (50_000_000 nanos = 0x02faf080 -> varint 0x80,0xe1,0xeb,0x17) -> rejected (< 0.1s)
        let too_short = [
            0x0a, 0x0b, 0x22, 0x02, 0x18, 0x0a, 0x62, 0x05, 0x10, 0x80, 0xe1, 0xeb, 0x17,
        ];
        assert_eq!(extract_turn_metrics(&too_short), None);
        assert_eq!(extract_metadata_speed(&too_short), None);

        // output = 1000, decode_secs = 1s -> 1000.0 tok/s -> rejected (>= 1000.0)
        let too_fast = [
            0x0a, 0x09, 0x22, 0x03, 0x18, 0xe8, 0x07, 0x62, 0x02, 0x08, 0x01,
        ];
        assert_eq!(extract_turn_metrics(&too_fast), None);
        assert_eq!(extract_metadata_speed(&too_fast), None);
    }
}
