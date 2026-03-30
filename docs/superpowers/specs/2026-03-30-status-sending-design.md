# Status/Story Sending

**Date:** 2026-03-30
**Status:** Approved (Codex-verified, zero breaking changes)

## Goal

Add WhatsApp status/story posting (text, image, video) and revocation, using the wa-rs `client.status()` API available at rev `6fa2f8a`.

## Architecture

### Queue Integration

Status ops use the existing SQLite-first durable queue. Four new `OutboundOpKind` variants:

- `StatusText` — text story with background color + font
- `StatusImage` — image story with caption
- `StatusVideo` — video story with caption + duration
- `StatusRevoke` — revoke a previously posted status

All status jobs store `jid = "status@broadcast"` in the queue. The actual recipient list lives in `payload_json`. This preserves the worker's single-JID parse contract at `bridge.rs:2936`.

### Payload Structs (src/outbound.rs)

```rust
#[derive(Serialize, Deserialize)]
pub struct StatusTextPayload {
    pub recipients: Vec<String>,    // phone numbers
    pub text: String,
    pub background_argb: u32,       // 0xAARRGGBB
    pub font: i32,                  // 0-4
    pub privacy: Option<String>,    // "contacts" | "allowlist" | "denylist"
}

#[derive(Serialize, Deserialize)]
pub struct StatusMediaPayload {
    pub recipients: Vec<String>,
    pub mime: String,
    pub caption: String,
    pub seconds: u32,               // video duration (0 for image)
    pub privacy: Option<String>,
}
// payload_blob carries the raw media bytes (same as Image/Video ops)

#[derive(Serialize, Deserialize)]
pub struct StatusRevokePayload {
    pub recipients: Vec<String>,    // MUST match original post recipients
    pub message_id: String,
    pub privacy: Option<String>,
}
```

### Worker Branches (execute_job)

- `StatusText`: parse `StatusTextPayload`, build `Vec<Jid>` from recipients, call `client.status().send_text(text, bg, font, recipients, opts)`
- `StatusImage`: parse `StatusMediaPayload`, upload blob via `client.upload(data, MediaType::Image)`, generate empty JPEG thumbnail (or skip), call `client.status().send_image(upload, thumbnail, caption, recipients, opts)`
- `StatusVideo`: same as image but `MediaType::Video`, pass `seconds` as duration, call `client.status().send_video(upload, thumbnail, duration, caption, recipients, opts)`
- `StatusRevoke`: parse `StatusRevokePayload`, build `Vec<Jid>`, call `client.status().revoke(message_id, recipients, opts)`

All return the WA message ID from wa-rs.

### Bridge Methods (src/bridge.rs)

```
send_status_text(recipients, text, bg, font, privacy) -> job_id
send_status_image(recipients, data, mime, caption, privacy) -> job_id
send_status_video(recipients, data, mime, caption, duration, privacy) -> job_id
revoke_status(recipients, message_id, privacy) -> job_id
```

Each calls `enqueue_job` with `jid = "status@broadcast"`. Sync variants (`send_status_text_with_id` etc.) use `enqueue_and_wait` like existing sync sends.

### Privacy Mapping

`privacy` string in payload maps to `StatusPrivacySetting`:
- `None` or `"contacts"` -> `StatusPrivacySetting::Contacts` (default)
- `"allowlist"` -> `StatusPrivacySetting::AllowList`
- `"denylist"` -> `StatusPrivacySetting::DenyList`

## API Endpoints (src/api.rs)

| Method | Path | Body | Response |
|--------|------|------|----------|
| POST | `/api/status-text` | `{recipients, text, background_argb?, font?, privacy?}` | `{ok, job_id}` or `{ok, id}` with `?sync=true` |
| POST | `/api/status-image` | `{recipients, data (base64), mime?, caption?, privacy?}` | same |
| POST | `/api/status-video` | `{recipients, data (base64), mime?, caption?, seconds?, privacy?}` | same |
| POST | `/api/status-revoke` | `{recipients, message_id, privacy?}` | same |

Body size limit: 50 MB for media endpoints (matches existing media limits).

## CLI Commands (src/main.rs)

REPL commands:
- `status-text <recipient1,recipient2,...> <text>` — posts text status
- `status-image <recipient1,...> <path>` — posts image status
- `status-video <recipient1,...> <path>` — posts video status
- `status-revoke <recipient1,...> <message_id>` — revokes status

CLI client mode (HTTP):
- `whatsrust status-text <recipients> <text> [--bg 0xFF1E6E4F] [--font 0] [--privacy contacts]`
- `whatsrust status-image <recipients> <path> [--caption text] [--privacy contacts]`
- `whatsrust status-video <recipients> <path> [--caption text] [--privacy contacts]`
- `whatsrust status-revoke <recipients> <message_id> [--privacy contacts]`

Recipients are comma-separated phone numbers (e.g., `15551234567,15559876543`).

## MCP Tools (src/mcp.rs)

Two new tools:
- `send_status` — `{type: "text"|"image"|"video", recipients, text?, data?, caption?, background_argb?, font?, privacy?}`
- `revoke_status` — `{recipients, message_id, privacy?}`

## Inbound Handling

No changes. `should_ignore_jid("status@broadcast")` continues to filter inbound status messages (other people's stories). Delivery receipts for our own status posts will flow through the existing `Event::Receipt` path unaffected.

## Schema

No migration needed. `op_kind` is free TEXT in `outbound_queue`. New string values (`status_text`, `status_image`, `status_video`, `status_revoke`) are additive.

## Testing

- Unit tests for new payload serialization/deserialization
- Unit tests for privacy string -> StatusPrivacySetting mapping
- Unit test for `from_str`/`as_str` roundtrip on new OpKind variants
- Integration: manual test posting a text status to a known recipient

## Non-Goals

- Viewing others' status updates (requires inbound status@broadcast handling — separate feature)
- Contact list discovery (recipients are always caller-provided)
- Status privacy list management (which contacts are in allow/deny lists — managed in WhatsApp app)

## Codex-Verified Constraints

1. New `OutboundOpKind` strings added to both `as_str()` and `from_str()` — no existing strings changed
2. `outbound_queue.jid` is always `"status@broadcast"` — valid JID, worker parse succeeds
3. Status-specific payload structs — no reuse of `MediaPayload` or `RevokePayload`
4. `StatusRevokePayload` carries recipients — matches wa-rs requirement at status.rs:156
5. All status ops go through connection-gated worker — no hot-loop risk
6. Zero regressions to existing 17 outbound op kinds
