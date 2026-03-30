# Status/Story Sending Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add WhatsApp status/story posting (text, image, video) and revocation via the wa-rs `client.status()` API.

**Architecture:** Four new `OutboundOpKind` variants with dedicated payload structs flow through the existing SQLite-first durable queue. Status ops use `jid = "status@broadcast"` with recipients in `payload_json`. The `execute_job` worker dispatches to `client.status().send_*()` methods. API, CLI, and MCP layers proxy to the bridge.

**Tech Stack:** Rust, wa-rs (rev 6fa2f8a), SQLite (no migration needed), serde_json

**Spec:** `docs/superpowers/specs/2026-03-30-status-sending-design.md`

---

### Task 1: OutboundOpKind Variants + Payload Structs

**Files:**
- Modify: `src/outbound.rs:11-29` (enum), `src/outbound.rs:31-75` (as_str/from_str), `src/outbound.rs:140-159` (after existing payloads), `src/outbound.rs:500-547` (tests)

- [ ] **Step 1: Write failing tests for new OpKind roundtrip and payload serde**

Add to the `tests` module at the bottom of `src/outbound.rs`:

```rust
#[test]
fn test_status_op_kind_roundtrip() {
    for kind in [
        OutboundOpKind::StatusText,
        OutboundOpKind::StatusImage,
        OutboundOpKind::StatusVideo,
        OutboundOpKind::StatusRevoke,
    ] {
        assert_eq!(OutboundOpKind::from_str(kind.as_str()), Some(kind));
    }
}

#[test]
fn test_status_text_payload_serde() {
    let p = StatusTextPayload {
        recipients: vec!["15551234567".to_string()],
        text: "Hello from status".to_string(),
        background_argb: 0xFF1E6E4F,
        font: 2,
        privacy: None,
    };
    let json = serde_json::to_string(&p).unwrap();
    let p2: StatusTextPayload = serde_json::from_str(&json).unwrap();
    assert_eq!(p2.text, "Hello from status");
    assert_eq!(p2.recipients.len(), 1);
    assert_eq!(p2.background_argb, 0xFF1E6E4F);
    assert_eq!(p2.font, 2);
    assert!(p2.privacy.is_none());
}

#[test]
fn test_status_media_payload_serde() {
    let p = StatusMediaPayload {
        recipients: vec!["15551234567".to_string(), "15559876543".to_string()],
        mime: "image/jpeg".to_string(),
        caption: Some("My photo".to_string()),
        seconds: 0,
        privacy: Some("contacts".to_string()),
    };
    let json = serde_json::to_string(&p).unwrap();
    let p2: StatusMediaPayload = serde_json::from_str(&json).unwrap();
    assert_eq!(p2.recipients.len(), 2);
    assert_eq!(p2.caption, Some("My photo".to_string()));
    assert_eq!(p2.seconds, 0);
}

#[test]
fn test_status_revoke_payload_serde() {
    let p = StatusRevokePayload {
        recipients: vec!["15551234567".to_string()],
        message_id: "3EB06D00CAB92340790621".to_string(),
        privacy: None,
    };
    let json = serde_json::to_string(&p).unwrap();
    let p2: StatusRevokePayload = serde_json::from_str(&json).unwrap();
    assert_eq!(p2.message_id, "3EB06D00CAB92340790621");
    assert_eq!(p2.recipients.len(), 1);
}
```

- [ ] **Step 2: Run tests — verify they fail**

Run: `cargo test --lib test_status_op_kind 2>&1 | tail -5`
Expected: FAIL — `StatusText` variant does not exist

- [ ] **Step 3: Add enum variants**

In `src/outbound.rs`, add 4 variants after `ViewOnceVideo` (line 28):

```rust
pub enum OutboundOpKind {
    Text,
    Reply,
    Image,
    Video,
    Audio,
    Document,
    Sticker,
    Location,
    Contact,
    Reaction,
    Unreact,
    Edit,
    Revoke,
    Poll,
    Forward,
    ViewOnceImage,
    ViewOnceVideo,
    StatusText,
    StatusImage,
    StatusVideo,
    StatusRevoke,
}
```

- [ ] **Step 4: Add as_str arms**

