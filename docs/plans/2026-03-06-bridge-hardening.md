# Bridge Hardening Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix 10 issues found via 3-model review: dedup race, read receipt scheduling, group reactions, presence, privacy-aware receipts.

**Architecture:** Extract dedup, read receipts, and presence into standalone modules with unit tests. Introduce `MessageRef` type for target-message addressing. Add `dashmap` dependency for atomic dedup. Keep bridge.rs as orchestrator.

**Tech Stack:** Rust, tokio, dashmap, parking_lot, waproto protobuf types

---

### Task 1: Add `dashmap` dependency

**Files:**
- Modify: `Cargo.toml:18` (dependencies section)

**Step 1: Add dashmap to Cargo.toml**

Add after `chrono = "0.4"`:
```toml
dashmap = "6"
```

**Step 2: Verify it compiles**

Run: `cd /Users/biobook/Projects/picoclaw-wa-rust && cargo check 2>&1 | head -20`
Expected: compiles with no errors

**Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "deps: add dashmap for atomic dedup"
```

---

### Task 2: Atomic DedupCache with DashMap

Replace the racy `ParkingMutex<DedupCache>` (check-then-insert across await points) with an atomic `DashMap`-based dedup that supports `InFlight`/`Done` states and removal on transient failure.

**Files:**
- Create: `src/dedup.rs`
- Modify: `src/bridge.rs:59-93` (remove old DedupCache)
- Modify: `src/bridge.rs:10` (add `mod dedup;`)
- Modify: `src/lib.rs` (add `pub mod dedup;`)

**Step 1: Write the failing tests in `src/dedup.rs`**

```rust
use dashmap::DashMap;
use std::collections::VecDeque;
use std::sync::Mutex;

/// Dedup state: InFlight means extraction in progress, Done means fully processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupState {
    InFlight,
    Done,
}

/// Thread-safe, atomic dedup cache with bounded capacity.
/// Uses DashMap for lock-free concurrent access and a Mutex<VecDeque> for eviction order.
pub struct AtomicDedupCache {
    map: DashMap<String, DedupState>,
    order: Mutex<VecDeque<String>>,
    capacity: usize,
}

