# picoclaw-wa-rust

Pure Rust WhatsApp bridge for picoclaw — lean replacement for the Baileys (Node.js) sidecar.

Uses [whatsapp-rust](https://github.com/jlucaso1/whatsapp-rust) for the WhatsApp Web protocol and a custom rusqlite backend for Signal Protocol storage.

## Features

**Messaging:** Text, image, audio, video, document, sticker, location, contact card, reaction, edit, revoke, reply/quote — all directions.

**Reliability:** Message dedup cache (4096-entry ring buffer), exponential backoff with jitter, DB integrity check on startup, sender allowlist, auto read receipts.

**Infrastructure:** QR + pair-code pairing, session persistence (SQLite/WAL), LID-to-phone resolution, typing indicators, 12+ event types handled, 0 compiler warnings.

## Quick Start

```bash
# QR pairing (default)
cargo run

# Phone number pairing
WHATSAPP_PAIR_PHONE="+447438689825" cargo run

# With sender allowlist
WHATSAPP_ALLOWED="447957491755,447438689825" cargo run
```

## Architecture

- `src/bridge.rs` — Core bridge: events, all message types, reconnection, dedup, outbound queue
- `src/storage.rs` — Lean rusqlite Signal Protocol store (parking_lot::Mutex, WAL mode, single-device)
- `src/main.rs` — Interactive REPL for testing all message types

~3400 lines total. Single binary, no Node.js, no Diesel, no migration framework.
