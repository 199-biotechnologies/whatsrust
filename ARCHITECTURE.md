# Architecture

whatsrust is a pure Rust WhatsApp Web bridge. ~10,000 lines across 13 files.

## Module Map

```
src/
  bridge.rs          Core: event loop, messaging, groups, delivery receipts, group cache, presence, chat management, status/stories
  outbound.rs        Typed outbound ops (17 OpKinds), payload structs, execute_job()
  bridge_events.rs   Broadcast event bus: BridgeEvent, OutboundStatusEvent, DeliveryStatus
  api.rs             REST API (54 endpoints), SSE streaming, CLI HTTP client
  mcp.rs             MCP server (30 tools, JSON-RPC over stdio)
  storage.rs         rusqlite Signal Protocol store + typed job queue + inbound history
  dedup.rs           Generation-tracked DashMap dedup (concurrent-safe, bounded)
  read_receipts.rs   Batched receipt scheduler with flush-before-reply
  polls.rs           Poll crypto (HKDF-SHA256 + AES-256-GCM)
  qr.rs              QR rendering (terminal/PNG/HTML/SVG)
  instance_lock.rs   flock-based single-instance guard (prevents StreamReplaced loops)
  main.rs            Daemon (REPL + API), CLI client (54 commands), MCP mode
  lib.rs             Library crate exports (all modules pub, consumed by habb)
```

## Data Flow

```
                          ┌──────────────┐
                          │  WhatsApp    │
                          │  Servers     │
                          └──────┬───────┘
                                 │ WebSocket (Noise Protocol + Signal E2EE)
                          ┌──────┴───────┐
                          │  wa-rs       │  ← whatsapp-rust library (git-pinned)
                          │  (Client)    │
                          └──────┬───────┘
                                 │ Event callbacks (tokio::spawn per event)
                          ┌──────┴───────┐
                          │  bridge.rs   │
                          │  handle_event│
                          └──┬───────┬───┘
                             │       │
              ┌──────────────┘       └──────────────┐
              ▼                                      ▼
    ┌──────────────┐                      ┌──────────────────┐
    │ inbound_tx   │ mpsc channel         │ outbound queue   │ SQLite-backed
    │ (to consumer)│                      │ (from consumer)  │
    └──────────────┘                      └──────────────────┘
```

**Inbound path:** wa-rs event → `handle_event` → dedup check → content extraction (media download if needed) → `WhatsAppInbound` on mpsc channel → consumer.

**Outbound path:** Consumer calls `send_message()` → SQLite queue → `handle_outbound` loop → anti-ban pacing → `client.send_message()` → retry on failure.

**Chat management path:** `pin_chat()`, `mute_chat()`, `archive_chat()`, `mark_read()`, `delete_chat()`, `star_message()` use direct `client` calls (not the outbound queue). These are app-state mutations, not messages — they don't need queueing, retry, or pacing.

**Status/story path:** `send_status_text()`, `send_status_image()`, `send_status_video()`, `revoke_status()` go through the standard outbound queue. Status messages are regular messages sent to `status@broadcast`.

## Key Design Decisions

**parking_lot::Mutex + spawn_blocking for SQLite.** No async SQLite driver needed. WAL mode + `synchronous=NORMAL` gives fast writes without corruption risk.

**DashMap with generation counter for dedup.** Lock-free concurrent access. Generation tracking prevents eviction corruption after remove+re-admit cycles.

**SQLite-first sends.** All 17 outbound op types write to SQLite before returning success. Outbound worker wakes via `tokio::sync::Notify`, claims jobs with `claim_next_job()`, executes via `execute_job()` (media upload + send). Survives crashes — inflight jobs are requeued on restart.

**Broadcast event bus.** `tokio::sync::broadcast` (cap 256) carries `BridgeEvent::Inbound`, `OutboundStatus`, `Heartbeat`. Feeds SSE endpoint, in-process subscribers, and sync waiters (`enqueue_and_wait` subscribes BEFORE enqueue to avoid race).

**Token-bucket rate limiter.** Allows short bursts (default 5) while enforcing sustained rate (400ms/msg + jitter). Passive refill, no background task.

**Channel architecture.** `mpsc` for inbound to consumer, `broadcast` for event bus, `watch` for state + QR. `Notify` for outbound worker wakeup.

**Single-device only.** No `device_id` column in the protocol store. Cuts query complexity in half vs multi-device implementations.

**Read receipt batching.** Groups message IDs by (chat, participant) on a 200ms coalesce timer. Matches WhatsApp Web's native batching pattern. Flush-before-reply ensures read receipts go out before bot responses.

**Atomic BridgeMetrics.** All counters use `AtomicU64` — no locks, no DB reads for health checks. API server is raw TCP (no HTTP framework dependency) with connection semaphore (64) + dedicated SSE semaphore (8).

## Dependencies

The bridge depends on `whatsapp-rust` (wa-rs) by jlucaso1, git-pinned to a specific commit. wa-rs handles: WebSocket transport, Noise Protocol handshake, Signal Protocol encryption/decryption, protobuf encoding, keepalive pings, media upload/download.

The bridge layer handles: reconnection, state management, dedup, queueing, pacing, receipts, metrics, QR rendering, and the consumer API.