impl AtomicDedupCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            map: DashMap::with_capacity(capacity),
            order: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    /// Try to admit a message ID. Returns `true` if this caller won the race
    /// (inserted as InFlight). Returns `false` if already present.
    pub fn try_admit(&self, id: &str) -> bool {
        use dashmap::mapref::entry::Entry;
        match self.map.entry(id.to_string()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(v) => {
                v.insert(DedupState::InFlight);
                let mut order = self.order.lock().unwrap();
                order.push_back(id.to_string());
                // Evict oldest if over capacity
                while order.len() > self.capacity {
                    if let Some(old) = order.pop_front() {
                        self.map.remove(&old);
                    }
                }
                true
            }
        }
    }

    /// Mark a message as fully processed.
    pub fn mark_done(&self, id: &str) {
        if let Some(mut entry) = self.map.get_mut(id) {
            *entry = DedupState::Done;
        }
    }

    /// Remove a message from dedup (allows retry on transient failure).
    pub fn remove(&self, id: &str) {
        self.map.remove(id);
        // Don't bother removing from order vec — it'll be evicted naturally
    }

    /// Check if an ID is present (any state).
    pub fn contains(&self, id: &str) -> bool {
        self.map.contains_key(id)
    }

    /// Current number of entries.
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_admit_returns_true_first_time() {
        let cache = AtomicDedupCache::new(10);
        assert!(cache.try_admit("msg1"));
        assert!(cache.contains("msg1"));
    }

    #[test]
    fn test_admit_returns_false_for_duplicate() {
        let cache = AtomicDedupCache::new(10);
        assert!(cache.try_admit("msg1"));
        assert!(!cache.try_admit("msg1")); // second caller loses
    }

    #[test]
    fn test_mark_done() {
        let cache = AtomicDedupCache::new(10);
        cache.try_admit("msg1");
        cache.mark_done("msg1");
        // Still present (can't re-admit)
        assert!(!cache.try_admit("msg1"));
        assert!(cache.contains("msg1"));
    }

    #[test]
    fn test_remove_allows_retry() {
        let cache = AtomicDedupCache::new(10);
        cache.try_admit("msg1");
        cache.remove("msg1");
        // After removal, can be re-admitted
        assert!(cache.try_admit("msg1"));
    }

    #[test]
    fn test_capacity_eviction() {
        let cache = AtomicDedupCache::new(3);
        cache.try_admit("a");
        cache.try_admit("b");
        cache.try_admit("c");
        // Full — admitting "d" evicts "a"
        cache.try_admit("d");
        assert!(!cache.contains("a")); // evicted
        assert!(cache.contains("b"));
        assert!(cache.contains("d"));
    }

    #[test]
    fn test_concurrent_admit_only_one_wins() {
        let cache = Arc::new(AtomicDedupCache::new(100));
        let mut handles = vec![];
        let wins = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        for _ in 0..100 {
            let c = cache.clone();
            let w = wins.clone();
            handles.push(std::thread::spawn(move || {
                if c.try_admit("race_id") {
                    w.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // Exactly one thread should have won
        assert_eq!(wins.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
```

**Step 2: Run tests to verify they pass**

Run: `cd /Users/biobook/Projects/picoclaw-wa-rust && cargo test dedup -- --nocapture 2>&1 | tail -15`
Expected: all 6 tests pass

**Step 3: Wire into bridge.rs — replace old DedupCache usage**

In `src/bridge.rs`:
- Remove lines 59-93 (old `DedupCache` struct and impl)
- Add `use crate::dedup::AtomicDedupCache;` to imports
- Replace `Arc<ParkingMutex<DedupCache>>` with `Arc<AtomicDedupCache>` everywhere
- At line 868: `let dedup = Arc::new(AtomicDedupCache::new(DEDUP_CACHE_CAPACITY));`
- At line 1247: replace `dedup.lock().is_seen(&info.id)` with `!dedup.try_admit(&info.id)` (atomic admit — if false, skip)
- After successful enqueue (line ~1288): `dedup.mark_done(&info.id);`
- At line 1338 (Unhandled): `dedup.mark_done(&info.id);` (already admitted, just mark done)
- At line 1346 (TransientFailure): `dedup.remove(&info.id);` (allow retry)
- Remove all other `dedup.lock().insert()` calls — they're replaced by the atomic admit

In `src/lib.rs`: add `pub mod dedup;`

In `src/bridge.rs` imports: remove `use std::collections::HashSet;` if no longer used.

**Step 4: Remove old DedupCache test from bridge.rs tests**

Remove the `test_dedup_cache` test at lines 2146-2164 (moved to dedup.rs).

**Step 5: Verify everything compiles and tests pass**

Run: `cd /Users/biobook/Projects/picoclaw-wa-rust && cargo test 2>&1 | tail -10`
Expected: all tests pass

**Step 6: Commit**

```bash
git add src/dedup.rs src/bridge.rs src/lib.rs Cargo.toml
git commit -m "fix: atomic dedup with DashMap — eliminates race condition on concurrent event handlers"
```

---

### Task 3: Read Receipt Scheduler

Extract read receipt logic from event handler into a background task with batching.

**Files:**
- Create: `src/read_receipts.rs`
- Modify: `src/bridge.rs:1296-1334` (remove inline receipt logic)
- Modify: `src/bridge.rs:868` (spawn scheduler)
- Modify: `src/lib.rs` (add `pub mod read_receipts;`)

**Step 1: Write `src/read_receipts.rs` with tests**

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, warn};

use whatsapp_rust::Client;

/// Commands sent to the read receipt scheduler.
#[derive(Debug)]
pub enum ReadReceiptCmd {
    /// Queue a message to be marked as read (batched by chat).
    Seen {
        chat_jid: String,
        participant_jid: Option<String>,
        message_id: String,
    },
    /// Force-flush all pending receipts for a chat (call before replying).
    FlushChat(String),
    /// Update the client handle (on reconnect).
    SetClient(Option<Arc<Client>>),
    /// Shutdown.
    Stop,
}

/// Batch key: (chat_jid, participant_jid).
type BatchKey = (String, Option<String>);

/// Pending batch: message IDs grouped by chat+participant.
#[derive(Default)]
struct PendingBatch {
    batches: HashMap<BatchKey, Vec<String>>,
}

impl PendingBatch {
    fn insert(&mut self, chat: String, participant: Option<String>, msg_id: String) {
        self.batches
            .entry((chat, participant))
            .or_default()
            .push(msg_id);
    }

    fn drain_chat(&mut self, chat: &str) -> Vec<(BatchKey, Vec<String>)> {
        let keys: Vec<BatchKey> = self
            .batches
            .keys()
            .filter(|(c, _)| c == chat)
            .cloned()
            .collect();
        keys.into_iter()
            .filter_map(|k| self.batches.remove(&k).map(|ids| (k, ids)))
            .collect()
    }

    fn drain_all(&mut self) -> HashMap<BatchKey, Vec<String>> {
        std::mem::take(&mut self.batches)
    }

    fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }
}

/// Spawn the read receipt scheduler. Returns a sender for commands.
pub fn spawn_scheduler(
    client: Option<Arc<Client>>,
    coalesce_ms: u64,
) -> mpsc::Sender<ReadReceiptCmd> {
    let (tx, rx) = mpsc::channel(512);
    tokio::spawn(run_scheduler(rx, client, coalesce_ms));
    tx
}

async fn run_scheduler(
    mut rx: mpsc::Receiver<ReadReceiptCmd>,
    mut client: Option<Arc<Client>>,
    coalesce_ms: u64,
) {
    let mut pending = PendingBatch::default();
    let mut interval = tokio::time::interval(Duration::from_millis(coalesce_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some(ReadReceiptCmd::Seen { chat_jid, participant_jid, message_id }) => {
                        pending.insert(chat_jid, participant_jid, message_id);
                    }
                    Some(ReadReceiptCmd::FlushChat(chat)) => {
                        let flushed = pending.drain_chat(&chat);
                        if let Some(ref c) = client {
                            for ((chat_jid, participant), msg_ids) in flushed {
                                send_receipt(c, &chat_jid, participant.as_deref(), msg_ids).await;
                            }
                        }
                    }
                    Some(ReadReceiptCmd::SetClient(c)) => {
                        client = c;
                    }
                    Some(ReadReceiptCmd::Stop) | None => break,
                }
            }
            _ = interval.tick() => {
                if !pending.is_empty() {
                    if let Some(ref c) = client {
                        for ((chat_jid, participant), msg_ids) in pending.drain_all() {
                            send_receipt(c, &chat_jid, participant.as_deref(), msg_ids).await;
                        }
                    }
                }
            }
        }
    }
    // Drain remaining on shutdown
    if let Some(ref c) = client {
        for ((chat_jid, participant), msg_ids) in pending.drain_all() {
            send_receipt(c, &chat_jid, participant.as_deref(), msg_ids).await;
        }
    }
    debug!("read receipt scheduler stopped");
}

async fn send_receipt(
    client: &Client,
    chat_jid: &str,
    participant: Option<&str>,
    msg_ids: Vec<String>,
) {
    if msg_ids.is_empty() {
        return;
    }
    let chat = match wacore_binary::jid::Jid::from_str(chat_jid) {
        Ok(j) => j,
        Err(e) => {
            warn!(error = %e, chat = chat_jid, "invalid chat JID for read receipt");
            return;
        }
    };
    let group_sender = participant.and_then(|p| wacore_binary::jid::Jid::from_str(p).ok());
    if let Err(e) = client
        .mark_as_read(&chat, group_sender.as_ref(), msg_ids)
        .await
    {
        warn!(error = %e, chat = chat_jid, "failed to send batched read receipt");
    }
}

use std::str::FromStr;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pending_batch_insert_and_drain() {
        let mut batch = PendingBatch::default();
        batch.insert("chat1".into(), None, "m1".into());
        batch.insert("chat1".into(), None, "m2".into());
        batch.insert("chat2".into(), Some("sender@s.whatsapp.net".into()), "m3".into());

        assert!(!batch.is_empty());

        // Drain chat1 — should get m1, m2
        let drained = batch.drain_chat("chat1");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].1, vec!["m1", "m2"]);

        // chat2 still there
        assert!(!batch.is_empty());

        // Drain all
        let all = batch.drain_all();
        assert_eq!(all.len(), 1);
        assert!(batch.is_empty());
    }

    #[test]
    fn test_pending_batch_groups_by_participant() {
        let mut batch = PendingBatch::default();
        batch.insert("group@g.us".into(), Some("alice@s.whatsapp.net".into()), "m1".into());
        batch.insert("group@g.us".into(), Some("bob@s.whatsapp.net".into()), "m2".into());
        batch.insert("group@g.us".into(), Some("alice@s.whatsapp.net".into()), "m3".into());

        let all = batch.drain_all();
        // Two separate batches: one for alice (m1, m3), one for bob (m2)
        assert_eq!(all.len(), 2);
        let alice_key = ("group@g.us".to_string(), Some("alice@s.whatsapp.net".to_string()));
        let alice_msgs = &all[&alice_key];
        assert_eq!(alice_msgs, &vec!["m1", "m3"]);
    }

    #[test]
    fn test_drain_chat_returns_empty_for_unknown() {
        let mut batch = PendingBatch::default();
        batch.insert("chat1".into(), None, "m1".into());
        let drained = batch.drain_chat("nonexistent");
        assert!(drained.is_empty());
    }

    #[tokio::test]
    async fn test_scheduler_batches_and_stops() {
        // Just verify the scheduler can start and stop without panicking
        let tx = spawn_scheduler(None, 50);
        tx.send(ReadReceiptCmd::Seen {
            chat_jid: "test@s.whatsapp.net".into(),
            participant_jid: None,
            message_id: "m1".into(),
        }).await.unwrap();
        tx.send(ReadReceiptCmd::Stop).await.unwrap();
        // Give it a moment to shut down
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
```

**Step 2: Run tests**

Run: `cd /Users/biobook/Projects/picoclaw-wa-rust && cargo test read_receipts -- --nocapture 2>&1 | tail -15`
Expected: all 4 tests pass

**Step 3: Wire into bridge.rs**

- Add `use crate::read_receipts::{ReadReceiptCmd, spawn_scheduler};` to imports
- In `run_bridge()` around line 868: spawn scheduler and store the tx
  ```rust
  let rr_tx = spawn_scheduler(None, 200); // 200ms coalesce
  ```
- Pass `rr_tx` through to `handle_event` (add parameter)
- On connect (line 1242), send `SetClient`:
  ```rust
  let _ = rr_tx.send(ReadReceiptCmd::SetClient(Some(client.clone()))).await;
  ```
- Replace the entire inline read receipt block (lines 1296-1334) with:
  ```rust
  if auto_mark_read && !info.source.is_from_me {
      let _ = rr_tx.send(ReadReceiptCmd::Seen {
          chat_jid: info.source.chat.to_string(),
          participant_jid: if info.source.is_group {
              Some(info.source.sender.to_string())
          } else {
              None
          },
          message_id: info.id.clone(),
      }).await;
  }
  ```
- Remove `ActivityTracker` type, `activity` parameter, `ACTIVE_CHAT_WINDOW` const, and `rr_delay_min`/`rr_delay_max` parameters — all replaced by the scheduler.
- Remove `read_receipt_delay_min_ms` and `read_receipt_delay_max_ms` from `BridgeConfig` and `Default`.
- On disconnect events, send `SetClient(None)`.
- Add `flush_read_receipts(chat_jid)` method to `WhatsAppBridge`:
  ```rust
  pub async fn flush_read_receipts(&self, chat_jid: &str) -> Result<()> {
      self.rr_tx.send(ReadReceiptCmd::FlushChat(chat_jid.to_string())).await
          .map_err(|e| anyhow::anyhow!("receipt scheduler closed: {e}"))
  }
  ```
  This requires storing `rr_tx` in the `WhatsAppBridge` struct.

**Step 4: Guard enqueue failure — only queue receipt after successful send**

Move the read receipt `Seen` send inside the `Ok(())` arm of `inbound_tx.try_send()` / `send()`, NOT after the match. This prevents blue ticks for never-delivered messages.

**Step 5: Verify everything compiles and tests pass**

Run: `cd /Users/biobook/Projects/picoclaw-wa-rust && cargo test 2>&1 | tail -10`
Expected: all tests pass

**Step 6: Commit**

```bash
git add src/read_receipts.rs src/bridge.rs src/lib.rs
git commit -m "feat: read receipt scheduler with batching, flush-before-reply, and enqueue guard"
```

---

### Task 4: MessageRef + Group Reactions Fix

**Files:**
- Modify: `src/bridge.rs:271-290` (add `sender_raw` usage for MessageRef)
- Modify: `src/bridge.rs:697-725` (fix send_reaction)
- Modify: `src/bridge.rs:175-178` (split Reaction into ReactionAdded/ReactionRemoved)
- Modify: `src/bridge.rs:1697-1705` (extract_content for reactions)

**Step 1: Add MessageRef type and conversion**

Add after `WhatsAppInbound` struct (around line 290):
```rust
/// Reference to a specific message — used for reactions, replies, edits.
#[derive(Debug, Clone)]
pub struct MessageRef {
    pub chat_jid: String,
    pub message_id: String,
    pub from_me: bool,
    /// Sender JID — required for group chats when from_me is false.
    pub sender_jid: Option<String>,
}

impl MessageRef {
    /// Create from an inbound message.
    pub fn from_inbound(msg: &WhatsAppInbound) -> Self {
        Self {
            chat_jid: msg.jid.clone(),
            message_id: msg.id.clone(),
            from_me: msg.is_from_me,
            sender_jid: if msg.is_from_me {
                None
            } else {
                Some(msg.sender_raw.clone())
            },
        }
    }
}
```

**Step 2: Split Reaction into ReactionAdded/ReactionRemoved**

Replace in `InboundContent`:
```rust
    /// Emoji reaction added to a message.
    ReactionAdded {
        target_id: String,
        emoji: String,
        /// Sender of the original message (for group context).
        target_sender: Option<String>,
    },
    /// Emoji reaction removed from a message.
    ReactionRemoved {
        target_id: String,
        /// Sender of the original message (for group context).
        target_sender: Option<String>,
    },
```

Update `kind()`: `ReactionAdded` → "reaction", `ReactionRemoved` → "reaction-removed".
Update `display_text()`: `ReactionAdded` → `format!("[react {emoji} on {target_id}]")`, `ReactionRemoved` → `format!("[unreact on {target_id}]")`.

**Step 3: Fix extract_content for reactions (line ~1698)**

```rust
    if let Some(ref reaction) = msg.reaction_message {
        if let Some(ref key) = reaction.key {
            let target_id = key.id.clone().unwrap_or_default();
            let emoji = reaction.text.clone().unwrap_or_default();
            let target_sender = key.participant.clone();
            if emoji.is_empty() {
                return ExtractResult::Content(InboundContent::ReactionRemoved {
                    target_id,
                    target_sender,
                });
            } else {
                return ExtractResult::Content(InboundContent::ReactionAdded {
                    target_id,
                    emoji,
                    target_sender,
                });
            }
        }
    }
```

**Step 4: Fix send_reaction with participant support**

Replace the current `send_reaction` method:
```rust
    /// Send an emoji reaction to a message.
    pub async fn send_reaction(
        &self,
        chat_jid: &str,
        target_message_id: &str,
        target_sender_jid: Option<&str>,
        emoji: &str,
        target_is_from_me: bool,
    ) -> Result<()> {
        let target = parse_jid(chat_jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        // In group chats, participant must be set to the original sender's JID
        let participant = if chat_jid.contains("@g.us") && !target_is_from_me {
            match target_sender_jid {
                Some(s) => Some(s.to_string()),
                None => return Err(anyhow::anyhow!(
                    "target_sender_jid is required for group reactions when target is not from_me"
                )),
            }
        } else {
            None
        };
        let msg = wa::Message {
            reaction_message: Some(wa::message::ReactionMessage {
                key: Some(wa::MessageKey {
                    remote_jid: Some(target.to_string()),
                    id: Some(target_message_id.to_string()),
                    from_me: Some(target_is_from_me),
                    participant,
                }),
                text: Some(emoji.to_string()),
                sender_timestamp_ms: Some(chrono::Utc::now().timestamp_millis()),
                ..Default::default()
            }),
            ..Default::default()
        };
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send reaction failed: {e}"))?;
        Ok(())
    }

    /// Remove an emoji reaction from a message.
    pub async fn remove_reaction(
        &self,
        chat_jid: &str,
        target_message_id: &str,
        target_sender_jid: Option<&str>,
        target_is_from_me: bool,
    ) -> Result<()> {
        self.send_reaction(chat_jid, target_message_id, target_sender_jid, "", target_is_from_me).await
    }
```

**Step 5: Update main.rs REPL react command**

Update the react command handler to pass sender JID (add optional `sender` arg or default to None for 1:1 testing).

**Step 6: Add tests**

Add to the `#[cfg(test)] mod tests` in bridge.rs:
```rust
    #[test]
    fn test_message_ref_from_inbound() {
        let msg = WhatsAppInbound {
            bridge_id: "default".into(),
            jid: "group@g.us".into(),
            id: "msg123".into(),
            content: InboundContent::Text { body: "hi".into() },
            sender: "447957491755".into(),
            sender_raw: "447957491755@s.whatsapp.net".into(),
            timestamp: 1000,
            reply_to: None,
            is_from_me: false,
        };
        let mref = MessageRef::from_inbound(&msg);
        assert_eq!(mref.chat_jid, "group@g.us");
        assert_eq!(mref.message_id, "msg123");
        assert!(!mref.from_me);
        assert_eq!(mref.sender_jid.as_deref(), Some("447957491755@s.whatsapp.net"));
    }

    #[test]
    fn test_message_ref_from_me_has_no_sender() {
        let msg = WhatsAppInbound {
            bridge_id: "default".into(),
            jid: "chat@s.whatsapp.net".into(),
            id: "msg456".into(),
            content: InboundContent::Text { body: "hi".into() },
            sender: "myphone".into(),
            sender_raw: "myphone@s.whatsapp.net".into(),
            timestamp: 1000,
            reply_to: None,
            is_from_me: true,
        };
        let mref = MessageRef::from_inbound(&msg);
        assert!(mref.from_me);
        assert!(mref.sender_jid.is_none());
    }

    #[test]
    fn test_reaction_added_display() {
        let content = InboundContent::ReactionAdded {
            target_id: "m1".into(),
            emoji: "👍".into(),
            target_sender: None,
        };
        assert_eq!(content.kind(), "reaction");
        assert!(content.display_text().contains("👍"));
    }

    #[test]
    fn test_reaction_removed_display() {
        let content = InboundContent::ReactionRemoved {
            target_id: "m1".into(),
            target_sender: None,
        };
        assert_eq!(content.kind(), "reaction-removed");
        assert!(content.display_text().contains("unreact"));
    }
```

**Step 7: Verify all tests pass**

Run: `cd /Users/biobook/Projects/picoclaw-wa-rust && cargo test 2>&1 | tail -10`
Expected: all tests pass

**Step 8: Commit**

```bash
git add src/bridge.rs src/main.rs
git commit -m "fix: group reactions with participant field, split ReactionAdded/Removed, add MessageRef"
```

---

### Task 5: Inbound ChatPresence + Outbound Recording

**Files:**
- Modify: `src/bridge.rs` (handle ChatPresence event, add send_recording/stop_recording)
- Modify: `src/bridge.rs:372` (add presence_tx to WhatsAppBridge)

**Step 1: Add PresenceEvent enum and channel**

Add near the types section:
```rust
/// Presence events surfaced to the consumer (typing, recording indicators).
#[derive(Debug, Clone)]
pub enum PresenceEvent {
    Composing { chat_jid: String, sender: String },
    Recording { chat_jid: String, sender: String },
    Paused { chat_jid: String, sender: String },
}
```

**Step 2: Add presence channel to WhatsAppBridge::start and pass through**

- Add `presence_tx: Option<mpsc::Sender<PresenceEvent>>` parameter to `BridgeConfig` (default None)
- Pass it through `run_bridge` → `handle_event`
- In `handle_event`, add a match arm for `Event::ChatPresence`:
```rust
        Event::ChatPresence(update) => {
            if let Some(ref ptx) = presence_tx {
                let chat_jid = update.source.chat.to_string();
                let sender = update.source.sender.to_string();
                let evt = match update.state {
                    wacore::types::presence::ChatPresence::Composing => {
                        match update.media {
                            wacore::types::presence::ChatPresenceMedia::Audio => {
                                PresenceEvent::Recording { chat_jid, sender }
                            }
                            _ => PresenceEvent::Composing { chat_jid, sender },
                        }
                    }
                    wacore::types::presence::ChatPresence::Paused => {
                        PresenceEvent::Paused { chat_jid, sender }
                    }
                };
                let _ = ptx.try_send(evt);
            }
        }
```

**Step 3: Add outbound recording methods**

Add alongside existing `start_typing`/`stop_typing`:
```rust
    /// Send a "recording audio" indicator to a chat.
    pub async fn start_recording(&self, jid: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client
            .chatstate()
            .send_recording(&target)
            .await
            .map_err(|e| anyhow::anyhow!("chatstate error: {e}"))
    }

    /// Cancel recording indicator for a chat.
    pub async fn stop_recording(&self, jid: &str) -> Result<()> {
        self.stop_typing(jid).await // same as paused
    }
```

**Step 4: Add subscribe method for presence events**

Add to `WhatsAppBridge`:
```rust
    /// Subscribe to presence events (typing, recording indicators).
    /// Must set presence_tx in BridgeConfig for these events to be delivered.
    pub fn subscribe_presence(&self) -> Option<mpsc::Receiver<PresenceEvent>> {
        // This would need architectural change — for now, presence_tx is set in config
        None // Consumer passes their own channel via BridgeConfig
    }
```

**Step 5: Add tests**

```rust
    #[test]
    fn test_presence_event_variants() {
        let composing = PresenceEvent::Composing {
            chat_jid: "chat@s.whatsapp.net".into(),
            sender: "user@s.whatsapp.net".into(),
        };
        let recording = PresenceEvent::Recording {
            chat_jid: "chat@s.whatsapp.net".into(),
            sender: "user@s.whatsapp.net".into(),
        };
        let paused = PresenceEvent::Paused {
            chat_jid: "chat@s.whatsapp.net".into(),
            sender: "user@s.whatsapp.net".into(),
        };
        // Just verify they can be constructed and debug-printed
        assert!(format!("{:?}", composing).contains("Composing"));
        assert!(format!("{:?}", recording).contains("Recording"));
        assert!(format!("{:?}", paused).contains("Paused"));
    }
```

**Step 6: Verify all tests pass**

Run: `cd /Users/biobook/Projects/picoclaw-wa-rust && cargo test 2>&1 | tail -10`

**Step 7: Commit**

```bash
git add src/bridge.rs src/lib.rs
git commit -m "feat: inbound ChatPresence events + outbound send_recording for voice note anti-ban"
```

---

### Task 6: Presence Management (Available/Unavailable)

**Files:**
- Modify: `src/bridge.rs` (send Available on connect, Unavailable on shutdown)

**Step 1: Send Available on connect**

In `handle_event` at `Event::Connected`, after setting client handle:
```rust
        Event::Connected(_) => {
            // ... existing code ...
            // Send presence Available so server sends us ChatPresence events
            let c = client.clone();
            tokio::spawn(async move {
                if let Err(e) = c.presence().send_available().await {
                    warn!(error = %e, "failed to send available presence on connect");
                }
            });
        }
```

Note: Check if wa-rs `presence()` API exists. If not, this may need a raw node send or upstream support. If the API doesn't exist, skip and log a TODO.

**Step 2: Send Unavailable on graceful shutdown**

In the shutdown section of `run_bridge()` (around line 1004), before the final state update:
```rust
    // Best-effort unavailable presence before shutdown
    if let Some(c) = client_handle.lock().take() {
        let _ = c.presence().send_unavailable().await;
    }
```

**Step 3: Verify compiles**

Run: `cd /Users/biobook/Projects/picoclaw-wa-rust && cargo check 2>&1 | head -20`

**Step 4: Commit**

```bash
git add src/bridge.rs
git commit -m "feat: auto-send Available on connect, Unavailable on shutdown"
```

---

### Task 7: Clean up removed config fields + final test sweep

**Files:**
- Modify: `src/bridge.rs` (remove dead config fields, clean imports)
- Modify: `src/main.rs` (update REPL react command for new API)

**Step 1: Remove dead config fields**

Remove from `BridgeConfig`:
- `read_receipt_delay_min_ms`
- `read_receipt_delay_max_ms`

Remove from `Default` impl.

Add to `BridgeConfig`:
- `presence_tx: Option<mpsc::Sender<PresenceEvent>>` with `Default` = `None`

**Step 2: Update REPL react command in main.rs**

Update to pass `None` as sender_jid for 1:1 testing, add optional 5th arg for group sender:
```rust
"react" => {
    let react_parts: Vec<&str> = line.splitn(6, ' ').collect();
    if react_parts.len() < 4 {
        println!("usage: react <jid> <msg_id> <emoji> [from_me=true] [sender_jid]");
        continue;
    }
    let from_me = react_parts.get(4).map(|v| *v != "false" && *v != "0").unwrap_or(true);
    let sender_jid = react_parts.get(5).map(|s| s.to_string());
    match bridge_for_repl.send_reaction(
        react_parts[1], react_parts[2], sender_jid.as_deref(), react_parts[3], from_me,
    ).await {
        Ok(()) => println!(">> reacted {} (from_me={})", react_parts[3], from_me),
        Err(e) => println!("!! react failed: {e}"),
    }
}
```

**Step 3: Clean up unused imports in bridge.rs**

Remove: `HashSet` (if fully replaced by DashMap), `ActivityTracker` type alias, `ACTIVE_CHAT_WINDOW` const.

**Step 4: Full test sweep**

Run: `cd /Users/biobook/Projects/picoclaw-wa-rust && cargo test 2>&1 | tail -10`
Expected: all tests pass

Run: `cd /Users/biobook/Projects/picoclaw-wa-rust && cargo clippy 2>&1 | head -20`
Expected: no warnings

**Step 5: Commit**

```bash
git add -A
git commit -m "chore: clean up dead config fields, update REPL for new reaction API"
```
