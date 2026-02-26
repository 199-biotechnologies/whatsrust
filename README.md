# whatsrust

**WhatsApp in pure Rust.** Single binary. No Node.js. No Baileys. No kidding.

Built by [Boris Djordjevic](https://github.com/borisdjordjevic) at [199 Biotechnologies](https://github.com/199-biotechnologies).

---

## Why does this exist?

Every WhatsApp bot project eventually hits the same wall: Baileys. A Node.js library that works until it doesn't, eats 200MB of RAM to say "hello," and crashes at 3 AM when your on-call engineer is asleep.

whatsrust fixes that. One Rust binary, ~15MB, handles everything Baileys does -- text, images, audio, video, documents, stickers, locations, contacts, reactions, edits, revokes, replies. It also does things Baileys doesn't: crash-safe message queues, automatic reconnection with jitter, SQLite backups on startup and shutdown, single-instance locking so two bridges can't fight over one session, plus a health endpoint that actually responds to HTTP properly.

We built this because we needed WhatsApp messaging inside agent software and got tired of babysitting a Node.js sidecar. So we replaced it.

---

## What can it do?

**Send and receive everything:**
text, image, audio/voice, video, document, sticker, location, contact card, reaction, edit, revoke, reply/quote.

**Stay alive when things go wrong:**
- Persistent outbound queue -- SQLite, crash-safe, claim/send/mark lifecycle
- Message dedup -- 4096-entry ring buffer, no double-processing
- Exponential backoff with jitter on reconnect (not the naive kind)
- Graceful shutdown -- drains inflight messages on SIGINT/SIGTERM
- Single-instance lock -- flock-based, so two bridges can't fight over one session
- SQLite backup on startup and shutdown
- Database pruning of old sent/failed messages

**Ship without drama:**
- QR + pair-code pairing with multi-format QR rendering (terminal, PNG, HTML, SVG)
- Health endpoint -- TCP, JSON response with connection state and queue depth
- Sender allowlist
- Anti-ban pacing -- configurable send intervals + randomized read receipt delays
- LID-to-phone sender resolution
- 15+ WhatsApp event types handled

---

## Get started

```bash
# Clone and run (QR pairing)
git clone https://github.com/199-biotechnologies/whatsrust
cd whatsrust && cargo run

# Phone number pairing
WHATSAPP_PAIR_PHONE="+1234567890" cargo run

# With allowlist + health check
WHATSAPP_ALLOWED="1234567890" HEALTH_PORT=8080 cargo run
```

Scan the QR code with your phone. You're connected. The built-in REPL gives you every message type: `send`, `reply`, `edit`, `react`, `image`, `audio`, `video`, `doc`, `sticker`, `location`, `contact`, `typing`, `status`, `quit`.

---

## Use it as a library

This was designed to be embedded. Add it to your project and wire it up in about 10 lines:

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

### QR code for your UI

Subscribe to QR events and render however you want -- terminal, HTML dashboard, native GUI:

```rust
use whatsrust::qr::QrRender;

let mut qr_rx = bridge.subscribe_qr();
while qr_rx.changed().await.is_ok() {
    if let Some(data) = qr_rx.borrow().as_ref() {
        let qr = QrRender::new(data).unwrap();

        print!("{}", qr.terminal());       // half-block Unicode, compact
        let html = qr.html();              // self-contained HTML page
        let svg = qr.svg();                // SVG for embedding
        let png = qr.png(8);               // PNG bytes, 8px per module
        qr.save_png("/tmp/qr.png", 10)?;   // save to file
    }
}
```

---

## Full API

| Method | What it does |
|--------|-------------|
| `WhatsAppBridge::start(config, tx, cancel)` | Start the bridge, get a handle back |
| `send_message(jid, text)` | Queue a text message |
| `send_message_with_id(jid, text)` | Send text, get the message ID back |
| `send_image(jid, data, mime, caption)` | Send an image |
| `send_audio(jid, data, mime, caption)` | Send audio or voice note |
| `send_video(jid, data, mime, caption)` | Send video |
| `send_document(jid, data, mime, filename)` | Send a file |
| `send_sticker(jid, data, mime, animated)` | Send a sticker |
| `send_location(jid, lat, lon, name, addr)` | Drop a pin |
| `send_contact(jid, name, vcard)` | Send a contact card |
| `send_reply(jid, reply_id, sender, text)` | Reply quoting a message |
| `send_reaction(jid, msg_id, emoji, from_me)` | React with an emoji |
| `edit_message(jid, msg_id, new_text)` | Edit a sent message |
| `revoke_message(jid, msg_id)` | Delete a sent message |
| `start_typing(jid)` / `stop_typing(jid)` | Typing indicators |
| `state()` | Current connection state |
| `is_connected()` | Quick check: are we online? |
| `subscribe_qr()` | Watch for QR code events |
| `subscribe_state()` | Watch for state changes |
| `stop()` / `wait_stopped(timeout)` | Shut down gracefully |

### Inbound messages

Every inbound message lands in your `mpsc` channel as a `WhatsAppInbound` with `sender`, `jid`, `reply_to`, `bridge_id`, and one of these:

| Type | What you get |
|------|-------------|
| `Text` | `body` |
| `Image` | `data`, `mime`, `caption` -- bytes included, no extra download |
| `Audio` | `data`, `mime`, `seconds`, `is_voice` |
| `Video` | `data`, `mime`, `caption` |
| `Document` | `data`, `mime`, `filename` |
| `Sticker` | `data`, `mime`, `is_animated` |
| `Location` | `lat`, `lon`, `name`, `address` |
| `Contact` | `display_name`, `vcard` |
| `Reaction` | `target_id`, `emoji` |
| `Edit` | `target_id`, `new_text` |
| `Revoke` | `target_id` |

Media arrives as raw bytes. No second download step. Open it, process it, do what you want with it.

---

## Config

| Variable | Default | What it controls |
|----------|---------|-----------------|
| `WHATSAPP_PAIR_PHONE` | *(QR mode)* | Phone number for pair-code linking |
| `WHATSAPP_ALLOWED` | *(everyone)* | Comma-separated sender allowlist |
| `HEALTH_PORT` | `0` (off) | TCP port for health endpoint |
| `BACKUP_DIR` | `whatsapp.db.backups` | Where SQLite backups go |
| `RUST_LOG` | `info` | Log verbosity (`debug` for protocol internals) |

---

## How it's built

```
src/
  main.rs           REPL + signals + instance lock          507 lines
  bridge.rs         Events, messaging, queue, health       2157 lines
  storage.rs        rusqlite Signal Protocol store          1469 lines
  qr.rs             QR rendering (terminal/PNG/HTML/SVG)     235 lines
  instance_lock.rs  flock-based single-instance guard         98 lines
                                                    total: 4466 lines
```

Five files. Under 4500 lines. That's the whole thing.

**Design choices worth knowing about:**
- `parking_lot::Mutex<Connection>` + `spawn_blocking` -- no async SQLite headaches
- WAL mode + `synchronous=NORMAL` -- fast writes, no corruption
- Channel architecture (`mpsc` inbound, `mpsc` outbound) -- clean for consumers
- `watch` channel for state + QR events -- multiple subscribers, zero polling
- Single-device only -- no `device_id` column, cuts query complexity in half

**Dependencies:** `tokio`, `rusqlite` (bundled), `anyhow`, `tracing`, `whatsapp-rust` (git-pinned), `wacore`, `waproto`, `prost`, `parking_lot`, `chrono`, `fs2`, `qrcode`, `png`, `serde`, `serde_json`. Single binary. No Node.js runtime. No Diesel ORM. No migration framework. `cargo build --release` and you're done.

---

## Baileys vs whatsrust

| | Baileys | whatsrust |
|---|---------|-----------|
| Language | Node.js | Rust |
| Binary size | ~200MB with node_modules | ~15MB |
| Memory at idle | ~150MB | ~20MB |
| Crash recovery | Roll your own | Built-in queue + backoff |
| Message types | All | All |
| Instance locking | No | flock-based |
| Health endpoint | No | JSON over TCP |
| SQLite backups | No | Automatic |
| Dependencies | npm install and pray | cargo build |

---

## License

MIT. Do what you want with it.

---

*Built with mass frustration and mass caffeine. By [Boris Djordjevic](https://github.com/borisdjordjevic).*