In the `as_str()` match (after `ViewOnceVideo` arm):

```rust
Self::StatusText => "status_text",
Self::StatusImage => "status_image",
Self::StatusVideo => "status_video",
Self::StatusRevoke => "status_revoke",
```

- [ ] **Step 5: Add from_str arms**

In the `from_str()` match (after `"view_once_video"` arm):

```rust
"status_text" => Some(Self::StatusText),
"status_image" => Some(Self::StatusImage),
"status_video" => Some(Self::StatusVideo),
"status_revoke" => Some(Self::StatusRevoke),
```

- [ ] **Step 6: Add payload structs**

After `ForwardPayload` (line ~159), add:

```rust
/// Payload for status/story text posts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusTextPayload {
    pub recipients: Vec<String>,
    pub text: String,
    pub background_argb: u32,
    pub font: i32,
    pub privacy: Option<String>,
}

/// Payload for status/story media posts (image or video).
/// Actual bytes stored in `payload_blob` column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusMediaPayload {
    pub recipients: Vec<String>,
    pub mime: String,
    pub caption: Option<String>,
    /// Video duration in seconds. 0 for images.
    pub seconds: u32,
    pub privacy: Option<String>,
}

/// Payload for status/story revocation.
/// Recipients MUST match the original post — wa-rs encrypts revoke to same device set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusRevokePayload {
    pub recipients: Vec<String>,
    pub message_id: String,
    pub privacy: Option<String>,
}
```

- [ ] **Step 7: Run tests — verify they pass**

Run: `cargo test --lib test_status_ 2>&1 | tail -5`
Expected: 4 new tests PASS

- [ ] **Step 8: Commit**

```bash
git add src/outbound.rs
git commit -m "feat: add StatusText/Image/Video/Revoke OutboundOpKind variants + payloads"
```

---

### Task 2: Privacy Mapping Helper + execute_job Worker Branches

**Files:**
- Modify: `src/outbound.rs:191-497` (execute_job match arms)

- [ ] **Step 1: Write failing test for privacy mapping**

Add to `src/outbound.rs` tests:

```rust
#[test]
fn test_parse_status_privacy() {
    assert_eq!(parse_status_privacy(None).as_str(), "contacts");
    assert_eq!(parse_status_privacy(Some("contacts".to_string())).as_str(), "contacts");
    assert_eq!(parse_status_privacy(Some("allowlist".to_string())).as_str(), "allowlist");
    assert_eq!(parse_status_privacy(Some("denylist".to_string())).as_str(), "denylist");
    // Unknown falls back to default
    assert_eq!(parse_status_privacy(Some("bogus".to_string())).as_str(), "contacts");
}
```

- [ ] **Step 2: Run test — verify it fails**

Run: `cargo test --lib test_parse_status_privacy 2>&1 | tail -5`
Expected: FAIL — `parse_status_privacy` not found

- [ ] **Step 3: Add parse_status_privacy and parse_recipient_jids helpers**

Add above `execute_job` in `src/outbound.rs`:

```rust
/// Parse a privacy string into a StatusPrivacySetting.
pub fn parse_status_privacy(s: Option<String>) -> whatsapp_rust::StatusPrivacySetting {
    match s.as_deref() {
        Some("allowlist") => whatsapp_rust::StatusPrivacySetting::AllowList,
        Some("denylist") => whatsapp_rust::StatusPrivacySetting::DenyList,
        _ => whatsapp_rust::StatusPrivacySetting::Contacts,
    }
}

/// Parse a list of phone number strings into Vec<Jid>.
fn parse_recipient_jids(recipients: &[String]) -> anyhow::Result<Vec<wacore_binary::jid::Jid>> {
    recipients
        .iter()
        .map(|r| {
            let normalized = r.trim().replace('+', "").replace(' ', "").replace('-', "");
            wacore_binary::jid::Jid::from_str(&format!("{normalized}@s.whatsapp.net"))
                .map_err(|e| anyhow::anyhow!("bad recipient JID '{r}': {e}"))
        })
        .collect()
}
```

- [ ] **Step 4: Run test — verify it passes**

