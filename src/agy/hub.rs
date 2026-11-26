//! Opportunistic zero-dependency `daemon/hub_*.json` discovery and loopback HTTP/1.1 Connect RPC client.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Cursor, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::agy::quota::{load_quota_snapshot, save_quota_snapshot, QuotaSnapshot, QuotaSource};
use crate::statusline::QuotaBucket;
use crate::sys::fs::modified_timestamp;
use crate::sys::time::parse_rfc3339_unix_secs;

/// Freshness TTL (seconds) before cached Hub RPC snapshots trigger a new loopback probe.
pub const HUB_RPC_FRESH_TTL_SECS: u64 = 10;
/// Connect timeout for strictly loopback (`127.0.0.1`) Hub RPC probes.
pub const HUB_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
/// Read/write timeout for strictly loopback (`127.0.0.1`) Hub RPC probes.
pub const HUB_IO_TIMEOUT: Duration = Duration::from_millis(500);
/// Maximum HTTP response body bytes accepted from a local Hub RPC endpoint (`256 KiB`).
pub const HUB_MAX_RESPONSE_BODY_BYTES: usize = 256 * 1024;

const HUB_MAX_RESPONSE_HEADER_BYTES: usize = 16 * 1024;
const MAX_HUB_DISCOVERY_FILE_BYTES: u64 = 16 * 1024;
const MAX_CHUNK_LINE_BYTES: usize = 128;
const SIX_HOURS_SECS: i64 = 6 * 3_600;
const SHARED_QUOTA_FRAC_EPS: f64 = 0.015;
const SHARED_QUOTA_RESET_EPS_SECS: i64 = 120;

/// Discovered local Antigravity Hub / Language Server loopback endpoint.
#[derive(Clone, PartialEq, Eq)]
pub struct HubEndpoint {
    /// Owning Hub daemon process ID (`0` if unspecified).
    pub pid: u32,
    /// Loopback HTTP port (`1..=65535`).
    pub http_port: u16,
    /// Connect RPC `x-codeium-csrf-token` header value.
    pub csrf_token: String,
    /// Source `hub_*.json` path on disk.
    pub path: PathBuf,
    mtime: (i64, u32),
}

impl fmt::Debug for HubEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HubEndpoint")
            .field("pid", &self.pid)
            .field("http_port", &self.http_port)
            .field("csrf_token", &"<redacted>")
            .field("path", &self.path)
            .field("mtime", &self.mtime)
            .finish()
    }
}

/// Outcome of [`fetch_or_load_quota_snapshot`].
#[derive(Debug, Clone)]
pub struct HubQuotaResult {
    /// Resolved quota snapshot (live or cached).
    pub snapshot: QuotaSnapshot,
    /// `true` when returned from `quota_snapshot.json` instead of a fresh live RPC call.
    pub from_cache: bool,
    /// `true` when a live loopback probe was attempted during this call.
    pub live_probe_attempted: bool,
    /// Error message from the live probe attempt, if it failed before falling back to cache.
    pub probe_error: Option<String>,
}

/// Checks whether `pid` appears alive using safe `/proc/<pid>` inspection on Linux.
pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if pid == std::process::id() {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        if !Path::new("/proc").is_dir() {
            return true;
        }
        let proc_dir = PathBuf::from(format!("/proc/{pid}"));
        if !fs::metadata(&proc_dir).is_ok_and(|m| m.is_dir()) {
            return false;
        }
        if let Ok(stat) = fs::read_to_string(proc_dir.join("stat")) {
            if let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest.trim_start()) {
                if after_comm.starts_with('Z') {
                    return false;
                }
            }
        }
        true
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

fn is_hub_discovery_filename(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("hub_") && lower.ends_with(".json")
}

fn is_safe_header_token(token: &str) -> bool {
    !token.is_empty() && !token.bytes().any(|b| matches!(b, b'\r' | b'\n' | b'\0'))
}

fn parse_port_value(v: &Value) -> Option<u16> {
    let n = v
        .as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))?;
    u16::try_from(n).ok().filter(|&p| p > 0)
}

fn parse_pid_value(v: &Value) -> Option<u32> {
    let n = v
        .as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))?;
    u32::try_from(n).ok()
}

