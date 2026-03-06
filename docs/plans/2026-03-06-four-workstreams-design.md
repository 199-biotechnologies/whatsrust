# Design: Four Workstreams for whatsrust v0.4

**Date:** 2026-03-06
**Status:** Approved (revised after Codex GPT-5.4 review)

## Measured Baselines

- Release binary: 5.0 MB (stripped, LTO, opt-level=z)
- RSS at startup: ~13 MB
- RSS after WS connect: ~15 MB
- Application code: 5,087 lines across 7 files
- Unique crate dependencies: 196

---

## Phase 1: Bugfixes (this session)

Codex found 4 bugs that must be fixed before any new work.

| ID | Bug | File | Detail |
|----|-----|------|--------|
| B1 | Dedup eviction corrupts on re-admit | dedup.rs:31,58 | `order` accumulates stale IDs after remove+re-admit. Later eviction deletes live entries. Fix: clean `order` on remove, or use a proper LRU. |
| B2 | Shutdown drain burns retry budget | bridge.rs:1228 | Calls `mark_outbound_failed` during drain. Should call `requeue_outbound` to preserve for next session. |
| B3 | FlushChat drops receipts when disconnected | read_receipts.rs:104 | Drains pending receipts, then checks client. If None, receipts are lost. Fix: check client first. |
| B4 | LoggedOut comment contradicts behavior | bridge.rs:1428-1431 | `stop_reconnect = true` but comment says "will reconnect". Fix comment or implement re-pair. |

## Phase 2: Reliability (this session)

| ID | Improvement | Detail |
|----|-------------|--------|
| R2 | Smart outbound retry | Per-message `next_attempt_at` + jitter. Classify permanent vs transient errors. Skip-ahead past failing messages to prevent head-of-line blocking. |
| R4 | Atomic BridgeMetrics | New struct with `started_at`, `last_connect_at`, `last_disconnect_at`, `last_inbound_at`, `last_outbound_ok_at`, `reconnect_count`, `messages_sent`, `messages_received`. Served from health endpoint — no DB reads. |
| R6 | Fix reconnect backoff reset | bridge.rs:1028 resets to 1s on every clean `SessionAction::Retry`. Graduated reset instead (halve, don't zero). |

**Dropped:** R1 (keepalive) — wa-rs already has a keepalive loop every 20-30s.
**Deferred:** R3 (rate limiter), R5 (StreamReplaced migration).

## Phase 3: Documentation (this session)

- Fix README numbers: 5 MB binary, ~15 MB RAM
- Fix Baileys comparison: ~90 MB install, ~50 MB RAM
- Add ARCHITECTURE.md
- Add CHANGELOG.md

## Phase 4: Performance (this session, low priority)

| ID | Optimization | Detail |
|----|-------------|--------|
| P4 | Dedup capacity + TTL | Expose via BridgeConfig. Only after B1 is fixed. |

**Deferred:** P1 (connection pool), P2 (batch dequeue — not bottleneck at 400ms pacing), P3 (media streaming).

## Phase 5: Group Management (next session)

Phase 1 (read-only):
- get_group_info(jid) -> name, description, participants, admins
- get_joined_groups() -> list
- Surface group metadata on WhatsAppInbound

Phase 2 (actions, follow-up):
- create_group, set_subject, set_description
- add/remove/promote/demote participants
- leave_group, get_invite_link, join_group_with_link

## Baileys-Inspired Additions (future)

From Codex review — adopt these concepts, done the Rust way:
- **Typed outbound ops:** `OutboundOp` enum in SQLite instead of `jid + payload TEXT`
- **shouldIgnoreJid:** Early-ignore filter for status/broadcast/newsletter traffic
- **Group/device caches:** Lightweight in-memory caches for metadata
- **History sync control:** Fix broken `skip_history_sync` config

## Execution Order

1. B1 → B2 → B3 → B4 (bugfixes)
2. R6 → R2 → R4 (reliability)
3. Docs (README + ARCHITECTURE.md)
4. P4 (dedup config)
5. Groups Phase 1 (next session)
