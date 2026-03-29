use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use whatsapp_rust::Client;

/// Commands sent to the read receipt scheduler.
#[allow(dead_code)] // Stop variant used in tests and available for graceful shutdown
pub enum ReadReceiptCmd {
    /// Queue a message to be marked as read (batched by chat).
    Seen {
        chat_jid: String,
        participant_jid: Option<String>,
        message_id: String,
    },
    /// Force-flush all pending receipts for a chat (call before replying).
    /// Ack is true if receipts were actually sent, false if no client was available.
    FlushChat {
        chat_jid: String,
        ack: oneshot::Sender<bool>,
    },
    /// Update the client handle (on reconnect).
    SetClient(Option<Arc<Client>>),
    /// Shutdown.
    Stop,
}

impl std::fmt::Debug for ReadReceiptCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Seen { chat_jid, message_id, .. } => {
                f.debug_struct("Seen").field("chat", chat_jid).field("msg", message_id).finish()
            }
            Self::FlushChat { chat_jid, .. } => write!(f, "FlushChat({chat_jid})"),
            Self::SetClient(c) => write!(f, "SetClient({})", if c.is_some() { "Some" } else { "None" }),
            Self::Stop => write!(f, "Stop"),
        }
    }
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
                    Some(ReadReceiptCmd::FlushChat { chat_jid, ack }) => {
                        if let Some(ref c) = client {
                            let flushed = pending.drain_chat(&chat_jid);
                            for ((chat_jid, participant), msg_ids) in flushed {
                                send_receipt(c, &chat_jid, participant.as_deref(), msg_ids).await;
                            }
                            let _ = ack.send(true);
                        } else {
                            // No client — leave receipts pending, tell caller flush didn't happen
                            let _ = ack.send(false);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pending_batch_insert_and_drain() {
        let mut batch = PendingBatch::default();
        batch.insert("chat1".into(), None, "m1".into());
        batch.insert("chat1".into(), None, "m2".into());
        batch.insert(
            "chat2".into(),
            Some("sender@s.whatsapp.net".into()),
            "m3".into(),
        );

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
        batch.insert(
            "group@g.us".into(),
            Some("alice@s.whatsapp.net".into()),
            "m1".into(),
        );
        batch.insert(
            "group@g.us".into(),
            Some("bob@s.whatsapp.net".into()),
            "m2".into(),
        );
        batch.insert(
            "group@g.us".into(),
            Some("alice@s.whatsapp.net".into()),
            "m3".into(),
        );

        let all = batch.drain_all();
        // Two separate batches: one for alice (m1, m3), one for bob (m2)
        assert_eq!(all.len(), 2);
        let alice_key = (
            "group@g.us".to_string(),
            Some("alice@s.whatsapp.net".to_string()),
        );
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
        })
        .await
        .unwrap();
        tx.send(ReadReceiptCmd::Stop).await.unwrap();
        // Give it a moment to shut down
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
