# Bridge Hardening Design

Addresses 10 issues found via 3-model code review (Claude + Codex GPT-5.4 + Gemini 3.1 Pro), validated against Baileys (JS) and whatsmeow (Go) source code.

## P0: Dedup Race Condition

**Problem:** wa-rs Bot `tokio::spawn`s each event handler. Two handlers for the same message can both pass `contains()` before either calls `insert()`.

**Fix:** Atomic admission via `DashMap::entry()` with `DedupState`:
```rust
enum DedupState { InFlight, Done }
```
Use `entry().or_insert()` semantics — first caller wins, second caller sees `InFlight`/`Done` and skips. On transient extraction failure, remove entry so message can retry on redeliver.

## P1: Read Receipt Scheduler

**Problem:** Inline `tokio::time::sleep().await` in event handler. Even though handlers are spawned (not sequential), receipts are unsophisticated: one stanza per message, no batching, no flush-before-reply.

**Fix:** Dedicated background task with channel:
```rust
enum ReadReceiptCmd {
    Seen { chat: String, participant: Option<String>, message_id: String },
    FlushChat(String),
    Stop,
}
```
- Batches by `(chat_jid, participant_jid)` — mirrors Baileys' `aggregateMessageKeysNotFromMe()`
- Short coalescing window (200ms) to catch bursts
- `FlushChat` API for reply code to enforce read→typing→send ordering
- Consumer/LLM manages "human-like" delays, not the bridge

## P2: Privacy-Aware Receipts

**Problem:** Bridge hardcodes `mark_as_read` without checking privacy settings. Both Baileys and whatsmeow downgrade to `read-self` when user has receipts disabled.

**Fix:** Check privacy setting from wa-rs client if available. Conservative default: `read-self` when unknown. Expose `set_read_receipt_mode(ReadMode)` for manual override.

## P2: Enqueue Failure Guard

**Problem:** At bridge.rs:1285, if inbound channel is closed, code falls through to read receipt logic — blue ticks for never-delivered messages.

**Fix:** Only send read receipt cmd after successful `inbound_tx.send()`.

## P3: Group Reactions — MessageRef

**Problem:** `send_reaction` always sets `participant: None`. Group reactions silently fail. whatsmeow's `BuildMessageKey` auto-sets participant for groups.

**Fix:** New `MessageRef` type:
```rust
pub struct MessageRef {
    pub chat_jid: String,
    pub message_id: String,
    pub from_me: bool,
    pub sender_jid: Option<String>,
}
```
- `send_reaction(target: &MessageRef, emoji: &str)` validates: group + !from_me requires sender_jid
- `remove_reaction(target: &MessageRef)` sugar over emoji=""
- `impl From<&WhatsAppInbound> for MessageRef`

## P4: Inbound ChatPresence + Outbound Recording

**Problem:** `Event::ChatPresence` falls through to catch-all. `send_recording()` not exposed.

**Fix:** Surface typing/recording via callback or separate channel:
```rust
pub enum PresenceEvent {
    Composing { chat_jid: String, sender: String },
    Recording { chat_jid: String, sender: String },
    Paused { chat_jid: String, sender: String },
}
```
Add `start_recording(jid)` / `stop_recording(jid)` alongside existing typing methods.

## P5: Presence Management

**Problem:** Bridge never sends Available/Unavailable. WhatsApp won't send ChatPresence events unless marked online.

**Fix:** Auto-send Available on connect, Unavailable on graceful shutdown. Verify pinned wa-rs doesn't already do this.

## Testing Strategy

Each fix gets unit tests:
- Dedup: concurrent insert races, transient failure cleanup, capacity eviction
- ReadReceiptScheduler: batching correctness, flush ordering, coalescing window
- MessageRef: group validation, 1:1 passthrough, from_me detection
- Reactions: group participant populated, removal via empty emoji
- Presence events: composing/recording/paused routing