Run: `cargo test --lib test_parse_status_privacy 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 5: Add status worker branches to execute_job**

In the `match kind` inside `execute_job`, after the `OutboundOpKind::Forward` arm (before the closing `}`), add:

```rust
OutboundOpKind::StatusText => {
    let p: StatusTextPayload = serde_json::from_str(&row.payload_json)?;
    let recipients = parse_recipient_jids(&p.recipients)?;
    let opts = whatsapp_rust::StatusSendOptions {
        privacy: parse_status_privacy(p.privacy),
    };
    let id = client.status().send_text(&p.text, p.background_argb, p.font, recipients, opts).await
        .map_err(|e| anyhow::anyhow!("send status text: {e}"))?;
    Ok(ExecOutcome { wa_message_id: Some(id), poll_key: None })
}

OutboundOpKind::StatusImage => {
    let p: StatusMediaPayload = serde_json::from_str(&row.payload_json)?;
    let data = row.payload_blob.clone()
        .ok_or_else(|| anyhow::anyhow!("status image op missing payload_blob"))?;
    let recipients = parse_recipient_jids(&p.recipients)?;
    let opts = whatsapp_rust::StatusSendOptions {
        privacy: parse_status_privacy(p.privacy),
    };
    let upload = client.upload(data, MediaType::Image).await
        .map_err(|e| anyhow::anyhow!("status image upload: {e}"))?;
    let id = client.status().send_image(&upload, vec![], p.caption.as_deref(), recipients, opts).await
        .map_err(|e| anyhow::anyhow!("send status image: {e}"))?;
    Ok(ExecOutcome { wa_message_id: Some(id), poll_key: None })
}

OutboundOpKind::StatusVideo => {
    let p: StatusMediaPayload = serde_json::from_str(&row.payload_json)?;
    let data = row.payload_blob.clone()
        .ok_or_else(|| anyhow::anyhow!("status video op missing payload_blob"))?;
    let recipients = parse_recipient_jids(&p.recipients)?;
    let opts = whatsapp_rust::StatusSendOptions {
        privacy: parse_status_privacy(p.privacy),
    };
    let upload = client.upload(data, MediaType::Video).await
        .map_err(|e| anyhow::anyhow!("status video upload: {e}"))?;
    let id = client.status().send_video(&upload, vec![], p.seconds, p.caption.as_deref(), recipients, opts).await
        .map_err(|e| anyhow::anyhow!("send status video: {e}"))?;
    Ok(ExecOutcome { wa_message_id: Some(id), poll_key: None })
}

OutboundOpKind::StatusRevoke => {
    let p: StatusRevokePayload = serde_json::from_str(&row.payload_json)?;
    let recipients = parse_recipient_jids(&p.recipients)?;
    let opts = whatsapp_rust::StatusSendOptions {
        privacy: parse_status_privacy(p.privacy),
    };
    let id = client.status().revoke(p.message_id, recipients, opts).await
        .map_err(|e| anyhow::anyhow!("revoke status: {e}"))?;
    Ok(ExecOutcome { wa_message_id: Some(id), poll_key: None })
}
```

- [ ] **Step 6: Run cargo check**

Run: `cargo check 2>&1 | head -20`
Expected: clean compilation (zero warnings)

- [ ] **Step 7: Run full test suite**

Run: `cargo test --all-targets 2>&1 | tail -5`
Expected: all tests pass (including new ones from Task 1)

- [ ] **Step 8: Commit**

```bash
git add src/outbound.rs
git commit -m "feat: status sending execute_job worker branches + privacy helper"
```

---

### Task 3: Bridge Methods

**Files:**
- Modify: `src/bridge.rs` (after existing send methods, around line ~1450)

- [ ] **Step 1: Add send_status_text method**

Add after the last existing send method (e.g. after `forward_message`):

```rust
/// Post a text status/story. Returns the WA message ID (sync).
pub async fn send_status_text(
    &self,
    recipients: &[String],
    text: &str,
    background_argb: u32,
    font: i32,
    privacy: Option<String>,
) -> Result<String> {
    let payload = serde_json::to_string(&crate::outbound::StatusTextPayload {
        recipients: recipients.to_vec(),
        text: text.to_string(),
        background_argb,
        font,
        privacy,
    })?;
    self.enqueue_and_wait("status@broadcast", crate::outbound::OutboundOpKind::StatusText, &payload, None).await
}