fn read_hub_endpoint_file<F>(path: &Path, is_alive: &F) -> Option<HubEndpoint>
where
    F: Fn(u32) -> bool,
{
    let meta = fs::symlink_metadata(path).ok()?;
    let ft = meta.file_type();
    if ft.is_symlink() || !ft.is_file() || meta.len() > MAX_HUB_DISCOVERY_FILE_BYTES {
        return None;
    }
    let file = File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(meta.len().min(MAX_HUB_DISCOVERY_FILE_BYTES) as usize);
    file.take(MAX_HUB_DISCOVERY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_HUB_DISCOVERY_FILE_BYTES {
        return None;
    }

    let parsed: Value = serde_json::from_slice(&bytes).ok()?;
    let obj = parsed.as_object()?;
    let csrf_token = obj
        .get("csrfToken")
        .or_else(|| obj.get("csrf_token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| is_safe_header_token(t))?
        .to_string();

    let http_port = obj
        .get("httpPort")
        .or_else(|| obj.get("http_port"))
        .or_else(|| obj.get("port"))
        .and_then(parse_port_value)?;

    let pid = obj.get("pid").and_then(parse_pid_value).unwrap_or(0);
    if pid > 0 && !is_alive(pid) {
        return None;
    }

    Some(HubEndpoint {
        pid,
        http_port,
        csrf_token,
        path: path.to_path_buf(),
        mtime: modified_timestamp(&meta),
    })
}

/// Discovers candidate local Hub endpoints from `daemon_dirs`, filtering out symlinks,
/// non-regular files, and dead PIDs, ordered newest-first.
pub fn discover_hub_endpoints(daemon_dirs: &[PathBuf]) -> Vec<HubEndpoint> {
    discover_hub_endpoints_with(daemon_dirs, is_pid_alive)
}

/// Discovers candidate local Hub endpoints using an explicit PID-liveness predicate.
pub fn discover_hub_endpoints_with<F>(daemon_dirs: &[PathBuf], is_alive: F) -> Vec<HubEndpoint>
where
    F: Fn(u32) -> bool,
{
    let mut endpoints = Vec::new();
    let mut seen: HashSet<(u16, String)> = HashSet::new();

    for dir in daemon_dirs {
        let Ok(dir_meta) = fs::symlink_metadata(dir) else {
            continue;
        };
        let dir_ft = dir_meta.file_type();
        if dir_ft.is_symlink() || !dir_ft.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if !is_hub_discovery_filename(name_str) {
                continue;
            }
            if let Some(ep) = read_hub_endpoint_file(&entry.path(), &is_alive) {
                if seen.insert((ep.http_port, ep.csrf_token.clone())) {
                    endpoints.push(ep);
                }
            }
        }
    }

    endpoints.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| b.pid.cmp(&a.pid)));
    endpoints
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn parse_http_status_code(status_line: &str) -> io::Result<u16> {
    let mut parts = status_line.split_whitespace();
    let version = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty HTTP status line"))?;
    if !version.starts_with("HTTP/1.") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected HTTP response version: {version}"),
        ));
    }
    parts
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP status code"))
}

struct ResponseHeaders {
    status: u16,
    content_length: Option<usize>,
    chunked: bool,
}

fn parse_response_headers(raw_headers: &[u8]) -> io::Result<ResponseHeaders> {
    let header_str = std::str::from_utf8(raw_headers).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "non-UTF-8 HTTP response headers",
        )
    })?;
    let mut lines = header_str.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HTTP status line"))?;
    let status = parse_http_status_code(status_line)?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;

    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name_lower = name.trim().to_ascii_lowercase();
        let val = value.trim();
        if name_lower == "content-length" {
            let parsed = val.parse::<usize>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length header")
            })?;
            if let Some(prev) = content_length {
                if prev != parsed {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "conflicting Content-Length headers",
                    ));
                }
            }
            content_length = Some(parsed);
        } else if name_lower == "transfer-encoding"
            && val
                .split(',')
                .any(|tok| tok.trim().eq_ignore_ascii_case("chunked"))
        {
            chunked = true;
        }
    }

    Ok(ResponseHeaders {
        status,
        content_length,
        chunked,
    })
}

fn read_crlf_line_bounded<R: BufRead>(reader: &mut R, max_len: usize) -> io::Result<String> {
    let mut buf = Vec::with_capacity(32);
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected EOF in chunked body",
            ));
        }
        if let Some(nl) = available.iter().position(|&b| b == b'\n') {
            let take = nl + 1;
            if buf.len() + take > max_len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "chunk header line exceeded maximum length",
                ));
            }
            buf.extend_from_slice(&available[..take]);
            reader.consume(take);
            break;
        }
        let avail_len = available.len();
        if buf.len() + avail_len > max_len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunk header line exceeded maximum length",
            ));
        }
        buf.extend_from_slice(available);
        reader.consume(avail_len);
    }
    let line = std::str::from_utf8(&buf)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 chunk size line"))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn read_chunked_body<R: Read>(reader: R, max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut buffered = BufReader::new(reader);
    let mut out = Vec::new();
    loop {
        let size_line = read_crlf_line_bounded(&mut buffered, MAX_CHUNK_LINE_BYTES)?;
        let hex_part = size_line.split(';').next().unwrap_or("").trim();
        let chunk_size = usize::from_str_radix(hex_part, 16).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid hex chunk size: {hex_part}"),
            )
        })?;
        if chunk_size == 0 {
            break;
        }
        let new_len = out.len().checked_add(chunk_size).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "chunked response overflow")
        })?;
        if new_len > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("response body exceeds {max_bytes} bytes"),
            ));
        }
        let start = out.len();
        out.resize(new_len, 0);
        buffered.read_exact(&mut out[start..new_len])?;
        let mut crlf = [0u8; 2];
        buffered.read_exact(&mut crlf)?;
        if crlf != *b"\r\n" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing CRLF terminator after chunk data",
            ));
        }
    }
    Ok(out)
}

