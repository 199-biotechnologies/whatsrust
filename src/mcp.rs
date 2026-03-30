//! MCP (Model Context Protocol) server — JSON-RPC over stdio.
//!
//! Proxies tool calls to the running whatsrust daemon via its HTTP API.
//! Start with `whatsrust mcp` — typically invoked by Claude Code or other
//! MCP-compatible AI tool harnesses.

use std::io::{BufRead, Write};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Run the MCP server on stdin/stdout. Blocks until EOF.
pub fn run_mcp_server(port: u16) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    // Send server info on startup (initialize handshake)
    // MCP clients send initialize first, but we also need to be ready to handle it.

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
                let _ = writeln!(stdout, "{}", serde_json::to_string(&err).unwrap());
                continue;
            }
        };

        // Notifications have null id — don't send a response
        let is_notification = req.id.is_null() || req.method.starts_with("notifications/");
        let response = handle_rpc(&req, port);
        if !is_notification {
            let _ = writeln!(stdout, "{}", serde_json::to_string(&response).unwrap());
            let _ = stdout.flush();
        }
    }
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
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "whatsrust",
                "version": env!("CARGO_PKG_VERSION")
            }
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
        tool_def("whatsrust_send", "Send a text message (optionally with @mentions)",
            json!({"type":"object","properties":{
                "jid":{"type":"string","description":"Recipient JID (phone@s.whatsapp.net or group@g.us)"},
                "text":{"type":"string","description":"Message text"},
                "mentions":{"type":"array","items":{"type":"string"},"description":"JIDs to @mention (optional)"},
                "schedule_at":{"type":"integer","description":"Unix epoch to defer send (optional)"}
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
