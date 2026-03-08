//! REST API server for CLI and tool integration.
//!
//! Replaces the old health-only TCP server with a full API.
//! All endpoints return JSON. Media endpoints accept local file paths.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::bridge::{BridgeState, WhatsAppBridge};
use crate::qr::QrRender;

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

struct HttpRequest {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn query_get(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Unknown",
    };
    let header = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut resp = header.into_bytes();
    resp.extend_from_slice(body);
    resp
}

fn json_response(status: u16, body: &str) -> Vec<u8> {
    http_response(status, "application/json", body.as_bytes())
}

fn json_ok(data: serde_json::Value) -> Vec<u8> {
    let mut map = match data {
        serde_json::Value::Object(m) => m,
        other => {
            let mut m = serde_json::Map::new();
            m.insert("data".to_string(), other);
            m
        }
    };
    map.insert("ok".to_string(), serde_json::Value::Bool(true));
    json_response(200, &serde_json::Value::Object(map).to_string())
}

fn json_ok_id(id: &str) -> Vec<u8> {
    json_response(200, &json!({"ok": true, "id": id}).to_string())
}

fn json_ok_simple() -> Vec<u8> {
    json_response(200, r#"{"ok":true}"#)
}

fn json_err(status: u16, msg: &str) -> Vec<u8> {
    json_response(status, &json!({"ok": false, "error": msg}).to_string())
}

fn parse_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, Vec<u8>> {
    serde_json::from_slice(body).map_err(|e| json_err(400, &format!("invalid JSON: {e}")))
}

// ---------------------------------------------------------------------------
// Request parsing
// ---------------------------------------------------------------------------

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<HttpRequest> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];

    // Read until end of headers
    let header_end;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut tmp)).await {
            Ok(Ok(0)) | Err(_) => return None,
            Ok(Ok(n)) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = pos;
                    break;
                }
                if buf.len() > 128 * 1024 {
                    return None; // headers too large
                }
            }
            Ok(Err(_)) => return None,
        }
    }

    let headers_str = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = headers_str.lines();

    // Parse request line
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let raw_path = parts.next()?.to_string();

    // Split path and query
    let (path, query) = if let Some(idx) = raw_path.find('?') {
        let q = raw_path[idx + 1..]
            .split('&')
            .filter_map(|pair| {
                let mut kv = pair.splitn(2, '=');
                Some((kv.next()?.to_string(), kv.next().unwrap_or("").to_string()))
            })
            .collect();
        (raw_path[..idx].to_string(), q)
    } else {
        (raw_path, Vec::new())
    };

    // Parse Content-Length (case-insensitive)
    let mut content_length = 0usize;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(val) = lower.strip_prefix("content-length:") {
            content_length = val.trim().parse().unwrap_or(0);
        }
    }

    // Read body
    let body_start = header_end + 4;
    let body = if content_length > 0 {
        let mut body_buf = if body_start < buf.len() {
            buf[body_start..].to_vec()
        } else {
            Vec::new()
        };
        while body_buf.len() < content_length {
            match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut tmp)).await {
                Ok(Ok(0)) | Err(_) => break,
                Ok(Ok(n)) => body_buf.extend_from_slice(&tmp[..n]),
                Ok(Err(_)) => break,
            }
        }
        body_buf.truncate(content_length);
        body_buf
    } else {
        Vec::new()
    };

    Some(HttpRequest { method, path, query, body })
}

// ---------------------------------------------------------------------------
// Route dispatch
// ---------------------------------------------------------------------------

