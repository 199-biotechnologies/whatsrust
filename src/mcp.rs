//! MCP (Model Context Protocol) server — JSON-RPC over stdio.
//!
//! Proxies tool calls to the running whatsrust daemon via its HTTP API.
//! Start with `whatsrust mcp` — typically invoked by Claude Code or other
//! MCP-compatible AI tool harnesses.
//!
//! ## Channel support
//! When the daemon's SSE stream (`GET /api/events`) emits an `inbound` event,
//! this server pushes a `notifications/claude/channel` notification to Claude Code
//! so incoming WhatsApp messages arrive in the session automatically.

use std::io::{BufRead, Write};
use std::sync::mpsc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Run the MCP server on stdin/stdout. Blocks until EOF.
pub fn run_mcp_server(port: u16) {
    let stdin = std::io::stdin();

    // All stdout writes go through this channel so both the stdin handler and
    // the SSE forwarder can write without needing stdout to be Send.
    let (write_tx, write_rx) = mpsc::channel::<String>();

    // Writer thread: owns stdout and flushes every line immediately.
    {
        let write_tx_sse = write_tx.clone();
        // SSE forwarder — sends notifications directly to the writer.
        std::thread::spawn(move || {
            sse_channel_forwarder(port, write_tx_sse);
        });
    }
    std::thread::spawn(move || {
        let mut stdout = std::io::stdout();
        for line in write_rx {
            let _ = writeln!(stdout, "{line}");
            let _ = stdout.flush();
        }
    });

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let err = json_rpc_error(Value::Null, -32700, &format!("parse error: {e}"));
                let _ = write_tx.send(serde_json::to_string(&err).unwrap());
                continue;
            }
        };

        // Notifications have null id — don't send a response
        let is_notification = req.id.is_null() || req.method.starts_with("notifications/");
        let response = handle_rpc(&req, port);
        if !is_notification {
            let _ = write_tx.send(serde_json::to_string(&response).unwrap());
        }
    }
}

/// Connects to the daemon's SSE stream and forwards every `inbound` event to
/// Claude Code as a `notifications/claude/channel` notification.
/// Reconnects with backoff on disconnect (daemon restart, network hiccup, etc.).
fn sse_channel_forwarder(port: u16, tx: mpsc::Sender<String>) {
    use std::io::BufRead;
    use std::net::TcpStream;
    use std::time::Duration;

    let mut backoff = Duration::from_secs(1);

    loop {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
            // Send HTTP GET /api/events request
            {
                use std::io::Write as _;
                let mut s = stream.try_clone().unwrap();
                let req = format!(
                    "GET /api/events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n\r\n"
                );
                if s.write_all(req.as_bytes()).is_err() {
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
            }

            backoff = Duration::from_secs(1); // reset on successful connect
            let reader = std::io::BufReader::new(stream);
            let mut past_headers = false;
            let mut event_type = String::new();
            let mut data_buf = String::new();

            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };

                // Skip HTTP response headers
                if !past_headers {
                    if line.is_empty() { past_headers = true; }
                    continue;
                }

                // SSE chunked transfer may prefix data length lines — skip hex lines
                if line.chars().all(|c| c.is_ascii_hexdigit()) && !line.is_empty() {
                    continue;
                }

                if line.starts_with("event:") {
                    event_type = line[6..].trim().to_string();
                } else if line.starts_with("data:") {
                    data_buf = line[5..].trim().to_string();
                } else if line.is_empty() {
                    if event_type == "inbound" && !data_buf.is_empty() {
                        if let Ok(msg) = serde_json::from_str::<Value>(&data_buf) {
                            let notification = build_channel_notification(&msg);
                            if let Ok(serialized) = serde_json::to_string(&notification) {
                                if tx.send(serialized).is_err() {
                                    return; // main thread gone, exit
                                }
                            }
                        }
                    }
                    event_type.clear();
                    data_buf.clear();
                }
            }
        }

        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Build the `notifications/claude/channel` JSON-RPC notification for an inbound message.
