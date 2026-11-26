#![forbid(unsafe_code)]
//! Tier 2: live loopback Hub RPC boundary tests (`127.0.0.1:0`), snapshot caching, `doctor`, and exit-0 guard.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use agy_statusline::agy::hub::{
    call_loopback_rpc, discover_hub_endpoints, fetch_or_load_quota_snapshot, HUB_RPC_FRESH_TTL_SECS,
};
use agy_statusline::agy::quota::{load_quota_snapshot, QuotaSource};

use common::{isolated_agy_cmd, run_statusline_stdin};

#[derive(Clone)]
struct RouteReply {
    status: u16,
    chunked: bool,
    body: String,
}

struct LoopbackHubServer {
    port: u16,
    shutdown: Arc<AtomicBool>,
    requests: Arc<AtomicUsize>,
    methods_seen: Arc<Mutex<Vec<String>>>,
    handle: Option<JoinHandle<()>>,
}

impl LoopbackHubServer {
    fn start(expected_csrf: &str, routes: BTreeMap<String, RouteReply>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));
        let methods_seen = Arc::new(Mutex::new(Vec::new()));

        let csrf = expected_csrf.to_string();
        let shutdown_bg = Arc::clone(&shutdown);
        let requests_bg = Arc::clone(&requests);
        let methods_bg = Arc::clone(&methods_seen);

        let handle = thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(mut stream) = incoming else {
                    break;
                };
                if shutdown_bg.load(Ordering::Acquire) {
                    break;
                }
                handle_single_connection(&mut stream, &csrf, &routes, &requests_bg, &methods_bg);
            }
        });

        Self {
            port,
            shutdown,
            requests,
            methods_seen,
            handle: Some(handle),
        }
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::Acquire)
    }

    fn methods_seen(&self) -> Vec<String> {
        self.methods_seen.lock().unwrap().clone()
    }

    fn stop(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for LoopbackHubServer {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

fn read_http_request(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 16 * 1024 {
            return None;
        }
    };
    let head_str = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = head_str.split("\r\n");
    let req_line = lines.next()?;
    let path = req_line.split_whitespace().nth(1)?.to_string();
    let mut csrf = String::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("x-codeium-csrf-token") {
                csrf = v.trim().to_string();
            } else if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse::<usize>().unwrap_or(0);
            }
        }
    }
    let mut already = buf.len() - header_end;
    while already < content_length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        already += n;
    }
    Some((path, csrf))
}

fn write_http_reply(stream: &mut TcpStream, reply: &RouteReply) {
    let reason = match reply.status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    if reply.chunked {
        let bytes = reply.body.as_bytes();
        let split = bytes.len() / 2;
        let part1 = &bytes[..split];
        let part2 = &bytes[split..];
        let mut resp = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            reply.status
        )
        .into_bytes();
        if !part1.is_empty() {
            resp.extend_from_slice(format!("{:x}\r\n", part1.len()).as_bytes());
            resp.extend_from_slice(part1);
            resp.extend_from_slice(b"\r\n");
        }
        if !part2.is_empty() {
            resp.extend_from_slice(format!("{:x}\r\n", part2.len()).as_bytes());
            resp.extend_from_slice(part2);
            resp.extend_from_slice(b"\r\n");
        }
        resp.extend_from_slice(b"0\r\n\r\n");
        let _ = stream.write_all(&resp);
    } else {
        let resp = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            reply.status,
            reply.body.len(),
            reply.body
        );
        let _ = stream.write_all(resp.as_bytes());
    }
    let _ = stream.flush();
}

fn handle_single_connection(
    stream: &mut TcpStream,
    expected_csrf: &str,
    routes: &BTreeMap<String, RouteReply>,
    requests: &AtomicUsize,
    methods_seen: &Mutex<Vec<String>>,
) {
    let Some((path, csrf)) = read_http_request(stream) else {
        return;
    };
    requests.fetch_add(1, Ordering::AcqRel);
    let method = path.rsplit('/').next().unwrap_or("").to_string();
    methods_seen.lock().unwrap().push(method.clone());

    if csrf != expected_csrf {
        write_http_reply(
            stream,
            &RouteReply {
                status: 403,
                chunked: false,
                body: r#"{"error":"invalid csrf"}"#.to_string(),
            },
        );
        return;
    }

    let reply = routes.get(&method).cloned().unwrap_or(RouteReply {
        status: 404,
        chunked: false,
        body: r#"{"error":"unknown method"}"#.to_string(),
    });
    write_http_reply(stream, &reply);
}