async fn handle_request(bridge: &WhatsAppBridge, req: &HttpRequest) -> Vec<u8> {
    match (req.method.as_str(), req.path.as_str()) {
        // Status & QR
        ("GET", "/api/status") | ("GET", "/health") | ("GET", "/") => handle_status(bridge).await,
        ("GET", "/api/qr") => handle_qr(bridge, req),

        // Groups
        ("GET", "/api/groups") => handle_groups(bridge).await,
        ("GET", "/api/group-info") => handle_group_info(bridge, req).await,

        // Messaging
        ("POST", "/api/send") => handle_send(bridge, &req.body).await,
        ("POST", "/api/reply") => handle_reply(bridge, &req.body).await,
        ("POST", "/api/edit") => handle_edit(bridge, &req.body).await,
        ("POST", "/api/react") => handle_react(bridge, &req.body).await,
        ("POST", "/api/image") => handle_image(bridge, &req.body).await,
        ("POST", "/api/video") => handle_video(bridge, &req.body).await,
        ("POST", "/api/audio") => handle_audio(bridge, &req.body).await,
        ("POST", "/api/doc") => handle_doc(bridge, &req.body).await,
        ("POST", "/api/sticker") => handle_sticker(bridge, &req.body).await,
        ("POST", "/api/location") => handle_location(bridge, &req.body).await,
        ("POST", "/api/contact") => handle_contact(bridge, &req.body).await,
        ("POST", "/api/forward") => handle_forward(bridge, &req.body).await,
        ("POST", "/api/poll") => handle_poll(bridge, &req.body).await,

        _ => json_err(404, "not found"),
    }
}

// ---------------------------------------------------------------------------
// Endpoint handlers
// ---------------------------------------------------------------------------

async fn handle_status(bridge: &WhatsAppBridge) -> Vec<u8> {
    let state = bridge.state();
    let m = bridge.metrics();
    let queue = bridge.queue_depth().await;
    json_ok(json!({
        "state": format!("{:?}", state),
        "connected": state == BridgeState::Connected,
        "queue_depth": queue,
        "uptime_secs": m.started_at.elapsed().as_secs(),
        "messages_sent": m.messages_sent.load(Ordering::Relaxed),
        "messages_received": m.messages_received.load(Ordering::Relaxed),
        "reconnect_count": m.reconnect_count.load(Ordering::Relaxed),
        "last_connect_epoch": m.last_connect_epoch.load(Ordering::Relaxed),
        "last_disconnect_epoch": m.last_disconnect_epoch.load(Ordering::Relaxed),
        "last_inbound_epoch": m.last_inbound_epoch.load(Ordering::Relaxed),
        "last_outbound_epoch": m.last_outbound_epoch.load(Ordering::Relaxed),
    }))
}

fn handle_qr(bridge: &WhatsAppBridge, req: &HttpRequest) -> Vec<u8> {
    let qr_data = bridge.current_qr();
    match qr_data {
        Some(data) => {
            let format = req.query_get("format").unwrap_or("json");
            let qr = match QrRender::new(&data) {
                Some(q) => q,
                None => return json_err(500, "failed to render QR code"),
            };
            match format {
                "png" => http_response(200, "image/png", &qr.png(8)),
                "svg" => http_response(200, "image/svg+xml", qr.svg().as_bytes()),
                "terminal" => http_response(200, "text/plain", qr.terminal().as_bytes()),
                "html" => http_response(200, "text/html", qr.html().as_bytes()),
                _ => json_ok(json!({
                    "qr_data": data,
                    "terminal": qr.terminal(),
                })),
            }
        }
        None => json_err(404, "no QR code available (already paired or not yet generated)"),
    }
}

async fn handle_groups(bridge: &WhatsAppBridge) -> Vec<u8> {
    match bridge.get_joined_groups().await {
        Ok(groups) => {
            let list: Vec<serde_json::Value> = groups
                .iter()
                .map(|g| json!({
                    "jid": g.jid,
                    "subject": g.subject,
                    "participant_count": g.participants.len(),
                }))
                .collect();
            json_ok(json!({ "groups": list }))
        }
        Err(e) => json_err(500, &e.to_string()),
    }
}

async fn handle_group_info(bridge: &WhatsAppBridge, req: &HttpRequest) -> Vec<u8> {
    let jid = match req.query_get("jid") {
        Some(j) => j,
        None => return json_err(400, "missing ?jid= parameter"),
    };
    match bridge.get_group_info(jid).await {
        Ok(info) => {
            let participants: Vec<serde_json::Value> = info.participants
                .iter()
                .map(|p| json!({
                    "jid": p.jid,
                    "phone": p.phone,
                    "is_admin": p.is_admin,
                }))
                .collect();
            json_ok(json!({
                "jid": info.jid,
                "subject": info.subject,
                "participants": participants,
            }))
        }
        Err(e) => json_err(500, &e.to_string()),
    }
}