fn build_channel_notification(msg: &Value) -> Value {
    let chat_jid = msg.get("jid").and_then(|v| v.as_str()).unwrap_or("");
    let sender = msg.get("sender").and_then(|v| v.as_str()).unwrap_or("");
    let message_id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let timestamp = msg.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
    let is_from_me = msg.get("is_from_me").and_then(|v| v.as_bool()).unwrap_or(false);
    let is_group = msg.get("is_group").and_then(|v| v.as_bool()).unwrap_or(false);
    let chat_type = if is_group { "group" } else { "individual" };

    // Extract display text from content
    let content = extract_display_text(msg);

    json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": content,
            "meta": {
                "chat_jid": chat_jid,
                "sender": sender,
                "message_id": message_id,
                "timestamp": timestamp.to_string(),
                "is_from_me": is_from_me.to_string(),
                "chat_type": chat_type
            }
        }
    })
}

/// Extract a human-readable string from the inbound message's content field.
fn extract_display_text(msg: &Value) -> String {
    if let Some(content) = msg.get("content") {
        // Text message
        if let Some(text) = content.get("Text").and_then(|v| v.as_str()) {
            return text.to_string();
        }
        // Image/video/audio with caption
        for kind in &["Image", "Video", "Audio", "Document", "Sticker"] {
            if let Some(media) = content.get(kind) {
                let caption = media.get("caption").and_then(|v| v.as_str()).unwrap_or("");
                return if caption.is_empty() {
                    format!("[{}]", kind.to_lowercase())
                } else {
                    format!("[{}] {}", kind.to_lowercase(), caption)
                };
            }
        }
        // Reaction
        if let Some(reaction) = content.get("Reaction") {
            let emoji = reaction.get("emoji").and_then(|v| v.as_str()).unwrap_or("?");
            return format!("[reaction: {}]", emoji);
        }
        // Poll
        if let Some(poll) = content.get("PollCreated") {
            let question = poll.get("question").and_then(|v| v.as_str()).unwrap_or("poll");
            return format!("[poll: {}]", question);
        }
        // Location
        if content.get("Location").is_some() {
            return "[location]".to_string();
        }
        // Contact
        if let Some(contact) = content.get("Contact") {
            let name = contact.get("display_name").and_then(|v| v.as_str()).unwrap_or("contact");
            return format!("[contact: {}]", name);
        }
        // Fallback: serialize the whole content
        return serde_json::to_string(content).unwrap_or_default();
    }
    String::new()
}

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    /// Null for notifications (no response expected).
    #[serde(default)]
    id: Value,
    method: String,
    params: Option<Value>,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

fn json_rpc_ok(id: Value, result: Value) -> JsonRpcResponse {
    JsonRpcResponse { jsonrpc: "2.0".to_string(), id, result: Some(result), error: None }
}

fn json_rpc_error(id: Value, code: i32, msg: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: None,
        error: Some(json!({"code": code, "message": msg})),
    }
}

