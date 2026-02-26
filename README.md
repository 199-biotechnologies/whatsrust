# picoclaw-wa-rust

Pure Rust WhatsApp bridge — a lean, single-binary replacement for Baileys (Node.js) sidecars. Designed to be embedded as a library in agent software, dashboards, or any Rust application that needs WhatsApp messaging.

Uses [whatsapp-rust](https://github.com/jlucaso1/whatsapp-rust) for the WhatsApp Web protocol and a custom rusqlite backend for Signal Protocol storage.

## Features

**Messaging** — Send and receive all WhatsApp message types:
text, image, audio/voice, video, document, sticker, location, contact card, reaction, edit, revoke, reply/quote.

**Reliability** — Production-hardened:
- Persistent outbound queue (SQLite, crash-safe claim/send/mark lifecycle)
- Message dedup (4096-entry ring buffer prevents double-processing)
- Exponential backoff with jitter on reconnect
- Graceful shutdown with inflight drain (SIGINT + SIGTERM)
- Single-instance lock (flock-based, prevents session contention)
- SQLite backup on startup/shutdown + periodic hot backup
- Database pruning of old sent/failed messages

**Infrastructure** — Ready for deployment:
- QR + pair-code pairing with multi-format QR rendering (terminal, PNG, HTML, SVG)
- Health endpoint (TCP, JSON: state + queue depth)
- Sender allowlist
- Anti-ban pacing (configurable send intervals + read receipt delays)
- Auto read receipts with randomized timing
- LID-to-phone sender resolution
- 15+ WhatsApp event types handled

## Quick Start

```bash
# QR pairing (default)
cargo run

# Phone number pairing
WHATSAPP_PAIR_PHONE="+447438689825" cargo run

# With sender allowlist + health endpoint
WHATSAPP_ALLOWED="447957491755" HEALTH_PORT=8080 cargo run
```

The REPL supports all message types: `send`, `reply`, `edit`, `react`, `image`, `audio`, `video`, `doc`, `sticker`, `location`, `contact`, `typing`, `status`, `quit`.

## Integration

This is designed as a library. Add it to your Cargo.toml or import the modules directly.

### Basic usage

```rust
use picoclaw_wa_rust::bridge::{BridgeConfig, WhatsAppBridge, WhatsAppInbound};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

let (inbound_tx, mut inbound_rx) = mpsc::channel::<WhatsAppInbound>(256);
let cancel = CancellationToken::new();

let bridge = WhatsAppBridge::start(
    BridgeConfig {
        db_path: "whatsapp.db".into(),
        health_port: 8080,
        ..Default::default()
    },
    inbound_tx,
    cancel.clone(),
);

// Send a message
bridge.send_message_with_id("447957491755", "Hello from Rust").await?;

// Receive messages
while let Some(msg) = inbound_rx.recv().await {
    println!("{}: {}", msg.sender, msg.content.display_text());
}
```

### QR code for pairing UI

Subscribe to QR events and render in your preferred format:

```rust
use picoclaw_wa_rust::qr::QrRender;

let mut qr_rx = bridge.subscribe_qr();
while qr_rx.changed().await.is_ok() {
    if let Some(data) = qr_rx.borrow().as_ref() {
        let qr = QrRender::new(data).unwrap();

        // Choose your format:
        print!("{}", qr.terminal());           // compact half-block Unicode
        let html = qr.html();                  // self-contained HTML page
        let svg = qr.svg();                    // SVG string for embedding
        let png_bytes = qr.png(8);             // PNG at 8px per module
        qr.save_png("/tmp/qr.png", 10)?;       // save to file
    }
}
```

### Public API

| Method | Description |
|--------|-------------|
| `WhatsAppBridge::start(config, tx, cancel)` | Start bridge, returns handle |
| `send_message(jid, text)` | Send text (queued) |
| `send_message_with_id(jid, text) -> String` | Send text, return message ID |
| `send_image(jid, data, mime, caption)` | Send image |
| `send_audio(jid, data, mime, caption)` | Send audio/voice note |
| `send_video(jid, data, mime, caption)` | Send video |
| `send_document(jid, data, mime, filename)` | Send document |
| `send_sticker(jid, data, mime, animated)` | Send sticker |
| `send_location(jid, lat, lon, name, addr)` | Send location pin |
| `send_contact(jid, name, vcard)` | Send contact card |
| `send_reply(jid, reply_id, sender, text)` | Reply quoting a message |
| `send_reaction(jid, msg_id, emoji, from_me)` | React to a message |
| `edit_message(jid, msg_id, new_text)` | Edit a sent message |
| `revoke_message(jid, msg_id)` | Delete a sent message |
| `start_typing(jid)` / `stop_typing(jid)` | Typing indicators |
| `state() -> BridgeState` | Current connection state |
| `is_connected() -> bool` | Quick connectivity check |
| `subscribe_qr() -> Receiver<Option<String>>` | QR code events for pairing UI |
| `stop()` / `wait_stopped(timeout)` | Graceful shutdown |

### Inbound messages

`WhatsAppInbound` contains `sender`, `jid`, `reply_to`, `bridge_id`, and `content`:

| `InboundContent` variant | Fields |
|--------------------------|--------|
| `Text` | `body` |
| `Image` | `data`, `mime`, `caption` |
| `Audio` | `data`, `mime`, `seconds`, `is_voice` |
| `Video` | `data`, `mime`, `caption` |
| `Document` | `data`, `mime`, `filename` |
| `Sticker` | `data`, `mime`, `is_animated` |
| `Location` | `lat`, `lon`, `name`, `address` |
| `Contact` | `display_name`, `vcard` |
| `Reaction` | `target_id`, `emoji` |
| `Edit` | `target_id`, `new_text` |
| `Revoke` | `target_id` |

Media types include the downloaded bytes — no separate download step needed.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `WHATSAPP_PAIR_PHONE` | *(QR mode)* | Phone number for pair-code linking |
| `WHATSAPP_ALLOWED` | *(all)* | Comma-separated allowlisted phone numbers |
| `HEALTH_PORT` | `0` (disabled) | TCP port for JSON health endpoint |
| `BACKUP_DIR` | `whatsapp.db.backups` | Directory for SQLite backups |
| `RUST_LOG` | `info` | Log level (`debug` for protocol details) |

## Architecture

```
src/
  main.rs           — REPL + signal handling + instance lock       (507 lines)
  bridge.rs         — Core bridge: events, messaging, queue, health (2157 lines)
  storage.rs        — rusqlite Signal Protocol store (15 tables)    (1469 lines)
  qr.rs             — Multi-format QR rendering                     (235 lines)
  instance_lock.rs  — flock-based single-instance guard             (98 lines)
                                                            total: 4466 lines
```

**Key design choices:**
- Single `parking_lot::Mutex<Connection>` + `spawn_blocking` — no async SQLite overhead
- WAL mode + `synchronous=NORMAL` — fast writes without corruption risk
- Channel-based architecture (`mpsc` inbound/outbound) — clean consumer interface
- `watch` channel for state + QR events — multiple subscribers, no polling
- Single-device only (no `device_id` column) — halves query complexity

## Dependencies

Core: `tokio`, `rusqlite` (bundled), `anyhow`, `tracing`
WhatsApp: `whatsapp-rust` (git-pinned), `wacore`, `waproto`, `prost`
Utilities: `parking_lot`, `chrono`, `fs2`, `qrcode`, `png`, `serde`, `serde_json`

Single binary. No Node.js. No Diesel. No migration framework.