fn read_http_response<R: Read>(mut reader: R, max_body_bytes: usize) -> io::Result<(u16, Vec<u8>)> {
    let mut raw = Vec::with_capacity(1024);
    let mut scratch = [0u8; 2048];
    let header_end = loop {
        if let Some(end) = find_header_end(&raw) {
            break end;
        }
        if raw.len() >= HUB_MAX_RESPONSE_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP response headers exceeded maximum size",
            ));
        }
        let n = reader.read(&mut scratch)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "EOF before end of HTTP response headers",
            ));
        }
        raw.extend_from_slice(&scratch[..n]);
    };

    let headers = parse_response_headers(&raw[..header_end])?;
    let leftover = raw[header_end..].to_vec();
    let mut body_reader = Cursor::new(leftover).chain(reader);

    let body = if headers.chunked {
        read_chunked_body(body_reader, max_body_bytes)?
    } else if let Some(len) = headers.content_length {
        if len > max_body_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Content-Length {len} exceeds {max_body_bytes} bytes"),
            ));
        }
        let mut buf = vec![0u8; len];
        body_reader.read_exact(&mut buf)?;
        buf
    } else {
        let mut buf = Vec::new();
        body_reader
            .take((max_body_bytes + 1) as u64)
            .read_to_end(&mut buf)?;
        if buf.len() > max_body_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("response body exceeds {max_body_bytes} bytes"),
            ));
        }
        buf
    };

    Ok((headers.status, body))
}

/// Executes a single Connect RPC POST request strictly to `127.0.0.1:<port>` using pure safe Rust
/// `TcpStream` HTTP/1.1 with bounded timeouts (`250ms` connect, `500ms` I/O) and a `256 KiB` body cap.
pub fn call_loopback_rpc(
    port: u16,
    csrf_token: &str,
    method: &str,
    body_json: &str,
) -> io::Result<Value> {
    if port == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid loopback port 0",
        ));
    }
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid RPC method name",
        ));
    }
    if !is_safe_header_token(csrf_token) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid CSRF token",
        ));
    }

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, HUB_CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(HUB_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(HUB_IO_TIMEOUT))?;

    let req = format!(
        "POST /exa.language_server_pb.LanguageServerService/{method} HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         x-codeium-csrf-token: {csrf_token}\r\n\
         Connection: close\r\n\
         \r\n\
         {body_json}",
        body_json.len()
    );
    stream.write_all(req.as_bytes())?;
    stream.flush()?;

    let (status, body) = read_http_response(stream, HUB_MAX_RESPONSE_BODY_BYTES)?;
    if !(200..=299).contains(&status) {
        return Err(io::Error::other(format!(
            "Hub RPC {method} returned HTTP {status}"
        )));
    }

    serde_json::from_slice(&body).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid JSON body: {e}"),
        )
    })
}

fn unwrap_rpc_response(val: &Value) -> &Value {
    val.get("response").unwrap_or(val)
}

fn parse_i64_flexible(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_u64().and_then(|u| i64::try_from(u).ok()))
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
        .or_else(|| {
            v.as_f64()
                .filter(|f| f.is_finite())
                .map(|f| f.round() as i64)
        })
}

fn parse_f64_flexible(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
        .filter(|f| f.is_finite())
}

fn tokenize_lower(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn contains_any_token(tokens: &[String], needles: &[&str]) -> bool {
    tokens
        .iter()
        .any(|t| needles.iter().any(|needle| t == needle))
}

fn is_third_party_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let tokens = tokenize_lower(&lower);
    contains_any_token(
        &tokens,
        &["claude", "gpt", "anthropic", "openai", "3p", "other"],
    ) || lower.contains("third-party")
        || lower.contains("third party")
}

fn classify_rpc_pool_slug(
    group_name: &str,
    group_desc: &str,
    bucket_id: &str,
    bucket_name: &str,
) -> String {
    let primary = format!("{bucket_id} {bucket_name} {group_name}");
    let primary_lower = primary.to_ascii_lowercase();
    let desc_lower = group_desc.to_ascii_lowercase();
    let gname_trim = group_name.trim();
    let is_generic_gemini_group = gname_trim.eq_ignore_ascii_case("gemini models");

    let desc_tokens = tokenize_lower(&desc_lower);
    if primary_lower.contains("carbon")
        || (contains_any_token(&desc_tokens, &["carbon"])
            && !contains_any_token(&desc_tokens, &["flash"])
            && !is_generic_gemini_group)
    {
        return "gemini-carbon".to_string();
    }
    if primary_lower.contains("argon")
        || (contains_any_token(&desc_tokens, &["argon"])
            && !contains_any_token(&desc_tokens, &["flash"])
            && !is_generic_gemini_group)
    {
        return "gemini-argon".to_string();
    }
    if is_third_party_text(&primary)
        || (!primary_lower.contains("gemini") && is_third_party_text(group_desc))
    {
        return "3p".to_string();
    }
    if primary_lower.contains("gemini") || desc_lower.contains("gemini") {
        return "gemini".to_string();
    }

    let fallback_tokens = tokenize_lower(if !gname_trim.is_empty() {
        gname_trim
    } else if !bucket_id.trim().is_empty() {
        bucket_id.trim()
    } else {
        "other"
    });
    if fallback_tokens.is_empty() {
        "other".to_string()
    } else {
        fallback_tokens.join("-")
    }
}

