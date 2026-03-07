# whatsrust

Pure Rust WhatsApp bridge. Single binary, no Node.js.

## wa-rs Dependency (Separate Repository)
- **Fork:** `199-biotechnologies/whatsapp-rust` (forked from jlucaso1/whatsapp-rust)
- **Local clone:** `../whatsapp-rust` (sibling directory)
- **Cargo.toml** points at the fork with pinned `rev`. `.cargo/config.toml` (gitignored) patches to local path for dev.
- **DO NOT** modify wa-rs files from this project. If a feature requires wa-rs changes, work in `../whatsapp-rust` instead.
- After pushing wa-rs changes, bump the `rev` in this project's `Cargo.toml`.

## Key Files
- `src/bridge.rs` — core bridge: events, messaging, typing, groups, polls, presence
- `src/storage.rs` — rusqlite Signal Protocol store + outbound queue
- `src/polls.rs` — poll crypto (HKDF-SHA256 + AES-256-GCM)
- `src/dedup.rs` — generation-tracked DashMap dedup
- `src/read_receipts.rs` — batched receipt scheduler
- `src/qr.rs` — QR rendering (terminal/PNG/HTML/SVG)
- `src/main.rs` — REPL + signals + instance lock

## Patterns
- `parse_jid()` + `get_client_handle()` for all send methods
- `parking_lot::Mutex<Connection>` + `spawn_blocking` for SQLite
- `extract_content_inner` recursive descent for inbound message parsing
- Schema migrations via version check in `Store::new()`