/// Post an image status/story. Returns the WA message ID (sync).
pub async fn send_status_image(
    &self,
    recipients: &[String],
    data: Vec<u8>,
    mime: &str,
    caption: Option<&str>,
    privacy: Option<String>,
) -> Result<String> {
    let payload = serde_json::to_string(&crate::outbound::StatusMediaPayload {
        recipients: recipients.to_vec(),
        mime: mime.to_string(),
        caption: caption.map(|c| c.to_string()),
        seconds: 0,
        privacy,
    })?;
    self.enqueue_and_wait("status@broadcast", crate::outbound::OutboundOpKind::StatusImage, &payload, Some(data)).await
}

/// Post a video status/story. Returns the WA message ID (sync).
pub async fn send_status_video(
    &self,
    recipients: &[String],
    data: Vec<u8>,
    mime: &str,
    caption: Option<&str>,
    seconds: u32,
    privacy: Option<String>,
) -> Result<String> {
    let payload = serde_json::to_string(&crate::outbound::StatusMediaPayload {
        recipients: recipients.to_vec(),
        mime: mime.to_string(),
        caption: caption.map(|c| c.to_string()),
        seconds,
        privacy,
    })?;
    self.enqueue_and_wait("status@broadcast", crate::outbound::OutboundOpKind::StatusVideo, &payload, Some(data)).await
}

/// Revoke a previously posted status/story. Returns the WA message ID (sync).
pub async fn revoke_status(
    &self,
    recipients: &[String],
    message_id: &str,
    privacy: Option<String>,
) -> Result<String> {
    let payload = serde_json::to_string(&crate::outbound::StatusRevokePayload {
        recipients: recipients.to_vec(),
        message_id: message_id.to_string(),
        privacy,
    })?;
    self.enqueue_and_wait("status@broadcast", crate::outbound::OutboundOpKind::StatusRevoke, &payload, None).await
}
```

- [ ] **Step 2: Run cargo check**

Run: `cargo check 2>&1 | head -20`
Expected: clean

- [ ] **Step 3: Commit**

```bash
git add src/bridge.rs
git commit -m "feat: bridge methods for status text/image/video/revoke"
```

---

### Task 4: API Endpoints

**Files:**
- Modify: `src/api.rs` (route table at line ~311, new handler functions + request structs)

- [ ] **Step 1: Add request structs**

Add near the other `Deserialize` structs in `src/api.rs` (after the messaging section):

```rust
#[derive(Deserialize)]
struct StatusTextReq {
    recipients: Vec<String>,
    text: String,
    #[serde(default = "default_status_bg")]
    background_argb: u32,
    #[serde(default)]
    font: i32,
    privacy: Option<String>,
}

fn default_status_bg() -> u32 { 0xFF1E6E4F }

#[derive(Deserialize)]
struct StatusMediaReq {
    recipients: Vec<String>,
    data: String, // base64
    mime: Option<String>,
    caption: Option<String>,
    seconds: Option<u32>,
    privacy: Option<String>,
}

#[derive(Deserialize)]
struct StatusRevokeReq {
    recipients: Vec<String>,
    message_id: String,
    privacy: Option<String>,
}
```

- [ ] **Step 2: Add handler functions**

```rust
async fn handle_status_text(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: StatusTextReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    if req.recipients.is_empty() {
        return json_err(400, "recipients must not be empty");
    }
    match bridge.send_status_text(&req.recipients, &req.text, req.background_argb, req.font, req.privacy).await {
        Ok(id) => json_response(200, &serde_json::json!({"ok": true, "id": id}).to_string()),
        Err(e) => bridge_err(e),
    }
}

async fn handle_status_image(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: StatusMediaReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    if req.recipients.is_empty() {
        return json_err(400, "recipients must not be empty");
    }
    let data = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &req.data) {
        Ok(d) => d,
        Err(e) => return json_err(400, &format!("bad base64: {e}")),
    };
    let mime = req.mime.as_deref().unwrap_or("image/jpeg");
    match bridge.send_status_image(&req.recipients, data, mime, req.caption.as_deref(), req.privacy).await {
        Ok(id) => json_response(200, &serde_json::json!({"ok": true, "id": id}).to_string()),
        Err(e) => bridge_err(e),
    }
}