// --- Messaging ---

#[derive(Deserialize)]
struct SendReq {
    jid: String,
    text: String,
}

async fn handle_send(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: SendReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    match bridge.send_message_with_id(&req.jid, &req.text).await {
        Ok(id) => json_ok_id(&id),
        Err(e) => json_err(500, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct ReplyReq {
    jid: String,
    id: String,
    sender: String,
    text: String,
}

async fn handle_reply(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: ReplyReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    match bridge.send_reply(&req.jid, &req.id, &req.sender, &req.text).await {
        Ok(id) => json_ok_id(&id),
        Err(e) => json_err(500, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct EditReq {
    jid: String,
    id: String,
    text: String,
}

async fn handle_edit(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: EditReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    match bridge.edit_message(&req.jid, &req.id, &req.text).await {
        Ok(()) => json_ok_simple(),
        Err(e) => json_err(500, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct ReactReq {
    jid: String,
    id: String,
    emoji: String,
    from_me: Option<bool>,
    sender_jid: Option<String>,
}

async fn handle_react(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: ReactReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    match bridge.send_reaction(
        &req.jid,
        &req.id,
        req.sender_jid.as_deref(),
        &req.emoji,
        req.from_me.unwrap_or(true),
    ).await {
        Ok(()) => json_ok_simple(),
        Err(e) => json_err(500, &e.to_string()),
    }
}

// --- Media ---

#[derive(Deserialize)]
struct MediaReq {
    jid: String,
    path: String,
    caption: Option<String>,
}

async fn read_file_for_media(path: &str) -> Result<Vec<u8>, Vec<u8>> {
    tokio::fs::read(path).await
        .map_err(|e| json_err(400, &format!("cannot read file {path}: {e}")))
}

fn mime_for_image(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/jpeg",
    }
}

fn mime_for_video(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        Some("3gp") => "video/3gpp",
        _ => "video/mp4",
    }
}

fn mime_for_audio(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("mp3") => "audio/mpeg",
        Some("m4a") | Some("aac") => "audio/mp4",
        _ => "audio/ogg; codecs=opus",
    }
}

fn mime_for_doc(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("txt") => "text/plain",
        _ => "application/octet-stream",
    }
}

async fn handle_image(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: MediaReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    let data = match read_file_for_media(&req.path).await { Ok(d) => d, Err(e) => return e };
    let path = std::path::Path::new(&req.path);
    match bridge.send_image(&req.jid, data, mime_for_image(path), req.caption.as_deref()).await {
        Ok(()) => json_ok_simple(),
        Err(e) => json_err(500, &e.to_string()),
    }
}

async fn handle_video(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: MediaReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    let data = match read_file_for_media(&req.path).await { Ok(d) => d, Err(e) => return e };
    let path = std::path::Path::new(&req.path);
    match bridge.send_video(&req.jid, data, mime_for_video(path), req.caption.as_deref()).await {
        Ok(()) => json_ok_simple(),
        Err(e) => json_err(500, &e.to_string()),
    }
}

async fn handle_audio(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: MediaReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    let data = match read_file_for_media(&req.path).await { Ok(d) => d, Err(e) => return e };
    let path = std::path::Path::new(&req.path);
    match bridge.send_audio(&req.jid, data, mime_for_audio(path), None).await {
        Ok(()) => json_ok_simple(),
        Err(e) => json_err(500, &e.to_string()),
    }
}

async fn handle_doc(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: MediaReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    let data = match read_file_for_media(&req.path).await { Ok(d) => d, Err(e) => return e };
    let path = std::path::Path::new(&req.path);
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    match bridge.send_document(&req.jid, data, mime_for_doc(path), filename).await {
        Ok(()) => json_ok_simple(),
        Err(e) => json_err(500, &e.to_string()),
    }
}

async fn handle_sticker(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: MediaReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    let data = match read_file_for_media(&req.path).await { Ok(d) => d, Err(e) => return e };
    match bridge.send_sticker(&req.jid, data, "image/webp", false).await {
        Ok(()) => json_ok_simple(),
        Err(e) => json_err(500, &e.to_string()),
    }
}

// --- Location / Contact / Forward / Poll ---

#[derive(Deserialize)]
struct LocationReq {
    jid: String,
    lat: f64,
    lon: f64,
}

async fn handle_location(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: LocationReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    match bridge.send_location(&req.jid, req.lat, req.lon, None, None).await {
        Ok(()) => json_ok_simple(),
        Err(e) => json_err(500, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct ContactReq {
    jid: String,
    name: String,
    phone: String,
}

async fn handle_contact(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: ContactReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    let vcard = format!(
        "BEGIN:VCARD\nVERSION:3.0\nFN:{}\nTEL;type=CELL:+{}\nEND:VCARD",
        req.name, req.phone
    );
    match bridge.send_contact(&req.jid, &req.name, &vcard).await {
        Ok(()) => json_ok_simple(),
        Err(e) => json_err(500, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct ForwardReq {
    jid: String,
    msg_id: String,
}

async fn handle_forward(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: ForwardReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    match bridge.forward_message(&req.jid, &req.msg_id).await {
        Ok(id) => json_ok_id(&id),
        Err(e) => json_err(500, &e.to_string()),
    }
}

#[derive(Deserialize)]
struct PollReq {
    jid: String,
    question: String,
    options: Vec<String>,
    selectable_count: u32,
}

async fn handle_poll(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: PollReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    match bridge.send_poll(&req.jid, &req.question, &req.options, req.selectable_count).await {
        Ok(id) => json_ok_id(&id),
        Err(e) => json_err(500, &e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Start the API server. Blocks until cancelled.
pub async fn serve(bridge: Arc<WhatsAppBridge>, port: u16, cancel: CancellationToken) {
    let bind = std::env::var("WHATSRUST_BIND").unwrap_or_else(|_| "127.0.0.1".to_string());
    let listener = match TcpListener::bind((&*bind, port)).await {
        Ok(l) => {
            info!(bind = %bind, port = port, "API server listening");
            l
        }
        Err(e) => {
            error!(error = %e, bind = %bind, port = port, "failed to bind API server");
            return;
        }
    };

    loop {
        tokio::select! {
            result = listener.accept() => {
                let Ok((mut stream, _)) = result else { continue };
                let bridge = bridge.clone();
                tokio::spawn(async move {
                    let req = match read_request(&mut stream).await {
                        Some(r) => r,
                        None => {
                            let _ = stream.write_all(&json_err(400, "bad request")).await;
                            return;
                        }
                    };
                    let response = handle_request(&bridge, &req).await;
                    let _ = stream.write_all(&response).await;
                });
            }
            _ = cancel.cancelled() => break,
        }
    }
}

// ---------------------------------------------------------------------------
// CLI HTTP client
// ---------------------------------------------------------------------------

/// Send a GET request to the running daemon and return (status, body_bytes).
pub async fn cli_get(port: u16, path: &str) -> anyhow::Result<(u16, Vec<u8>)> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await
        .map_err(|e| anyhow::anyhow!("cannot connect to whatsrust daemon on port {port}: {e}\nIs the daemon running? Start it with: WHATSRUST_PORT={port} whatsrust"))?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    stream.shutdown().await?;

    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("timeout reading response from daemon"))??;
    parse_cli_response(&buf)
}

/// Send a POST request with JSON body to the running daemon.
pub async fn cli_post(port: u16, path: &str, body: &str) -> anyhow::Result<(u16, Vec<u8>)> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await
        .map_err(|e| anyhow::anyhow!("cannot connect to whatsrust daemon on port {port}: {e}\nIs the daemon running? Start it with: WHATSRUST_PORT={port} whatsrust"))?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await?;
    stream.shutdown().await?;

    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("timeout reading response from daemon"))??;
    parse_cli_response(&buf)
}

fn parse_cli_response(raw: &[u8]) -> anyhow::Result<(u16, Vec<u8>)> {
    let header_end = raw.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("invalid HTTP response"))?;
    let header_str = String::from_utf8_lossy(&raw[..header_end]);
    let status: u16 = header_str
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = raw[header_end + 4..].to_vec();
    Ok((status, body))
}
