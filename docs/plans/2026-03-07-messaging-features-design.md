# Messaging Features Design: Forward, Poll, View-Once, Product

Date: 2026-03-07
Status: Approved (Codex + Gemini reviewed)

## Scope

Add 4 missing message types to close the gap with Baileys on core messaging.
Skip deprecated features (buttons, lists, templates).

## Architecture Decisions

### 1. No new abstraction layer

Keep `bridge.rs` as orchestration. Only split out `polls.rs` for poll crypto.
Forward, view-once, and product are thin enough (~20-40 LOC each) to live in bridge.rs.

### 2. Flags, not wrapper enums

Add `MessageFlags` to `WhatsAppInbound` for transport modifiers (forwarded, view-once).
Do NOT create nested types like `Forwarded<ViewOnce<Image>>`. Keep `MessageContent` flat.

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageFlags {
    pub is_forwarded: bool,
    pub forwarding_score: u32,
    pub is_view_once: bool,
}
```

Extend `WhatsAppInbound` with `pub flags: MessageFlags`.

### 3. Forward = standalone method

NOT a flag on every send method. Forwarding is "resend existing message with metadata".

### 4. View-once = separate send methods, shared inbound

Outbound: `send_view_once_image()`, `send_view_once_video()` (clear intent).
Inbound: reuse existing `Image`/`Video` variants + `flags.is_view_once = true`.

### 5. Poll crypto in polls.rs

Store poll secrets in SQLite. Decrypt votes using HKDF-SHA256 + AES-256-GCM.
Emit immutable `PollVote` events — no mutable poll state accumulation.

### 6. Product uses a spec struct

Avoid 10+ positional args. One `ProductSpec` struct.

## API Surface

### Outbound Methods

```rust
// Forward
pub async fn forward_message(&self, jid: &str, source_chat_jid: &str, source_message_id: &str) -> Result<String>;

// Poll
pub async fn send_poll(&self, jid: &str, question: &str, options: &[&str], selectable_count: u32) -> Result<String>;

// View-Once
pub async fn send_view_once_image(&self, jid: &str, data: Vec<u8>, mime: &str, caption: Option<&str>) -> Result<String>;
pub async fn send_view_once_video(&self, jid: &str, data: Vec<u8>, mime: &str, caption: Option<&str>) -> Result<String>;

// Product
pub struct ProductSpec {
    pub business_owner_jid: String,
    pub product_id: String,
    pub title: String,
    pub description: Option<String>,
    pub currency_code: Option<String>,
    pub price_amount_1000: Option<i64>,
    pub body: Option<String>,
}
pub async fn send_product(&self, jid: &str, product: ProductSpec) -> Result<String>;
```

### Inbound Variants (new additions to MessageContent)

```rust
PollCreated { question: String, options: Vec<String>, selectable_count: u32 },
PollVote { poll_id: String, selected_options: Vec<String> },
Product { owner_jid: String, product_id: String, title: String, description: Option<String> },
```

Existing variants get `flags.is_forwarded` / `flags.is_view_once` via `WhatsAppInbound.flags`.

### REPL Commands

- `forward <dst_jid> <src_chat_jid> <msg_id>`
- `poll <jid> <count> <question> | <opt1> | <opt2> ...`
- `vo-image <jid> <path> [caption]`
- `vo-video <jid> <path> [caption]`

## Implementation Order

1. **Forward** (~40 LOC) — highest ROI, lowest complexity
2. **View-once** (~35 LOC) — trivial wrapper, high user demand
3. **Poll** (~200 LOC) — needs crypto, new file polls.rs, SQLite schema
4. **Product** (~90 LOC) — defer if business-account-only

Total: ~365-475 LOC. Keeps codebase under 6100 lines.

## Poll Crypto Detail

WhatsApp encrypts poll votes:
- On poll creation: generate random 32-byte `enc_key` (the poll secret)
- Store: `(chat_jid, poll_id, enc_key, option_names, option_sha256_hashes)`
- On vote send: SHA256 each selected option name, encode as PollVoteMessage, encrypt with:
  - Key: HKDF-SHA256(enc_key, poll_id + voter_jid, "Poll Vote")
  - Cipher: AES-256-GCM, random 12-byte IV
  - AAD: `"{poll_id}\0{voter_jid}"`
- On vote receive: reverse the process, map hashes back to option names

New table in storage.rs:
```sql
CREATE TABLE IF NOT EXISTS poll_keys (
    chat_jid TEXT NOT NULL,
    poll_id TEXT NOT NULL,
    enc_key BLOB NOT NULL,
    options_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (chat_jid, poll_id)
);
```

## Anti-Patterns Avoided (from Baileys)

- No `AnyMessageContent` mega-union or generic `send(content, opts)`
- No mutable poll state store — emit immutable vote events
- No `forwarded: bool` parameter on every send method
- No unbounded raw-message cache (cap at 256 entries if needed)
- No product API with positional args — use struct

## GitHub Issues

- #2: Forward message
- #3: Poll messages
- #4: View-once messages
- #5: Product/catalog messages
- #6: Group settings (separate workstream)
- #7: Presence subscription (separate workstream)
- #8: Status/stories (separate workstream)
- #9: Newsletter/channels (separate workstream)
- #10: Profile & privacy (separate workstream)
- #11: Chat management (separate workstream)
- #12: History sync (separate workstream)