fn handle_rpc(req: &JsonRpcRequest, port: u16) -> JsonRpcResponse {
    match req.method.as_str() {
        "initialize" => json_rpc_ok(req.id.clone(), json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {},
                "experimental": { "claude/channel": {} }
            },
            "serverInfo": {
                "name": "whatsrust",
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": "You are a WhatsApp bridge assistant.\n\
                \n\
                ## Incoming WhatsApp messages\n\
                Messages arrive as <channel source=\"whatsrust\" chat_jid=\"...\" sender=\"...\" is_from_me=\"...\" chat_type=\"...\">.\n\
                - If is_from_me=\"true\" — these are messages the owner sent themselves; ignore unless they are commands.\n\
                - Otherwise: a new WhatsApp message has arrived. Read the chat_jid, sender, and content.\n\
                \n\
                ## Sending messages\n\
                Use whatsrust_send to send a message. Always confirm with the owner before sending unless explicitly told to act autonomously.\n\
                \n\
                ## Getting context\n\
                Call whatsrust_history with the chat_jid to read recent conversation before replying."
        })),
        "notifications/initialized" => json_rpc_ok(req.id.clone(), json!({})),
        "tools/list" => json_rpc_ok(req.id.clone(), json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let params = req.params.as_ref().unwrap_or(&Value::Null);
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let result = call_tool(name, &args, port);
            json_rpc_ok(req.id.clone(), result)
        }
        _ => json_rpc_error(req.id.clone(), -32601, &format!("method not found: {}", req.method)),
    }
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool_def("whatsrust_status", "Get bridge status, metrics, and connection state", json!({"type":"object","properties":{}})),
        tool_def("whatsrust_send", "Send a text message (optionally with @mentions and/or link preview)",
            json!({"type":"object","properties":{
                "jid":{"type":"string","description":"Recipient JID (phone@s.whatsapp.net or group@g.us)"},
                "text":{"type":"string","description":"Message text"},
                "mentions":{"type":"array","items":{"type":"string"},"description":"JIDs to @mention (optional)"},
                "schedule_at":{"type":"integer","description":"Unix epoch to defer send (optional)"},
                "link_preview":{"type":"object","description":"Link preview card (optional)","properties":{
                    "url":{"type":"string","description":"URL to preview (must appear in text)"},
                    "title":{"type":"string","description":"Preview title (og:title)"},
                    "description":{"type":"string","description":"Preview description (og:description)"},
                    "thumbnail_b64":{"type":"string","description":"Base64-encoded JPEG thumbnail"}
                },"required":["url"]}
            },"required":["jid","text"]})),
        tool_def("whatsrust_reply", "Reply to a message (quote)",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},
                "id":{"type":"string","description":"Message ID to reply to"},
                "sender":{"type":"string","description":"Sender JID of the quoted message"},
                "text":{"type":"string"},
                "mentions":{"type":"array","items":{"type":"string"},"description":"JIDs to @mention"}
            },"required":["jid","id","sender","text"]})),
        tool_def("whatsrust_react", "React to a message with an emoji",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},
                "id":{"type":"string","description":"Target message ID"},
                "emoji":{"type":"string"},
                "from_me":{"type":"boolean","description":"Whether target is from us (default true)"},
                "sender_jid":{"type":"string","description":"Target sender JID (required for group reactions when from_me=false)"}
            },"required":["jid","id","emoji"]})),
        tool_def("whatsrust_edit", "Edit a previously sent message",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"id":{"type":"string"},"text":{"type":"string"}
            },"required":["jid","id","text"]})),
        tool_def("whatsrust_revoke", "Delete a message for everyone",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"id":{"type":"string"}
            },"required":["jid","id"]})),
        tool_def("whatsrust_image", "Send an image (base64)",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"data":{"type":"string","description":"Base64-encoded image"},
                "mime":{"type":"string","description":"MIME type (default image/jpeg)"},
                "caption":{"type":"string"}
            },"required":["jid","data"]})),
        tool_def("whatsrust_groups", "List all joined groups", json!({"type":"object","properties":{}})),
        tool_def("whatsrust_group_info", "Get group details and members",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_history", "Get recent messages for a chat",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"limit":{"type":"integer","description":"Max messages (default 20)"},
                "before":{"type":"integer","description":"Unix timestamp for pagination"}
            },"required":["jid"]})),
        tool_def("whatsrust_search", "Search message history",
            json!({"type":"object","properties":{
                "q":{"type":"string","description":"Search query"},
                "jid":{"type":"string","description":"Limit to specific chat (optional)"},
                "limit":{"type":"integer"}
            },"required":["q"]})),
        tool_def("whatsrust_typing", "Send typing indicator",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_location", "Send a location pin",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"lat":{"type":"number"},"lon":{"type":"number"},
                "name":{"type":"string"},"address":{"type":"string"}
            },"required":["jid","lat","lon"]})),
        tool_def("whatsrust_contact", "Send a contact card",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"name":{"type":"string"},"vcard":{"type":"string"}
            },"required":["jid","name","vcard"]})),
        tool_def("whatsrust_poll", "Create a poll",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"question":{"type":"string"},
                "options":{"type":"array","items":{"type":"string"}},
                "selectable_count":{"type":"integer","description":"Max selectable options"}
            },"required":["jid","question","options","selectable_count"]})),
        // Chat management
        tool_def("whatsrust_pin_chat", "Pin a chat to the top",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_unpin_chat", "Unpin a chat",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_mute_chat", "Mute a chat indefinitely",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_unmute_chat", "Unmute a chat",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_archive_chat", "Archive a chat",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_unarchive_chat", "Unarchive a chat",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_mark_read", "Mark a chat as read",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_delete_chat", "Delete a chat and its media",
            json!({"type":"object","properties":{"jid":{"type":"string"}},"required":["jid"]})),
        tool_def("whatsrust_delete_for_me", "Delete a message for me only",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"id":{"type":"string","description":"Message ID"},
                "sender":{"type":"string","description":"Sender JID (for group messages from others)"},
                "from_me":{"type":"boolean","description":"Whether message is from us (default true)"}
            },"required":["jid","id"]})),
        tool_def("whatsrust_star", "Star a message",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"id":{"type":"string","description":"Message ID"},
                "sender":{"type":"string","description":"Sender JID (for group messages from others)"},
                "from_me":{"type":"boolean","description":"Whether message is from us (default true)"}
            },"required":["jid","id"]})),
        tool_def("whatsrust_unstar", "Unstar a message",
            json!({"type":"object","properties":{
                "jid":{"type":"string"},"id":{"type":"string","description":"Message ID"},
                "sender":{"type":"string","description":"Sender JID (for group messages from others)"},
                "from_me":{"type":"boolean","description":"Whether message is from us (default true)"}
            },"required":["jid","id"]})),
        tool_def("whatsrust_status_text", "Post a text status/story to specified recipients",
            json!({"type":"object","properties":{
                "recipients":{"type":"array","items":{"type":"string"},"description":"Phone numbers of recipients"},
                "text":{"type":"string","description":"Status text"},
                "background_argb":{"type":"integer","description":"Background color as 0xAARRGGBB (default 0xFF1E6E4F)"},
                "font":{"type":"integer","description":"Font style 0-4 (default 0)"},
                "privacy":{"type":"string","description":"contacts|allowlist|denylist (default contacts)"}
            },"required":["recipients","text"]})),
        tool_def("whatsrust_status_image", "Post an image status/story",
            json!({"type":"object","properties":{
                "recipients":{"type":"array","items":{"type":"string"}},
                "data":{"type":"string","description":"Base64-encoded image"},
                "mime":{"type":"string","description":"MIME type (default image/jpeg)"},
                "caption":{"type":"string"},
                "privacy":{"type":"string"}
            },"required":["recipients","data"]})),
        tool_def("whatsrust_status_video", "Post a video status/story",
            json!({"type":"object","properties":{
                "recipients":{"type":"array","items":{"type":"string"}},
                "data":{"type":"string","description":"Base64-encoded video"},
                "mime":{"type":"string","description":"MIME type (default video/mp4)"},
                "caption":{"type":"string"},
                "seconds":{"type":"integer","description":"Duration in seconds"},
                "privacy":{"type":"string"}
            },"required":["recipients","data"]})),
        tool_def("whatsrust_status_revoke", "Revoke a previously posted status/story",
            json!({"type":"object","properties":{
                "recipients":{"type":"array","items":{"type":"string"},"description":"Same recipients as original post"},
                "message_id":{"type":"string","description":"ID returned when status was posted"},
                "privacy":{"type":"string"}
            },"required":["recipients","message_id"]})),
    ]
}