async fn handle_status_video(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: StatusMediaReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    if req.recipients.is_empty() {
        return json_err(400, "recipients must not be empty");
    }
    let data = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &req.data) {
        Ok(d) => d,
        Err(e) => return json_err(400, &format!("bad base64: {e}")),
    };
    let mime = req.mime.as_deref().unwrap_or("video/mp4");
    let seconds = req.seconds.unwrap_or(0);
    match bridge.send_status_video(&req.recipients, data, mime, req.caption.as_deref(), seconds, req.privacy).await {
        Ok(id) => json_response(200, &serde_json::json!({"ok": true, "id": id}).to_string()),
        Err(e) => bridge_err(e),
    }
}

async fn handle_status_revoke(bridge: &WhatsAppBridge, body: &[u8]) -> Vec<u8> {
    let req: StatusRevokeReq = match parse_body(body) { Ok(r) => r, Err(e) => return e };
    if req.recipients.is_empty() {
        return json_err(400, "recipients must not be empty");
    }
    match bridge.revoke_status(&req.recipients, &req.message_id, req.privacy).await {
        Ok(id) => json_response(200, &serde_json::json!({"ok": true, "id": id}).to_string()),
        Err(e) => bridge_err(e),
    }
}
```

- [ ] **Step 3: Add routes to handle_request**

In the route table in `handle_request` (after the group management block, before the `_ => json_err(404, ...)`):

```rust
// Status/story
("POST", "/api/status-text") => handle_status_text(bridge, &req.body).await,
("POST", "/api/status-image") => handle_status_image(bridge, &req.body).await,
("POST", "/api/status-video") => handle_status_video(bridge, &req.body).await,
("POST", "/api/status-revoke") => handle_status_revoke(bridge, &req.body).await,
```

- [ ] **Step 4: Run cargo check**

Run: `cargo check 2>&1 | head -20`
Expected: clean compilation

- [ ] **Step 5: Commit**

```bash
git add src/api.rs
git commit -m "feat: REST API endpoints for status text/image/video/revoke"
```

---

### Task 5: CLI Commands

**Files:**
- Modify: `src/main.rs` (REPL commands and CLI client commands)

- [ ] **Step 1: Add REPL commands**

In the REPL match block in `src/main.rs` (after the last existing command like `"group-leave"`), add:

```rust
"status-text" | "st" => {
    if parts.len() < 3 {
        println!("usage: status-text <recipients> <text>");
        continue;
    }
    let recipients: Vec<String> = parts[1].split(',').map(|s| s.trim().to_string()).collect();
    let text = parts[2..].join(" ");
    match bridge_for_repl
        .send_status_text(&recipients, &text, 0xFF1E6E4F, 0, None)
        .await
    {
        Ok(id) => println!(">> status posted (id: {id})"),
        Err(e) => println!("!! status-text failed: {e}"),
    }
}
"status-revoke" | "sr" => {
    if parts.len() < 3 {
        println!("usage: status-revoke <recipients> <message_id>");
        continue;
    }
    let recipients: Vec<String> = parts[1].split(',').map(|s| s.trim().to_string()).collect();
    match bridge_for_repl
        .revoke_status(&recipients, parts[2], None)
        .await
    {
        Ok(id) => println!(">> status revoked (id: {id})"),
        Err(e) => println!("!! status-revoke failed: {e}"),
    }
}
```

- [ ] **Step 2: Add CLI client commands**

In the CLI client match block (after the last command like `"poll"`), add:

```rust
"status-text" => {
    require_args(args, 3, "status-text <recipients> <text>")?;
    let body = json!({
        "recipients": args[1].split(',').collect::<Vec<&str>>(),
        "text": args[2..].join(" ")
    }).to_string();
    let (status, resp) = api::cli_post(port, "/api/status-text", &body).await?;
    print_json_result(status, &resp)?;
    Ok(())
}
"status-image" => {
    require_args(args, 3, "status-image <recipients> <path> [caption]")?;
    let data = std::fs::read(&args[2])?;
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
    let mime = if args[2].ends_with(".png") { "image/png" } else { "image/jpeg" };
    let caption = if args.len() > 3 { Some(args[3..].join(" ")) } else { None };
    let body = json!({
        "recipients": args[1].split(',').collect::<Vec<&str>>(),
        "data": b64,
        "mime": mime,
        "caption": caption
    }).to_string();
    let (status, resp) = api::cli_post(port, "/api/status-image", &body).await?;
    print_json_result(status, &resp)?;
    Ok(())
}
"status-video" => {
    require_args(args, 3, "status-video <recipients> <path> [caption]")?;
    let data = std::fs::read(&args[2])?;
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
    let caption = if args.len() > 3 { Some(args[3..].join(" ")) } else { None };
    let body = json!({
        "recipients": args[1].split(',').collect::<Vec<&str>>(),
        "data": b64,
        "mime": "video/mp4",
        "caption": caption
    }).to_string();
    let (status, resp) = api::cli_post(port, "/api/status-video", &body).await?;
    print_json_result(status, &resp)?;
    Ok(())
}
"status-revoke" => {
    require_args(args, 3, "status-revoke <recipients> <msg_id>")?;
    let body = json!({
        "recipients": args[1].split(',').collect::<Vec<&str>>(),
        "message_id": args[2]
    }).to_string();
    let (status, resp) = api::cli_post(port, "/api/status-revoke", &body).await?;
    print_json_result(status, &resp)?;
    Ok(())
}
```

- [ ] **Step 3: Run cargo check**

Run: `cargo check 2>&1 | head -20`
Expected: clean

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: CLI + REPL commands for status text/image/video/revoke"
```

