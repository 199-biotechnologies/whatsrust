//! Bridge event bus — unified event stream for inbound messages, outbound status,
//! and delivery receipts. Backed by `tokio::sync::broadcast`.
//!
//! Consumers:
//! - Library callers via `WhatsAppBridge::subscribe_events()`
//! - SSE endpoint via `/api/events`
//! - Internal waiters (e.g. `send_message_with_id` waiting for send completion)

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::broadcast;

use crate::bridge::WhatsAppInbound;

/// Capacity of the broadcast channel. Slow receivers that fall behind
/// this many events will get `Lagged` and should reconnect.
const EVENT_BUS_CAPACITY: usize = 256;

/// Unified bridge event envelope.
#[derive(Debug, Clone)]
pub enum BridgeEvent {
    /// An inbound WhatsApp message or status update.
    Inbound(Arc<WhatsAppInbound>),
    /// Status change for an outbound job (queued → sending → sent → delivered → read).
    OutboundStatus(OutboundStatusEvent),
    /// Periodic heartbeat for keepalive (SSE clients).
    Heartbeat,
}

/// Status update for an outbound job.
#[derive(Debug, Clone, Serialize)]
pub struct OutboundStatusEvent {
    /// The queue row ID returned by `enqueue_job()`.
    pub job_id: i64,
    /// Current state of the job.
    pub state: OutboundJobState,
    /// WhatsApp message ID (available after successful send).
    pub wa_message_id: Option<String>,
    /// Error message (if state is Failed).
    pub error: Option<String>,
}

/// Lifecycle states for an outbound job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboundJobState {
    Queued,
    Sending,
    Sent,
    Delivered,
    Read,
    Played,
    Failed,
    Expired,
}

/// Delivery status for inbound receipt events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Sent,
    Delivered,
    Read,
    Played,
    Failed,
    Unknown,
}

/// Create a new event bus (sender + initial receiver).
pub fn new_event_bus() -> (broadcast::Sender<Arc<BridgeEvent>>, broadcast::Receiver<Arc<BridgeEvent>>) {
    broadcast::channel(EVENT_BUS_CAPACITY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_event_bus_send_receive() {
        let (tx, mut rx) = new_event_bus();
        tx.send(Arc::new(BridgeEvent::Heartbeat)).unwrap();
        let evt = rx.recv().await.unwrap();
        assert!(matches!(*evt, BridgeEvent::Heartbeat));
    }

    #[tokio::test]
    async fn test_outbound_status_serde() {
        let evt = OutboundStatusEvent {
            job_id: 42,
            state: OutboundJobState::Sent,
            wa_message_id: Some("ABC123".to_string()),
            error: None,
        };
        let json = serde_json::to_string(&evt).unwrap();
        assert!(json.contains("\"state\":\"sent\""));
        assert!(json.contains("\"job_id\":42"));
    }
}