fn tool_def(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

fn call_tool(name: &str, args: &Value, port: u16) -> Value {
    let result = match name {
        "whatsrust_status" => http_get(port, "/api/status"),
        "whatsrust_groups" => http_get(port, "/api/groups"),
        "whatsrust_group_info" => {
            let jid = args.get("jid").and_then(|v| v.as_str()).unwrap_or("");
            http_get(port, &format!("/api/group-info?jid={jid}"))
        }
        "whatsrust_history" => {
            let jid = args.get("jid").and_then(|v| v.as_str()).unwrap_or("");
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(20);
            let before = args.get("before").and_then(|v| v.as_i64());
            let mut url = format!("/api/history?jid={jid}&limit={limit}");
            if let Some(b) = before { url.push_str(&format!("&before={b}")); }
            http_get(port, &url)
        }
        "whatsrust_search" => {
            let q = args.get("q").and_then(|v| v.as_str()).unwrap_or("");
            let jid = args.get("jid").and_then(|v| v.as_str());
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(20);
            let mut url = format!("/api/search?q={q}&limit={limit}");
            if let Some(j) = jid { url.push_str(&format!("&jid={j}")); }
            http_get(port, &url)
        }
        "whatsrust_send" => http_post(port, "/api/send?sync=true", args),
        "whatsrust_reply" => http_post(port, "/api/reply", args),
        "whatsrust_react" => http_post(port, "/api/react", args),
        "whatsrust_edit" => http_post(port, "/api/edit", args),
        "whatsrust_revoke" => http_post(port, "/api/revoke", args),
        "whatsrust_image" => http_post(port, "/api/image", args),
        "whatsrust_typing" => http_post(port, "/api/typing", args),
        "whatsrust_location" => http_post(port, "/api/location", args),
        "whatsrust_contact" => http_post(port, "/api/contact", args),
        "whatsrust_poll" => http_post(port, "/api/poll", args),
        "whatsrust_pin_chat" => http_post(port, "/api/pin-chat", args),
        "whatsrust_unpin_chat" => http_post(port, "/api/unpin-chat", args),
        "whatsrust_mute_chat" => http_post(port, "/api/mute-chat", args),
        "whatsrust_unmute_chat" => http_post(port, "/api/unmute-chat", args),
        "whatsrust_archive_chat" => http_post(port, "/api/archive-chat", args),
        "whatsrust_unarchive_chat" => http_post(port, "/api/unarchive-chat", args),
        "whatsrust_mark_read" => http_post(port, "/api/mark-read", args),
        "whatsrust_delete_chat" => http_post(port, "/api/delete-chat", args),
        "whatsrust_delete_for_me" => http_post(port, "/api/delete-for-me", args),
        "whatsrust_star" => http_post(port, "/api/star", args),
        "whatsrust_unstar" => http_post(port, "/api/unstar", args),
        "whatsrust_status_text" => http_post(port, "/api/status-text", args),
        "whatsrust_status_image" => http_post(port, "/api/status-image", args),
        "whatsrust_status_video" => http_post(port, "/api/status-video", args),
        "whatsrust_status_revoke" => http_post(port, "/api/status-revoke", args),
        _ => Err(format!("unknown tool: {name}")),
    };

    match result {
        Ok(body) => json!({ "content": [{ "type": "text", "text": body }] }),
        Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
    }
}

/// Blocking HTTP GET to the daemon.
fn http_get(port: u16, path: &str) -> Result<String, String> {
    use std::io::Read;
    use std::net::TcpStream;

    let host = mcp_connect_host();
    let mut stream = TcpStream::connect((&*host, port))
        .map_err(|e| format!("cannot connect to daemon on {host}:{port}: {e}"))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok();

    let auth = std::env::var("WHATSRUST_API_TOKEN").ok()
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth}Connection: close\r\n\r\n");
    std::io::Write::write_all(&mut stream, req.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| format!("read failed: {e}"))?;
    extract_http_body(&buf)
}