fn write_hub_file(daemon_dir: &Path, filename: &str, port: u16, csrf: &str) {
    fs::create_dir_all(daemon_dir).unwrap();
    let pid = std::process::id();
    let payload = format!(r#"{{"pid":{pid},"httpPort":{port},"csrfToken":"{csrf}"}}"#);
    fs::write(daemon_dir.join(filename), payload).unwrap();
}

fn sample_quota_routes(chunked_summary: bool) -> BTreeMap<String, RouteReply> {
    BTreeMap::from([
        (
            "RetrieveUserQuotaSummary".to_string(),
            RouteReply {
                status: 200,
                chunked: chunked_summary,
                body: serde_json::json!({
                    "groups": [
                        {
                            "displayName": "Gemini Models",
                            "description": "Shared Gemini quota",
                            "buckets": [
                                {
                                    "bucketId": "gemini-5h",
                                    "remainingFraction": 0.72,
                                    "resetTime": "2026-11-26T16:00:00Z"
                                },
                                {
                                    "bucketId": "gemini-weekly",
                                    "resetTime": "2026-12-01T00:00:00Z"
                                }
                            ]
                        },
                        {
                            "displayName": "Gemini 4 Argon",
                            "buckets": [
                                {
                                    "bucketId": "argon-weekly",
                                    "remainingFraction": 0.55,
                                    "resetTime": "2026-12-01T00:00:00Z"
                                }
                            ]
                        },
                        {
                            "displayName": "Claude & GPT",
                            "description": "Separate from Gemini",
                            "buckets": [
                                {
                                    "bucketId": "3p-5h",
                                    "remainingFraction": 0.35,
                                    "resetTime": "2026-11-26T14:00:00Z"
                                }
                            ]
                        }
                    ]
                })
                .to_string(),
            },
        ),
        (
            "GetUserStatus".to_string(),
            RouteReply {
                status: 200,
                chunked: !chunked_summary,
                body: serde_json::json!({
                    "userStatus": {
                        "email": "alice@example.com",
                        "userTier": {
                            "id": "g1-ultra-tier",
                            "name": "Google AI Ultra",
                            "description": "20x plan",
                            "availableCredits": [
                                { "creditType": "GOOGLE_ONE_AI", "creditAmount": "480" }
                            ]
                        },
                        "planStatus": {
                            "planInfo": {
                                "planName": "Pro"
                            }
                        }
                    }
                })
                .to_string(),
            },
        ),
    ])
}

#[test]
fn tier2_loopback_hub_rpc_fetches_chunked_and_content_length_caches_and_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let daemon_dir = dir.path().join("daemon");
    let cache_dir = dir.path().join("cache");
    let now = 1_795_690_000u64;

    let server = LoopbackHubServer::start("secret-csrf-token", sample_quota_routes(true));
    write_hub_file(
        &daemon_dir,
        "hub_test.json",
        server.port(),
        "secret-csrf-token",
    );

    let discovered = discover_hub_endpoints(std::slice::from_ref(&daemon_dir));
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].http_port, server.port());

    // First call hits live loopback server (chunked RetrieveUserQuotaSummary + Content-Length GetUserStatus).
    let first = fetch_or_load_quota_snapshot(
        std::slice::from_ref(&daemon_dir),
        Some(&cache_dir),
        now,
        false,
    )
    .unwrap();
    assert!(!first.from_cache);
    assert!(first.live_probe_attempted);
    assert_eq!(first.probe_error, None);
    assert_eq!(server.request_count(), 2);
    assert_eq!(
        server.methods_seen(),
        vec!["RetrieveUserQuotaSummary", "GetUserStatus"]
    );
    assert_eq!(first.snapshot.source, QuotaSource::HubRpc);
    assert_eq!(first.snapshot.plan_tier.as_deref(), Some("Ultra"));
    assert_eq!(first.snapshot.ultra_tier_label.as_deref(), Some("20x"));
    assert_eq!(first.snapshot.email.as_deref(), Some("alice@example.com"));
    assert_eq!(first.snapshot.ai_credits, Some(480));
    assert_eq!(
        first.snapshot.quota["gemini-5h"].remaining_fraction,
        Some(0.72)
    );
    // Proto3 omitted 0% remainingFraction with future reset -> 0.0.
    assert_eq!(
        first.snapshot.quota["gemini-weekly"].remaining_fraction,
        Some(0.0)
    );
    assert_eq!(
        first.snapshot.quota["gemini-argon-weekly"].remaining_fraction,
        Some(0.55)
    );
    assert_eq!(first.snapshot.quota["3p-5h"].remaining_fraction, Some(0.35));

    // Verify persisted quota_snapshot.json on disk.
    let persisted = load_quota_snapshot(&cache_dir).unwrap().unwrap();
    assert_eq!(persisted.updated_at, now);
    assert_eq!(persisted.ai_credits, Some(480));
    assert_eq!(persisted.history["gemini-5h"].samples.len(), 1);

    // Second call within 10s fresh TTL returns cached snapshot without hitting the network.
    let cached_hit = fetch_or_load_quota_snapshot(
        std::slice::from_ref(&daemon_dir),
        Some(&cache_dir),
        now + HUB_RPC_FRESH_TTL_SECS - 1,
        false,
    )
    .unwrap();
    assert!(cached_hit.from_cache);
    assert!(!cached_hit.live_probe_attempted);
    assert_eq!(server.request_count(), 2);

    // Shut down the server and advance past TTL -> falls back to cached snapshot cleanly.
    server.stop();
    let fallback = fetch_or_load_quota_snapshot(
        &[daemon_dir],
        Some(&cache_dir),
        now + HUB_RPC_FRESH_TTL_SECS + 5,
        false,
    )
    .unwrap();
    assert!(fallback.from_cache);
    assert!(fallback.live_probe_attempted);
    assert!(fallback.probe_error.is_some());
    assert_eq!(fallback.snapshot.ai_credits, Some(480));
}

