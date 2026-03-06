# Architecture

whatsrust is a pure Rust WhatsApp Web bridge. ~5,000 lines across 7 files.

## Module Map

```
src/
  bridge.rs          Core: event loop, messaging, outbound queue, health endpoint, metrics
  storage.rs         rusqlite Signal Protocol store (identity, prekey, session, sender-key)
  dedup.rs           Generation-tracked DashMap dedup (concurrent-safe, bounded)
  read_receipts.rs   Batched receipt scheduler with flush-before-reply
  qr.rs              QR rendering (terminal/PNG/HTML/SVG)
  instance_lock.rs   flock-based single-instance guard (prevents StreamReplaced loops)
  main.rs            REPL + signal handling
  lib.rs             Library crate exports
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

## Key Design Decisions

**parking_lot::Mutex + spawn_blocking for SQLite.** No async SQLite driver needed. WAL mode + `synchronous=NORMAL` gives fast writes without corruption risk.

**DashMap with generation counter for dedup.** Lock-free concurrent access. Generation tracking prevents eviction corruption after remove+re-admit cycles.

**Channel architecture.** `mpsc` for inbound, `mpsc` for outbound, `watch` for state + QR. Clean separation between bridge internals and consumers.

**Single-device only.** No `device_id` column in the protocol store. Cuts query complexity in half vs multi-device implementations.

**Read receipt batching.** Groups message IDs by (chat, participant) on a 200ms coalesce timer. Matches WhatsApp Web's native batching pattern. Flush-before-reply ensures read receipts go out before bot responses.

**Atomic BridgeMetrics.** All counters use `AtomicU64` — no locks, no DB reads for health checks. Served over raw TCP (no HTTP framework dependency).

## Dependencies

The bridge depends on `whatsapp-rust` (wa-rs) by jlucaso1, git-pinned to a specific commit. wa-rs handles: WebSocket transport, Noise Protocol handshake, Signal Protocol encryption/decryption, protobuf encoding, keepalive pings, media upload/download.

The bridge layer handles: reconnection, state management, dedup, queueing, pacing, receipts, metrics, QR rendering, and the consumer API.