/// Blocking HTTP POST to the daemon.
fn http_post(port: u16, path: &str, body: &Value) -> Result<String, String> {
    use std::io::Read;
    use std::net::TcpStream;

    let host = mcp_connect_host();
    let mut stream = TcpStream::connect((&*host, port))
        .map_err(|e| format!("cannot connect to daemon on {host}:{port}: {e}"))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok();

    let body_str = serde_json::to_string(body).unwrap();
    let auth = std::env::var("WHATSRUST_API_TOKEN").ok()
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{auth}Connection: close\r\n\r\n{body_str}",
        body_str.len()
    );
    std::io::Write::write_all(&mut stream, req.as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| format!("read failed: {e}"))?;
    extract_http_body(&buf)
}

fn extract_http_body(raw: &[u8]) -> Result<String, String> {
    let header_end = raw.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("invalid HTTP response")?;
    let header_str = String::from_utf8_lossy(&raw[..header_end]);
    // Parse HTTP status code from first line
    let status: u16 = header_str
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = String::from_utf8(raw[header_end + 4..].to_vec())
        .map_err(|e| format!("invalid UTF-8: {e}"))?;
    if status >= 400 {
        Err(body)
    } else {
        Ok(body)
    }
}

/// Normalize bind host — replace wildcard "0.0.0.0" with "127.0.0.1" for connections.
fn mcp_connect_host() -> String {
    let bind = std::env::var("WHATSRUST_BIND").unwrap_or_else(|_| "127.0.0.1".to_string());
    if bind == "0.0.0.0" || bind == "::" {
        "127.0.0.1".to_string()
    } else {
        bind
    }
}
