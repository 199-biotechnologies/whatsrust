<p align="center">
  <img src="https://img.shields.io/badge/rust-stable-orange?logo=rust" alt="Rust">
  <img src="https://img.shields.io/badge/binary_size-5MB-brightgreen" alt="Binary Size">
  <img src="https://img.shields.io/badge/memory-~15MB-brightgreen" alt="Memory">
  <img src="https://img.shields.io/badge/license-MIT-blue" alt="License">
  <img src="https://img.shields.io/github/stars/199-biotechnologies/whatsrust?style=social" alt="Stars">
</p>

# whatsrust

**WhatsApp in pure Rust.** Single binary. No Node.js. No Baileys. No kidding.

Built by [Boris Djordjevic](https://github.com/borisdjordjevic).

---

## Why?

If you've built anything on WhatsApp, you've met Baileys. Node.js. 200MB of node_modules. Crashes at 3 AM. Memory leaks you can set your watch to. Works great until it doesn't, and then you're reading someone else's JavaScript at 4 AM wondering where your life went wrong.

whatsrust replaces all of that with one Rust binary. 5MB. 15MB RAM. Handles every message type Baileys does. Does a bunch of things Baileys doesn't.

We built this because we needed WhatsApp inside agent software and got tired of babysitting a Node sidecar. So we killed it.

---

## What it does

**Every message type:** text, image, audio/voice, video, document, sticker, location, contact card, reaction (add + remove), edit, revoke, reply/quote, forward, view-once (image/video), poll (create + encrypted vote decryption).

**Full group management:** list groups, get group info, create, rename, set description, add/remove/promote/demote participants, invite links.

**Presence:** subscribe to contact online/offline status, typing/recording indicators with `MessageFlags` (forwarded, view-once) on every inbound message.

**Stays alive:**
- Crash-safe outbound queue in SQLite. Messages survive restarts.
- Atomic message dedup with DashMap. No double-processing, even under concurrent event handlers.
- Per-message exponential backoff in the outbound queue. One stuck message doesn't block the rest.
- Graduated reconnect backoff with jitter. Not the naive kind.
- Graceful shutdown. Drains in-flight messages on SIGINT/SIGTERM.
- JID filtering. Drops status broadcasts, newsletter, and server noise before processing.
- Single-instance flock. Two bridges can't fight over one session.
- SQLite backup on startup and shutdown. WAL mode, no corruption.

**Doesn't get you banned:**
- Read receipt batching. Groups message IDs per chat into single network stanzas on a 200ms coalesce.
- Flush-before-reply. Marks messages as read before responding, matching the human read-then-type-then-send pattern.
- Recording indicator. Shows "recording audio" before sending voice notes.
- Configurable send pacing with randomized jitter intervals.
- Auto presence management. Available on connect, unavailable on shutdown.

**Ships clean:**
- QR + pair-code pairing with multi-format rendering (terminal, PNG, HTML, SVG)
- REST API on localhost for tool integration (17 endpoints, JSON)
- CLI mode: `whatsrust send`, `whatsrust image`, etc. — every command returns JSON
- Claude Code skill for AI-driven WhatsApp messaging
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

# With allowlist + custom port
WHATSAPP_ALLOWED="1234567890" WHATSRUST_PORT=8080 cargo run
```

The built-in REPL gives you every command: `send`, `reply`, `edit`, `react`, `image`, `audio`, `video`, `doc`, `sticker`, `location`, `contact`, `forward`/`fwd`, `vo-image`, `vo-video`, `poll`, `subscribe`, `typing`, `stop-typing`, `groups`, `group-info`, `group-create`, `group-rename`, `group-desc`, `group-add`, `group-remove`, `group-promote`, `group-demote`, `group-invite`, `group-leave`, `status`, `quit`.

### CLI mode

With the daemon running, use `whatsrust` as a one-shot CLI. Every command returns JSON to stdout:

```bash
# Check status
whatsrust status

# Get QR code (for pairing via agent/script)
whatsrust qr --png /tmp/qr.png

# Send messages
whatsrust send 15551234567 "Hello from the CLI"
whatsrust image 15551234567 /tmp/photo.jpg "Check this out"
whatsrust video 15551234567 /tmp/clip.mp4
whatsrust doc 15551234567 /tmp/report.pdf
whatsrust react 15551234567 MSG_ID "👍"
whatsrust poll 15551234567 1 "Lunch?" -- "Pizza" "Sushi" "Tacos"

# Groups
whatsrust groups
whatsrust group-info 120363012345678901@g.us

# All commands: whatsrust help
```

Output:
```json
{"ok": true, "id": "3EB0A1B2C3D4E5F6"}
```

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
| `get_joined_groups()` | List all groups you're in |
| `get_group_info(group_jid)` | Group metadata + participants |
| `create_group(name, participants)` | Create a new group |
| `set_group_subject(group_jid, subject)` | Rename a group |
| `leave_group(group_jid)` | Leave a group |
| `set_group_description(group_jid, desc)` | Set or clear group description |
| `add_participants(group_jid, phones)` | Add members to a group |
| `remove_participants(group_jid, phones)` | Remove members from a group |
| `promote_participants(group_jid, phones)` | Make members admin |
| `demote_participants(group_jid, phones)` | Remove admin from members |
| `get_group_invite_link(group_jid)` | Get group invite link |
| `forward_message(dst_jid, msg_id)` | Forward a cached message |
| `send_view_once_image(jid, data, mime, caption)` | Ephemeral image |
| `send_view_once_video(jid, data, mime, caption)` | Ephemeral video |
| `send_poll(jid, question, options, selectable_count)` | Create a poll |
| `subscribe_presence(jid)` | Online/offline notifications |
| `stop()` / `wait_stopped(timeout)` | Graceful shutdown |

### Inbound messages

Every message hits your `mpsc` channel as a `WhatsAppInbound` with `sender`, `jid`, `id`, `reply_to`, `is_from_me`, `flags` (`MessageFlags` — `is_forwarded`, `forwarding_score`, `is_view_once`), and content:

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
| `PollCreated` | `question`, `options`, `selectable_count` |
| `PollVote` | `poll_id`, `selected_options` (decrypted) |

Media arrives as raw bytes. No second download step.

---

## Config

| Variable | Default | Controls |
|----------|---------|----------|
| `WHATSAPP_PAIR_PHONE` | *(QR mode)* | Phone number for pair-code linking |
| `WHATSAPP_ALLOWED` | *(everyone)* | Comma-separated sender allowlist |
| `WHATSRUST_PORT` | `7270` | API server port (0 = disabled) |
| `WHATSRUST_BIND` | `127.0.0.1` | API bind address |
| `HEALTH_PORT` | *(fallback)* | Legacy alias for `WHATSRUST_PORT` |
| `BACKUP_DIR` | `whatsapp.db.backups` | SQLite backup directory |
| `RUST_LOG` | `info` | Log level (`debug` for protocol) |

---

## How it's built

```
src/
  main.rs            Daemon mode (REPL) + CLI mode (one-shot commands)
  api.rs             REST API server + CLI HTTP client
  bridge.rs          Events, messaging, queue, presence, groups
  storage.rs         rusqlite Signal Protocol store
  dedup.rs           Atomic DashMap dedup (concurrent-safe)
  read_receipts.rs   Batched receipt scheduler with flush-before-reply
  qr.rs              QR rendering (terminal/PNG/HTML/SVG)
  polls.rs           Poll crypto (HKDF-SHA256 + AES-256-GCM)
  instance_lock.rs   flock-based single-instance guard
  lib.rs             Library crate exports
```

Ten files. Under 7000 lines. That's the whole thing.

**Design decisions:**
- `parking_lot::Mutex<Connection>` + `spawn_blocking` for SQLite. No async DB headaches.
- WAL mode + `synchronous=NORMAL`. Fast writes, no corruption.
- Generation-tracked `DashMap` for dedup. Lock-free, atomic, handles concurrent event handlers. Generation counter prevents eviction corruption on remove+re-admit cycles.
- Channel architecture: `mpsc` inbound, `mpsc` outbound, `watch` for state + QR. Clean for consumers.
- Per-message exponential backoff via `retry_after` column. Failing messages wait out their backoff while newer messages flow through — no head-of-line blocking.
- Read receipt coalescing on a timer. Groups IDs by (chat, participant) into batched stanzas, matching what WhatsApp Web does.
- `AtomicU64` bridge metrics. No locks, no DB reads for health checks.
- Single-device only. No `device_id` column. Cuts query complexity in half.

**Dependencies:** `tokio`, `rusqlite` (bundled), `dashmap`, `anyhow`, `tracing`, `whatsapp-rust` (git-pinned), `wacore`, `waproto`, `prost`, `parking_lot`, `chrono`, `fs2`, `qrcode`, `png`, `serde`, `serde_json`. Single binary. No Node.js runtime. No Diesel. No migration framework. `cargo build --release` and ship.

---

## Baileys vs whatsrust

| | Baileys | whatsrust |
|---|---------|-----------|
| Language | Node.js | Rust |
| Binary size | ~90MB with node_modules + runtime | 5MB |
| Memory at idle | ~50MB | ~15MB |
| Crash recovery | Roll your own | Built-in queue + backoff |
| Message types | All | All |
| Message dedup | None | Atomic DashMap (concurrent-safe) |
| Read receipts | Per-message, inline | Batched, coalesced, flush-before-reply |
| Instance locking | No | flock-based |
| REST API | No | 17 endpoints, CLI mode, JSON |
| SQLite backups | No | Automatic |
| Group management | Manual API calls | 12 methods, full CRUD |
| Recording indicator | Manual | Built-in |
| Outbound queue | None | SQLite-backed, per-message backoff |
| Dependencies | npm install and pray | cargo build |

---

## Contributing

PRs welcome. Tests required. Run `cargo test` and `cargo clippy` before submitting.

## License

MIT. Do what you want with it.

---

*Built with mass frustration and mass caffeine. By [Boris Djordjevic](https://github.com/borisdjordjevic).*
