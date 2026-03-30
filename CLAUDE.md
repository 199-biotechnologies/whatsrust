# whatsrust

Pure Rust WhatsApp bridge. Single binary, no Node.js.

## wa-rs Dependency (Separate Repository)
- **Fork:** `199-biotechnologies/whatsapp-rust` (forked from jlucaso1/whatsapp-rust)
- **Local clone:** `../whatsapp-rust` (sibling directory)
- **Cargo.toml** points at the fork with pinned `rev`. `.cargo/config.toml` (gitignored) patches to local path for dev.
- **DO NOT** modify wa-rs files from this project. If a feature requires wa-rs changes, work in `../whatsapp-rust` instead.
- After pushing wa-rs changes, bump the `rev` in this project's `Cargo.toml`.

## Key Files
- `src/bridge.rs` — core bridge: events, all message types, typing, groups, polls, presence, delivery receipts, group cache
- `src/outbound.rs` — typed outbound ops (17 OpKinds), payload structs, execute_job() builds wa::Message + uploads media
- `src/bridge_events.rs` — broadcast event bus: BridgeEvent, OutboundStatusEvent, OutboundJobState, DeliveryStatus
- `src/api.rs` — REST API server (54 endpoints) + SSE streaming + CLI HTTP client
- `src/mcp.rs` — MCP server (30 tools, JSON-RPC over stdio, proxies to HTTP daemon)
- `src/storage.rs` — rusqlite Signal Protocol store + typed outbound queue + inbound history + search
- `src/polls.rs` — poll crypto (HKDF-SHA256 + AES-256-GCM)
- `src/dedup.rs` — generation-tracked DashMap dedup
- `src/read_receipts.rs` — batched receipt scheduler
- `src/qr.rs` — QR rendering (terminal/PNG/HTML/SVG)
- `src/instance_lock.rs` — single-instance file lock
- `src/lib.rs` — library crate entry: all modules pub (consumed by habb)
- `src/main.rs` — binary: daemon mode (REPL + API) + CLI client (54 commands) + MCP mode

## Patterns
- SQLite-first sends: all 17 ops enqueue to SQLite via `enqueue_job()`, worker executes via `execute_job()`
- `enqueue_and_wait()` subscribes to broadcast BEFORE enqueue for sync send methods
- `parse_jid()` for JID normalization (phone → @s.whatsapp.net, group → @g.us)
- `parking_lot::Mutex<Connection>` + `spawn_blocking` for SQLite
- `extract_content_inner` recursive descent for inbound message parsing
- Schema migrations via version check in `Store::new()` (currently v7)
- Token-bucket rate limiter (burst + sustained rate) for anti-ban pacing
- Chat management ops (pin, mute, archive, mark-read, delete, star) use direct client calls, not the outbound queue
- Status/story sending (text, image, video, revoke) goes through the outbound queue like regular messages