---

### Task 6: MCP Tools

**Files:**
- Modify: `src/mcp.rs:105-177` (tool_definitions), `src/mcp.rs:183-217` (call_tool)

- [ ] **Step 1: Add tool definitions**

In `tool_definitions()`, add after the `whatsrust_poll` entry:

```rust
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
```

- [ ] **Step 2: Add call_tool dispatch**

In `call_tool()`, add after `"whatsrust_poll"`:

```rust
"whatsrust_status_text" => http_post(port, "/api/status-text", args),
"whatsrust_status_image" => http_post(port, "/api/status-image", args),
"whatsrust_status_video" => http_post(port, "/api/status-video", args),
"whatsrust_status_revoke" => http_post(port, "/api/status-revoke", args),
```

- [ ] **Step 3: Run cargo check**

Run: `cargo check 2>&1 | head -20`
Expected: clean

- [ ] **Step 4: Run full test suite**

Run: `cargo test --all-targets 2>&1 | tail -5`
Expected: all tests pass

- [ ] **Step 5: Commit**

```bash
git add src/mcp.rs
git commit -m "feat: MCP tools for status text/image/video/revoke"
```

---

### Task 7: Final Verification + Codex Review

**Files:** None modified — review only.

- [ ] **Step 1: Run cargo check for zero warnings**

Run: `cargo check 2>&1 | head -20`
Expected: `Finished` with zero warnings

- [ ] **Step 2: Run full test suite**

Run: `cargo test --all-targets 2>&1 | tail -5`
Expected: all tests pass, including the 4+ new status tests

- [ ] **Step 3: Verify endpoint count**

Grep route table: `grep -c '"/api/' src/api.rs`
Expected: 39 routes (35 existing + 4 new status routes)

- [ ] **Step 4: Run Codex review**

```bash
codex exec -m gpt-5.4 --skip-git-repo-check --full-auto -c model_reasoning_effort="xhigh" \
  "Review the status sending implementation in whatsrust. Read src/outbound.rs, src/bridge.rs, src/api.rs, src/mcp.rs, src/main.rs. Check for: (1) any breaking changes to existing ops, (2) payload struct correctness, (3) execute_job worker branch correctness against wa-rs status API, (4) jid=status@broadcast contract, (5) recipient parsing safety, (6) privacy mapping. Be concise."
```

- [ ] **Step 5: Push**

```bash
git push
```

- [ ] **Step 6: Update habb Cargo.lock**

```bash
cd ~/Projects/habb && cargo update -p whatsrust && cargo check
```
