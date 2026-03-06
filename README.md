<p align="center">
  <img src="https://img.shields.io/badge/rust-stable-orange?logo=rust" alt="Rust">
  <img src="https://img.shields.io/badge/binary_size-~15MB-brightgreen" alt="Binary Size">
  <img src="https://img.shields.io/badge/memory-~20MB-brightgreen" alt="Memory">
  <img src="https://img.shields.io/badge/license-MIT-blue" alt="License">
  <img src="https://img.shields.io/github/stars/199-biotechnologies/whatsrust?style=social" alt="Stars">
</p>

# whatsrust

**WhatsApp in pure Rust.** Single binary. No Node.js. No Baileys. No kidding.

Built by [Boris Djordjevic](https://github.com/borisdjordjevic).

---

## Why?

If you've built anything on WhatsApp, you've met Baileys. Node.js. 200MB of node_modules. Crashes at 3 AM. Memory leaks you can set your watch to. Works great until it doesn't, and then you're reading someone else's JavaScript at 4 AM wondering where your life went wrong.

whatsrust replaces all of that with one Rust binary. 15MB. 20MB RAM. Handles every message type Baileys does. Does a bunch of things Baileys doesn't.

We built this because we needed WhatsApp inside agent software and got tired of babysitting a Node sidecar. So we killed it.

---

## What it does

**Every message type:** text, image, audio/voice, video, document, sticker, location, contact card, reaction (add + remove), edit, revoke, reply/quote.

**Stays alive:**
- Crash-safe outbound queue in SQLite. Messages survive restarts.
- Atomic message dedup with DashMap. No double-processing, even under concurrent event handlers.
- Exponential backoff with jitter on reconnect. Not the naive kind.
- Graceful shutdown. Drains in-flight messages on SIGINT/SIGTERM.
- Single-instance flock. Two bridges can't fight over one session.
- SQLite backup on startup and shutdown. WAL mode, no corruption.

**Doesn't get you banned:**
- Read receipt batching. Groups message IDs per chat into single network stanzas on a 200ms coalesce.
- Flush-before-reply. Marks messages as read before responding, matching the human read-then-type-then-send pattern.
- Recording indicator. Shows "recording audio" before sending voice notes.
- Configurable send pacing with randomized intervals.
- Auto presence management. Available on connect, unavailable on shutdown.

**Ships clean:**
- QR + pair-code pairing with multi-format rendering (terminal, PNG, HTML, SVG)
- Health endpoint over TCP with JSON: connection state, queue depth
- Sender allowlist
- Typing and recording indicators (inbound and outbound)
- LID-to-phone sender resolution
- 15+ WhatsApp event types handled

---

## Quick start

```bash
git clone https://github.com/199-biotechnologies/whatsrust
cd whatsrust && cargo run
```

Scan the QR with your phone. Done.

```bash
# Phone number pairing instead of QR
WHATSAPP_PAIR_PHONE="+1234567890" cargo run

# With allowlist + health check
WHATSAPP_ALLOWED="1234567890" HEALTH_PORT=8080 cargo run
```

The built-in REPL gives you every command: `send`, `reply`, `edit`, `react`, `image`, `audio`, `video`, `doc`, `sticker`, `location`, `contact`, `typing`, `stop-typing`, `status`, `quit`.

---

## Use as a library

Designed to be embedded. About 10 lines to wire up:

```rust
use whatsrust::bridge::{BridgeConfig, WhatsAppBridge, WhatsAppInbound};
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

// Send
bridge.send_message_with_id("1234567890", "Hello from Rust").await?;

// Receive
while let Some(msg) = inbound_rx.recv().await {
    println!("{}: {}", msg.sender, msg.content.display_text());
}
```

### QR for your UI

Subscribe to QR events and render however you want:

```rust
use whatsrust::qr::QrRender;

let mut qr_rx = bridge.subscribe_qr();
while qr_rx.changed().await.is_ok() {
    if let Some(data) = qr_rx.borrow().as_ref() {
        let qr = QrRender::new(data).unwrap();
        print!("{}", qr.terminal());       // Unicode half-block, compact
        let html = qr.html();              // self-contained HTML page
        let svg = qr.svg();                // SVG for embedding
        let png = qr.png(8);               // PNG bytes, 8px module
        qr.save_png("/tmp/qr.png", 10)?;   // file
    }
}
```

### Flush receipts before replying

The anti-ban pattern: mark as read, wait, type, send. whatsrust handles the first part:

```rust
// Before your bot sends a reply:
bridge.flush_read_receipts("1234567890@s.whatsapp.net").await?;
bridge.start_typing("1234567890").await?;
// ... generate reply ...
bridge.stop_typing("1234567890").await?;
bridge.send_message_with_id("1234567890", &reply).await?;
```

---

## Full API

| Method | Does |
|--------|------|
| `WhatsAppBridge::start(config, tx, cancel)` | Start bridge, get handle |
| `send_message(jid, text)` | Queue a text message |
| `send_message_with_id(jid, text)` | Send text, get message ID back |
| `send_image(jid, data, mime, caption)` | Image |
| `send_audio(jid, data, mime, caption)` | Audio or voice note |
| `send_video(jid, data, mime, caption)` | Video |
| `send_document(jid, data, mime, filename)` | File |
| `send_sticker(jid, data, mime, animated)` | Sticker |
| `send_location(jid, lat, lon, name, addr)` | Pin |
| `send_contact(jid, name, vcard)` | Contact card |
| `send_reply(jid, reply_id, sender, text)` | Quote reply |
| `send_reaction(jid, msg_id, sender, emoji, from_me)` | React (works in groups) |
| `remove_reaction(jid, msg_id, sender, from_me)` | Remove reaction |
| `edit_message(jid, msg_id, new_text)` | Edit sent message |
| `revoke_message(jid, msg_id)` | Delete sent message |
| `start_typing(jid)` / `stop_typing(jid)` | Typing indicators |
| `start_recording(jid)` / `stop_recording(jid)` | Recording indicators |
| `flush_read_receipts(chat_jid)` | Force-send pending read receipts |
| `state()` / `is_connected()` | Connection state |
| `subscribe_qr()` | QR code events |
| `subscribe_state()` | State change events |
| `stop()` / `wait_stopped(timeout)` | Graceful shutdown |

### Inbound messages

Every message hits your `mpsc` channel as a `WhatsAppInbound` with `sender`, `jid`, `id`, `reply_to`, `is_from_me`, and content:

| Type | Fields |
|------|--------|
| `Text` | `body` |
| `Image` | `data`, `mime`, `caption` (bytes included, no extra download) |
| `Audio` | `data`, `mime`, `seconds`, `is_voice` |
| `Video` | `data`, `mime`, `caption` |
| `Document` | `data`, `mime`, `filename` |
| `Sticker` | `data`, `mime`, `is_animated` |
| `Location` | `lat`, `lon`, `name`, `address` |
| `Contact` | `display_name`, `vcard` |
| `ReactionAdded` | `target_id`, `emoji`, `target_sender` |
| `ReactionRemoved` | `target_id`, `target_sender` |
| `Edit` | `target_id`, `new_text` |
| `Revoke` | `target_id` |

Media arrives as raw bytes. No second download step.

---

## Config

| Variable | Default | Controls |
|----------|---------|----------|
| `WHATSAPP_PAIR_PHONE` | *(QR mode)* | Phone number for pair-code linking |
| `WHATSAPP_ALLOWED` | *(everyone)* | Comma-separated sender allowlist |
| `HEALTH_PORT` | `0` (off) | TCP port for health endpoint |
| `BACKUP_DIR` | `whatsapp.db.backups` | SQLite backup directory |
| `RUST_LOG` | `info` | Log level (`debug` for protocol) |

---

## How it's built

```
src/
  main.rs            REPL + signals + instance lock
  bridge.rs          Events, messaging, queue, health, presence
  storage.rs         rusqlite Signal Protocol store
  dedup.rs           Atomic DashMap dedup (concurrent-safe)
  read_receipts.rs   Batched receipt scheduler with flush-before-reply
  qr.rs              QR rendering (terminal/PNG/HTML/SVG)
  instance_lock.rs   flock-based single-instance guard
  lib.rs             Library crate exports
```

Seven files. Under 5000 lines. That's the whole thing.

**Design decisions:**
- `parking_lot::Mutex<Connection>` + `spawn_blocking` for SQLite. No async DB headaches.
- WAL mode + `synchronous=NORMAL`. Fast writes, no corruption.
- `DashMap::entry()` for dedup. Lock-free, atomic, handles concurrent event handlers from wa-rs's `tokio::spawn` per event.
- Channel architecture: `mpsc` inbound, `mpsc` outbound, `watch` for state + QR. Clean for consumers.
- Read receipt coalescing on a timer. Groups IDs by (chat, participant) into batched stanzas, matching what WhatsApp Web does.
- Single-device only. No `device_id` column. Cuts query complexity in half.

**Dependencies:** `tokio`, `rusqlite` (bundled), `dashmap`, `anyhow`, `tracing`, `whatsapp-rust` (git-pinned), `wacore`, `waproto`, `prost`, `parking_lot`, `chrono`, `fs2`, `qrcode`, `png`, `serde`, `serde_json`. Single binary. No Node.js runtime. No Diesel. No migration framework. `cargo build --release` and ship.

---

## Baileys vs whatsrust

| | Baileys | whatsrust |
|---|---------|-----------|
| Language | Node.js | Rust |
| Binary size | ~200MB with node_modules | ~15MB |
| Memory at idle | ~150MB | ~20MB |
| Crash recovery | Roll your own | Built-in queue + backoff |
| Message types | All | All |
| Message dedup | None | Atomic DashMap (concurrent-safe) |
| Read receipts | Per-message, inline | Batched, coalesced, flush-before-reply |
| Instance locking | No | flock-based |
| Health endpoint | No | JSON over TCP |
| SQLite backups | No | Automatic |
| Recording indicator | Manual | Built-in |
| Dependencies | npm install and pray | cargo build |

---

## Contributing

PRs welcome. Tests required. Run `cargo test` and `cargo clippy` before submitting.

## License

MIT. Do what you want with it.

---

*Built with mass frustration and mass caffeine. By [Boris Djordjevic](https://github.com/borisdjordjevic).*