#[test]
fn tier2_loopback_hub_rpc_rejects_bad_csrf_and_accepts_valid_token() {
    let server = LoopbackHubServer::start("expected-token", sample_quota_routes(false));

    let bad_err =
        call_loopback_rpc(server.port(), "wrong-token", "GetUserStatus", "{}").unwrap_err();
    assert!(bad_err.to_string().contains("HTTP 403"));

    let ok_val = call_loopback_rpc(server.port(), "expected-token", "GetUserStatus", "{}").unwrap();
    assert_eq!(
        ok_val["userStatus"]["email"].as_str(),
        Some("alice@example.com")
    );
    assert_eq!(server.request_count(), 2);

    server.stop();
}

#[test]
fn tier2_doctor_cli_displays_live_hub_rpc_quota_and_multi_root_stores() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let cli_daemon = root.join(".gemini/antigravity-cli/daemon");
    let desk_conv = root.join(".gemini/antigravity/conversations");
    fs::create_dir_all(&desk_conv).unwrap();
    fs::write(
        desk_conv.join("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.db"),
        b"desktop-db",
    )
    .unwrap();

    let server = LoopbackHubServer::start("doctor-csrf", sample_quota_routes(false));
    write_hub_file(&cli_daemon, "hub_doctor.json", server.port(), "doctor-csrf");

    let output = isolated_agy_cmd(root)
        .env("NO_COLOR", "1")
        .arg("doctor")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();

    assert!(text.contains("Tools:    git="), "doctor stdout:\n{text}");
    assert!(
        text.contains("Sessions (desktop):"),
        "doctor stdout:\n{text}"
    );
    assert!(
        text.contains("found, 1 session .db file"),
        "doctor stdout:\n{text}"
    );
    assert!(
        text.contains("Quota:    live Hub RPC (fresh) · tier=Ultra 20x · credits=480"),
        "doctor stdout:\n{text}"
    );
    assert!(text.contains("Pools:"), "doctor stdout:\n{text}");
    assert!(text.contains("Gemini"), "doctor stdout:\n{text}");
    assert!(text.contains("Argon"), "doctor stdout:\n{text}");
    assert!(text.contains("Claude/3P"), "doctor stdout:\n{text}");

    server.stop();
}

#[test]
fn tier2_statusline_stdin_exits_zero_on_malformed_and_empty_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let rendered_empty = run_statusline_stdin(dir.path(), &["--preset=minimal"], "");
    assert!(rendered_empty.contains("READY"));

    let rendered_bad = run_statusline_stdin(dir.path(), &["--preset=minimal"], "{not-valid-json");
    assert!(rendered_bad.contains("READY"));
}