fn classify_rpc_bucket_window(bucket: &Value, now_unix_secs: u64) -> &'static str {
    let win = bucket
        .get("window")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let id_and_name = format!(
        "{} {}",
        bucket.get("bucketId").and_then(Value::as_str).unwrap_or(""),
        bucket
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or("")
    )
    .to_ascii_lowercase();

    let win_tokens = tokenize_lower(&win);
    let id_tokens = tokenize_lower(&id_and_name);
    if contains_any_token(&win_tokens, &["5h"])
        || win.contains("five-hour")
        || win.contains("five_hour")
        || contains_any_token(&id_tokens, &["5h"])
        || id_and_name.contains("five-hour")
        || id_and_name.contains("five_hour")
    {
        return "5h";
    }
    if win.contains("week")
        || contains_any_token(&win_tokens, &["7d"])
        || id_and_name.contains("week")
        || contains_any_token(&id_tokens, &["7d"])
    {
        return "weekly";
    }
    if let Some(reset_unix) = bucket
        .get("resetTime")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_unix_secs)
    {
        let diff = reset_unix - (now_unix_secs as i64);
        if diff > 0 && diff <= SIX_HOURS_SECS {
            return "5h";
        }
    }
    "weekly"
}

fn normalize_rpc_quota_bucket(raw: &Value, now_unix_secs: u64) -> Option<QuotaBucket> {
    let obj = raw.as_object()?;
    let disabled = obj.get("disabled").and_then(Value::as_bool);
    let reset_time = obj
        .get("resetTime")
        .or_else(|| obj.get("reset_time"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let reset_in_seconds = obj
        .get("resetInSeconds")
        .or_else(|| obj.get("reset_in_seconds"))
        .and_then(parse_i64_flexible);
    let remaining_amount = obj
        .get("remainingAmount")
        .or_else(|| obj.get("remaining_amount"))
        .and_then(parse_i64_flexible);

    if disabled == Some(true) {
        return Some(QuotaBucket {
            remaining_fraction: Some(0.0),
            remaining_amount,
            reset_in_seconds,
            reset_time,
            disabled: Some(true),
        });
    }

    let remaining_fraction = obj
        .get("remainingFraction")
        .or_else(|| obj.get("remaining_fraction"))
        .and_then(parse_f64_flexible)
        .map(|f| f.clamp(0.0, 1.0));

    let effective_frac = match remaining_fraction {
        Some(f) => Some(f),
        None if remaining_amount.is_none() => {
            // Proto3 zero-value omission: absent fraction with an active future reset means 0%.
            let has_future_reset = reset_time
                .as_deref()
                .and_then(parse_rfc3339_unix_secs)
                .is_some_and(|r| r > now_unix_secs as i64)
                || reset_in_seconds.is_some_and(|s| s > 0);
            has_future_reset.then_some(0.0)
        }
        None => None,
    };

    if effective_frac.is_none() && remaining_amount.is_none() {
        return None;
    }

    Some(QuotaBucket {
        remaining_fraction: effective_frac,
        remaining_amount,
        reset_in_seconds,
        reset_time,
        disabled,
    })
}

fn quotas_match_shared_pool(a: &QuotaBucket, b: &QuotaBucket) -> bool {
    let (Some(fa), Some(fb)) = (a.remaining_fraction, b.remaining_fraction) else {
        return false;
    };
    if (fa - fb).abs() > SHARED_QUOTA_FRAC_EPS {
        return false;
    }
    if let (Some(ra), Some(rb)) = (
        a.reset_time.as_deref().and_then(parse_rfc3339_unix_secs),
        b.reset_time.as_deref().and_then(parse_rfc3339_unix_secs),
    ) {
        if (ra - rb).abs() > SHARED_QUOTA_RESET_EPS_SECS {
            return false;
        }
    }
    true
}

fn extract_tier_and_account(
    user_status: &Value,
) -> (Option<String>, Option<String>, Option<String>, Option<i64>) {
    let email = user_status
        .get("email")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let user_tier = user_status.get("userTier");
    let raw_tier_id = user_tier
        .and_then(|t| t.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let raw_tier_name = user_tier
        .and_then(|t| t.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let raw_tier_desc = user_tier
        .and_then(|t| t.get("description"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let raw_plan_name = user_status
        .get("planStatus")
        .and_then(|ps| ps.get("planInfo"))
        .and_then(|pi| pi.get("planName"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");

    let tier_id_lower = raw_tier_id.to_ascii_lowercase();
    let tier_text = format!("{raw_tier_id} {raw_tier_name} {raw_tier_desc}").to_ascii_lowercase();
    let tier_tokens = tokenize_lower(&tier_text);

    let is_5x = contains_any_token(&tier_tokens, &["5x"])
        || tier_text.contains("ultra-lite")
        || tier_text.contains("ultra_lite")
        || tier_text.contains("ultralite");
    let is_20x = !is_5x
        && (contains_any_token(&tier_tokens, &["20x"])
            || tier_id_lower == "g1-ultra-tier"
            || (tier_id_lower.starts_with("g1-ultra")
                && tier_id_lower.ends_with("-tier")
                && !tier_id_lower.contains("lite")
                && !tier_id_lower.contains("5x")));

    let ultra_tier_label = if is_5x {
        Some("5x".to_string())
    } else if is_20x {
        Some("20x".to_string())
    } else {
        None
    };

    let name_tokens = tokenize_lower(raw_tier_name);
    let is_ultra = contains_any_token(&name_tokens, &["ultra"])
        || tier_id_lower.contains("ultra")
        || ultra_tier_label.is_some();

    let plan_tier = if is_ultra {
        if !raw_plan_name.is_empty() && !raw_plan_name.eq_ignore_ascii_case("pro") {
            Some(raw_plan_name.to_string())
        } else {
            Some("Ultra".to_string())
        }
    } else if !raw_plan_name.is_empty() {
        Some(raw_plan_name.to_string())
    } else if tier_id_lower.contains("pro") || contains_any_token(&name_tokens, &["pro"]) {
        Some("Pro".to_string())
    } else if tier_id_lower.contains("free")
        || tier_id_lower.contains("standard")
        || contains_any_token(&name_tokens, &["free"])
    {
        Some("Free".to_string())
    } else {
        None
    };

    let ai_credits = user_tier
        .and_then(|t| t.get("availableCredits"))
        .and_then(Value::as_array)
        .and_then(|credits| {
            credits.iter().find_map(|entry| {
                let kind = entry.get("creditType").and_then(Value::as_str)?;
                if kind == "GOOGLE_ONE_AI" {
                    entry.get("creditAmount").and_then(parse_i64_flexible)
                } else {
                    None
                }
            })
        });

    (email, plan_tier, ultra_tier_label, ai_credits)
}

fn insert_pool_bucket(
    quota: &mut BTreeMap<String, QuotaBucket>,
    slug: &str,
    window: &str,
    bucket: QuotaBucket,
    now_unix_secs: u64,
) {
    let key = format!("{slug}-{window}");
    if let Some(existing) = quota.get(&key) {
        if window == "5h" {
            let weekly_key = format!("{slug}-weekly");
            if !quota.contains_key(&weekly_key) {
                let prev_reset = existing
                    .reset_time
                    .as_deref()
                    .and_then(parse_rfc3339_unix_secs)
                    .unwrap_or(i64::MAX);
                let next_reset = bucket
                    .reset_time
                    .as_deref()
                    .and_then(parse_rfc3339_unix_secs)
                    .unwrap_or(i64::MAX);
                if next_reset < prev_reset && next_reset > now_unix_secs as i64 {
                    let moved = existing.clone();
                    quota.insert(key, bucket);
                    quota.insert(weekly_key, moved);
                } else {
                    quota.insert(weekly_key, bucket);
                }
                return;
            }
        }
        return;
    }
    quota.insert(key, bucket);
}

/// Parses `RetrieveUserQuotaSummary` and `GetUserStatus` JSON responses into a unified [`QuotaSnapshot`].
pub fn parse_hub_quota_responses(
    quota_summary_resp: Option<&Value>,
    user_status_resp: Option<&Value>,
    now_unix_secs: u64,
) -> QuotaSnapshot {
    let mut quota: BTreeMap<String, QuotaBucket> = BTreeMap::new();

    if let Some(raw_summary) = quota_summary_resp.map(unwrap_rpc_response) {
        if let Some(groups) = raw_summary.get("groups").and_then(Value::as_array) {
            for grp in groups {
                let group_name = grp.get("displayName").and_then(Value::as_str).unwrap_or("");
                let group_desc = grp.get("description").and_then(Value::as_str).unwrap_or("");
                let Some(buckets) = grp.get("buckets").and_then(Value::as_array) else {
                    continue;
                };
                for b in buckets {
                    let Some(norm) = normalize_rpc_quota_bucket(b, now_unix_secs) else {
                        continue;
                    };
                    let bucket_id = b.get("bucketId").and_then(Value::as_str).unwrap_or("");
                    let bucket_name = b.get("displayName").and_then(Value::as_str).unwrap_or("");
                    let slug =
                        classify_rpc_pool_slug(group_name, group_desc, bucket_id, bucket_name);
                    let window = classify_rpc_bucket_window(b, now_unix_secs);
                    insert_pool_bucket(&mut quota, &slug, window, norm, now_unix_secs);
                }
            }
        } else if let Some(buckets) = raw_summary.get("buckets").and_then(Value::as_array) {
            for b in buckets {
                let Some(norm) = normalize_rpc_quota_bucket(b, now_unix_secs) else {
                    continue;
                };
                let bucket_id = b.get("bucketId").and_then(Value::as_str).unwrap_or("");
                let bucket_name = b.get("displayName").and_then(Value::as_str).unwrap_or("");
                let slug = classify_rpc_pool_slug("", "", bucket_id, bucket_name);
                let window = classify_rpc_bucket_window(b, now_unix_secs);
                insert_pool_bucket(&mut quota, &slug, window, norm, now_unix_secs);
            }
        }
    }

    let user_status = user_status_resp
        .map(unwrap_rpc_response)
        .and_then(|v| v.get("userStatus").or(Some(v)));

    let (email, plan_tier, ultra_tier_label, ai_credits) = user_status
        .map(extract_tier_and_account)
        .unwrap_or((None, None, None, None));

    // Fallback inference from `clientModelConfigs.quotaInfo` for Pro Argon/Carbon pools
    // or when `RetrieveUserQuotaSummary` returned no buckets for a family.
    if let Some(configs) = user_status
        .and_then(|u| u.get("cascadeModelConfigData"))
        .and_then(|d| d.get("clientModelConfigs"))
        .and_then(Value::as_array)
    {
        let mut sample_std_gemini: Option<QuotaBucket> = None;
        let mut sample_argon: Option<QuotaBucket> = None;
        let mut sample_carbon: Option<QuotaBucket> = None;
        let mut sample_3p: Option<QuotaBucket> = None;

        for cfg in configs {
            let Some(qinfo) = cfg.get("quotaInfo") else {
                continue;
            };
            let Some(norm) = normalize_rpc_quota_bucket(qinfo, now_unix_secs) else {
                continue;
            };
            let ident = format!(
                "{} {} {}",
                cfg.get("modelOrAlias")
                    .and_then(|m| m.get("model"))
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                cfg.get("modelId").and_then(Value::as_str).unwrap_or(""),
                cfg.get("label").and_then(Value::as_str).unwrap_or("")
            )
            .to_ascii_lowercase();
            if ident.contains("carbon") {
                if sample_carbon.is_none() {
                    sample_carbon = Some(norm);
                }
            } else if ident.contains("argon") {
                if sample_argon.is_none() {
                    sample_argon = Some(norm);
                }
            } else if ident.contains("gemini") {
                if sample_std_gemini.is_none() {
                    sample_std_gemini = Some(norm);
                }
            } else if is_third_party_text(&ident) && sample_3p.is_none() {
                sample_3p = Some(norm);
            }
        }

        if let Some(argon) = sample_argon.as_ref() {
            let has_argon =
                quota.contains_key("gemini-argon-5h") || quota.contains_key("gemini-argon-weekly");
            let matches_gemini = sample_std_gemini
                .as_ref()
                .is_some_and(|g| quotas_match_shared_pool(argon, g))
                || quota
                    .get("gemini-5h")
                    .is_some_and(|g| quotas_match_shared_pool(argon, g))
                || quota
                    .get("gemini-weekly")
                    .is_some_and(|g| quotas_match_shared_pool(argon, g));
            let has_baseline = sample_std_gemini.is_some()
                || quota.contains_key("gemini-5h")
                || quota.contains_key("gemini-weekly");
            if !has_argon && has_baseline && !matches_gemini {
                quota.insert("gemini-argon-weekly".to_string(), argon.clone());
            }
        }

        if let Some(carbon) = sample_carbon.as_ref() {
            let has_carbon = quota.contains_key("gemini-carbon-5h")
                || quota.contains_key("gemini-carbon-weekly");
            let matches_other = sample_std_gemini
                .as_ref()
                .is_some_and(|g| quotas_match_shared_pool(carbon, g))
                || sample_argon
                    .as_ref()
                    .is_some_and(|a| quotas_match_shared_pool(carbon, a))
                || quota
                    .get("gemini-5h")
                    .is_some_and(|g| quotas_match_shared_pool(carbon, g))
                || quota
                    .get("gemini-weekly")
                    .is_some_and(|g| quotas_match_shared_pool(carbon, g))
                || quota
                    .get("gemini-argon-weekly")
                    .is_some_and(|a| quotas_match_shared_pool(carbon, a));
            let has_baseline = sample_std_gemini.is_some()
                || sample_argon.is_some()
                || quota.contains_key("gemini-5h")
                || quota.contains_key("gemini-weekly");
            if !has_carbon && has_baseline && !matches_other {
                quota.insert("gemini-carbon-weekly".to_string(), carbon.clone());
            }
        }

        if !quota.contains_key("gemini-5h") && !quota.contains_key("gemini-weekly") {
            if let Some(g) = sample_std_gemini {
                let win = if g
                    .reset_time
                    .as_deref()
                    .and_then(parse_rfc3339_unix_secs)
                    .is_some_and(|r| r - (now_unix_secs as i64) > SIX_HOURS_SECS)
                {
                    "weekly"
                } else {
                    "5h"
                };
                quota.insert(format!("gemini-{win}"), g);
            }
        }

        if !quota.contains_key("3p-5h") && !quota.contains_key("3p-weekly") {
            if let Some(tp) = sample_3p {
                let win = if tp
                    .reset_time
                    .as_deref()
                    .and_then(parse_rfc3339_unix_secs)
                    .is_some_and(|r| r - (now_unix_secs as i64) > SIX_HOURS_SECS)
                {
                    "weekly"
                } else {
                    "5h"
                };
                quota.insert(format!("3p-{win}"), tp);
            }
        }
    }

    if let Some(credits) = ai_credits {
        quota.insert(
            "google_one_ai".to_string(),
            QuotaBucket {
                remaining_amount: Some(credits),
                ..Default::default()
            },
        );
    }

    let mut snap = QuotaSnapshot {
        updated_at: now_unix_secs,
        source: QuotaSource::HubRpc,
        plan_tier,
        ultra_tier_label,
        email,
        ai_credits,
        quota,
        history: BTreeMap::new(),
    };
    snap.record_bucket_samples(now_unix_secs);
    snap
}

/// Probes a single discovered [`HubEndpoint`] for `RetrieveUserQuotaSummary` and `GetUserStatus`.
pub fn probe_hub_endpoint(endpoint: &HubEndpoint, now_unix_secs: u64) -> io::Result<QuotaSnapshot> {
    let summary_res = call_loopback_rpc(
        endpoint.http_port,
        &endpoint.csrf_token,
        "RetrieveUserQuotaSummary",
        r#"{"forceRefresh":true}"#,
    );
    let status_res = call_loopback_rpc(
        endpoint.http_port,
        &endpoint.csrf_token,
        "GetUserStatus",
        "{}",
    );

    match (summary_res, status_res) {
        (Err(e1), Err(_)) => Err(e1),
        (summary_ok, status_ok) => Ok(parse_hub_quota_responses(
            summary_ok.ok().as_ref(),
            status_ok.ok().as_ref(),
            now_unix_secs,
        )),
    }
}

/// Loads a fresh (`< 10s`) cached Hub RPC snapshot when available, otherwise opportunistically
/// probes local `daemon/hub_*.json` loopback endpoints and persists the merged snapshot to
/// `quota_snapshot.json` (falling back to any existing cached snapshot if probing fails).
pub fn fetch_or_load_quota_snapshot(
    daemon_dirs: &[PathBuf],
    cache_dir: Option<&Path>,
    now_unix_secs: u64,
    force_refresh: bool,
) -> Option<HubQuotaResult> {
    let cached = cache_dir.and_then(|dir| load_quota_snapshot(dir).ok().flatten());
    if !force_refresh {
        if let Some(ref snap) = cached {
            if snap.source == QuotaSource::HubRpc
                && now_unix_secs.saturating_sub(snap.updated_at) < HUB_RPC_FRESH_TTL_SECS
            {
                return Some(HubQuotaResult {
                    snapshot: snap.clone(),
                    from_cache: true,
                    live_probe_attempted: false,
                    probe_error: None,
                });
            }
        }
    }

    let endpoints = discover_hub_endpoints(daemon_dirs);
    let mut last_err: Option<String> = None;

    for endpoint in &endpoints {
        match probe_hub_endpoint(endpoint, now_unix_secs) {
            Ok(incoming) => {
                let merged = match cached {
                    Some(mut prev) => {
                        prev.merge_observation(incoming, now_unix_secs);
                        prev
                    }
                    None => incoming,
                };
                if let Some(dir) = cache_dir {
                    let _ = save_quota_snapshot(dir, &merged);
                }
                return Some(HubQuotaResult {
                    snapshot: merged,
                    from_cache: false,
                    live_probe_attempted: true,
                    probe_error: None,
                });
            }
            Err(e) => {
                last_err = Some(e.to_string());
            }
        }
    }

    let probe_error = last_err.or_else(|| {
        if endpoints.is_empty() {
            Some("no active Hub daemon found".to_string())
        } else {
            None
        }
    });

    cached.map(|snapshot| HubQuotaResult {
        snapshot,
        from_cache: true,
        live_probe_attempted: !endpoints.is_empty(),
        probe_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ultra_5x_and_20x_tiers_google_one_ai_credits_and_multi_pools() {
        let now = 1_795_000_000u64;
        let summary = serde_json::json!({
            "groups": [
                {
                    "displayName": "Gemini Models",
                    "description": "Shared Gemini quota",
                    "buckets": [
                        {
                            "bucketId": "gemini-5h",
                            "remainingFraction": 0.65,
                            "resetTime": "2026-11-18T14:00:00Z"
                        },
                        {
                            "bucketId": "gemini-weekly",
                            "resetTime": "2026-11-24T12:00:00Z"
                        }
                    ]
                },
                {
                    "displayName": "Claude & GPT",
                    "description": "Third party models separate from Gemini",
                    "buckets": [
                        {
                            "bucketId": "3p-5h",
                            "remainingFraction": 0.40,
                            "resetTime": "2026-11-18T13:00:00Z"
                        }
                    ]
                }
            ]
        });
        let status_20x = serde_json::json!({
            "userStatus": {
                "email": "ultra@example.com",
                "userTier": {
                    "id": "g1-ultra-tier",
                    "name": "Google AI Ultra",
                    "description": "20x quota",
                    "availableCredits": [
                        {"creditType": "GOOGLE_ONE_AI", "creditAmount": "1250"}
                    ]
                },
                "planStatus": {
                    "planInfo": {
                        "planName": "Pro"
                    }
                }
            }
        });

        let snap = parse_hub_quota_responses(Some(&summary), Some(&status_20x), now);
        assert_eq!(snap.plan_tier.as_deref(), Some("Ultra"));
        assert_eq!(snap.ultra_tier_label.as_deref(), Some("20x"));
        assert_eq!(snap.display_plan_tier().as_deref(), Some("Ultra 20x"));
        assert_eq!(snap.ai_credits, Some(1250));
        assert_eq!(snap.email.as_deref(), Some("ultra@example.com"));
        assert_eq!(snap.quota["gemini-5h"].remaining_fraction, Some(0.65));
        // Proto3 omitted 0% remainingFraction with future reset -> 0.0.
        assert_eq!(snap.quota["gemini-weekly"].remaining_fraction, Some(0.0));
        assert_eq!(snap.quota["3p-5h"].remaining_fraction, Some(0.40));
        assert_eq!(snap.quota["google_one_ai"].remaining_amount, Some(1250));

        let status_5x = serde_json::json!({
            "userStatus": {
                "userTier": {
                    "id": "g1-ultra-lite-tier",
                    "name": "Google AI Ultra",
                    "description": "5x quota"
                },
                "planStatus": {
                    "planInfo": {
                        "planName": "Pro"
                    }
                }
            }
        });
        let snap_5x = parse_hub_quota_responses(None, Some(&status_5x), now);
        assert_eq!(snap_5x.plan_tier.as_deref(), Some("Ultra"));
        assert_eq!(snap_5x.ultra_tier_label.as_deref(), Some("5x"));
        assert_eq!(snap_5x.display_plan_tier().as_deref(), Some("Ultra 5x"));
    }

    #[test]
    fn infers_pro_argon_and_carbon_pools_from_client_model_configs() {
        let now = 1_795_000_000u64;
        let summary = serde_json::json!({
            "groups": [
                {
                    "displayName": "Gemini Models",
                    "buckets": [
                        {
                            "bucketId": "gemini-5h",
                            "remainingFraction": 0.90,
                            "resetTime": "2026-11-18T14:00:00Z"
                        }
                    ]
                }
            ]
        });
        let status = serde_json::json!({
            "userStatus": {
                "userTier": { "id": "g1-pro-tier", "name": "Google AI Pro" },
                "cascadeModelConfigData": {
                    "clientModelConfigs": [
                        {
                            "label": "Gemini 4 Flash",
                            "modelId": "gemini-4-flash",
                            "quotaInfo": { "remainingFraction": 0.90, "resetTime": "2026-11-18T14:00:00Z" }
                        },
                        {
                            "label": "Gemini 4 Argon",
                            "modelId": "gemini-4-argon",
                            "quotaInfo": { "remainingFraction": 0.55, "resetTime": "2026-11-24T12:00:00Z" }
                        },
                        {
                            "label": "Gemini 4 Carbon",
                            "modelId": "gemini-4-carbon",
                            "quotaInfo": { "remainingFraction": 0.25, "resetTime": "2026-11-24T12:00:00Z" }
                        }
                    ]
                }
            }
        });

        let snap = parse_hub_quota_responses(Some(&summary), Some(&status), now);
        assert_eq!(snap.plan_tier.as_deref(), Some("Pro"));
        assert_eq!(snap.quota["gemini-5h"].remaining_fraction, Some(0.90));
        assert_eq!(
            snap.quota["gemini-argon-weekly"].remaining_fraction,
            Some(0.55)
        );
        assert_eq!(
            snap.quota["gemini-carbon-weekly"].remaining_fraction,
            Some(0.25)
        );
    }

    #[test]
    fn http_response_parser_decodes_content_length_chunked_and_enforces_cap() {
        let cl_resp = b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n{\"ok\":true}  ";
        let (status, body) = read_http_response(Cursor::new(cl_resp.as_slice()), 1024).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"{\"ok\":true}  ");

        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"ok\"\r\n6\r\n:true}\r\n0\r\n\r\n";
        let (c_status, c_body) = read_http_response(Cursor::new(chunked.as_slice()), 1024).unwrap();
        assert_eq!(c_status, 200);
        assert_eq!(c_body, b"{\"ok\":true}");

        let oversized = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n";
        let err = read_http_response(Cursor::new(oversized.as_slice()), 32).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn discover_hub_endpoints_filters_dead_pids_invalid_tokens_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = dir.path().join("daemon");
        fs::create_dir_all(&daemon).unwrap();

        fs::write(
            daemon.join("hub_live.json"),
            r#"{"pid":1234,"httpPort":41001,"csrfToken":"tok-good"}"#,
        )
        .unwrap();
        fs::write(
            daemon.join("hub_dead.json"),
            r#"{"pid":9999,"httpPort":41002,"csrfToken":"tok-dead"}"#,
        )
        .unwrap();
        fs::write(
            daemon.join("hub_inject.json"),
            "{\"pid\":1234,\"httpPort\":41003,\"csrfToken\":\"bad\\r\\ntoken\"}",
        )
        .unwrap();
        fs::write(
            daemon.join("other.json"),
            r#"{"pid":1234,"httpPort":41004,"csrfToken":"ignored"}"#,
        )
        .unwrap();

        let eps = discover_hub_endpoints_with(&[daemon], |pid| pid == 1234);
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].http_port, 41001);
        assert_eq!(eps[0].csrf_token, "tok-good");
        let dbg = format!("{:?}", eps[0]);
        assert!(dbg.contains("<redacted>"));
        assert!(!dbg.contains("tok-good"));
    }

    #[cfg(unix)]
    #[test]
    fn discover_hub_endpoints_rejects_symlinked_hub_files_and_dirs() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let daemon = dir.path().join("daemon");
        fs::create_dir_all(&daemon).unwrap();
        let external = dir.path().join("external_hub.json");
        fs::write(
            &external,
            r#"{"pid":1234,"httpPort":41099,"csrfToken":"tok-sym"}"#,
        )
        .unwrap();
        symlink(&external, daemon.join("hub_sym.json")).unwrap();

        let eps = discover_hub_endpoints_with(std::slice::from_ref(&daemon), |_| true);
        assert!(eps.is_empty());

        let sym_dir = dir.path().join("sym_daemon");
        fs::write(
            daemon.join("hub_real.json"),
            r#"{"pid":1234,"httpPort":41098,"csrfToken":"tok-real"}"#,
        )
        .unwrap();
        symlink(&daemon, &sym_dir).unwrap();
        let sym_eps = discover_hub_endpoints_with(&[sym_dir], |_| true);
        assert!(sym_eps.is_empty());
    }
}
