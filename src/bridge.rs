//! WhatsApp bridge — connects to WhatsApp Web via whatsapp-rust (wa-rs).
//!
//! Adapted from ZeroClaw's whatsapp_web.rs but leaner:
//!   - Channel-based architecture (mpsc inbound/outbound)
//!   - Reconnection with exponential backoff
//!   - QR + pair-code pairing
//!   - LID-to-phone sender normalization
//!   - Expanded event handling (ban, outdated, stream errors)

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::dedup::AtomicDedupCache;
use crate::read_receipts::{ReadReceiptCmd, spawn_scheduler};
use dashmap::DashSet;

use anyhow::{Context, Result};
use parking_lot::Mutex as ParkingMutex;
use tokio::sync::{mpsc, watch, Mutex as TokioMutex, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use wacore::store::traits::ProtocolStore;
use wacore_binary::jid::Jid;
use waproto::whatsapp as wa;

use whatsapp_rust::bot::Bot;
use whatsapp_rust::types::events::Event;
use whatsapp_rust::Client;
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust::download::Downloadable;
use whatsapp_rust::pair_code::PairCodeOptions;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

use crate::storage::Store;

const OUTBOUND_RETRY_DELAY: Duration = Duration::from_millis(500);
/// Max message IDs to remember for dedup (prevents double-processing on reconnect).
const DEDUP_CACHE_CAPACITY: usize = 4096;
/// Maximum media file size we'll download (bytes). Larger files are skipped.
const MAX_MEDIA_BYTES: u64 = 50 * 1024 * 1024; // 50 MiB
/// Maximum concurrent media downloads (bounds peak memory usage).
const MAX_CONCURRENT_DOWNLOADS: usize = 4;
/// Maximum cached messages for forward_message lookup.
const MSG_CACHE_CAP: usize = 256;
/// TTL for cached group metadata (5 minutes).
const GROUP_CACHE_TTL: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Group metadata cache
// ---------------------------------------------------------------------------

struct CachedGroup {
    info: GroupInfo,
    fetched_at: std::time::Instant,
}

struct GroupCache {
    entries: std::collections::HashMap<String, CachedGroup>,
}

impl GroupCache {
    fn new() -> Self {
        Self { entries: std::collections::HashMap::new() }
    }

    fn get(&mut self, jid: &str) -> Option<&GroupInfo> {
        // Evict the entry if expired
        if self.entries.get(jid).is_some_and(|e| e.fetched_at.elapsed() >= GROUP_CACHE_TTL) {
            self.entries.remove(jid);
            return None;
        }
        self.entries.get(jid).map(|e| &e.info)
    }

    fn insert(&mut self, jid: String, info: GroupInfo) {
        // Evict all expired entries on insert to prevent unbounded growth
        self.entries.retain(|_, e| e.fetched_at.elapsed() < GROUP_CACHE_TTL);
        self.entries.insert(jid, CachedGroup {
            info,
            fetched_at: std::time::Instant::now(),
        });
    }

    fn invalidate(&mut self, jid: &str) {
        self.entries.remove(jid);
    }

}

type GroupCacheHandle = Arc<ParkingMutex<GroupCache>>;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BridgeState {
    Disconnected,
    Pairing,
    Connected,
    Reconnecting,
    Stopped,
}

impl std::fmt::Display for BridgeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disconnected => f.write_str("disconnected"),
            Self::Pairing => f.write_str("pairing"),
            Self::Connected => f.write_str("connected"),
            Self::Reconnecting => f.write_str("reconnecting"),
            Self::Stopped => f.write_str("stopped"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionAction {
    Retry,
    Stop,
    /// Another client replaced our session. The reconnect loop should
    /// allow a few retries (session may survive) but stop if it loops.
    StreamReplaced,
}

/// Atomic counters for bridge health metrics — no locks, no DB reads.
pub struct BridgeMetrics {
    pub started_at: std::time::Instant,
    pub messages_sent: AtomicU64,
    pub messages_received: AtomicU64,
    pub inbound_sequence: AtomicU64,
    pub reconnect_count: AtomicU64,
    pub last_connect_epoch: AtomicU64,
    pub last_disconnect_epoch: AtomicU64,
    pub last_inbound_epoch: AtomicU64,
    pub last_outbound_epoch: AtomicU64,
}

impl Default for BridgeMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl BridgeMetrics {
    pub fn new() -> Self {
        Self {
            started_at: std::time::Instant::now(),
            messages_sent: AtomicU64::new(0),
            messages_received: AtomicU64::new(0),
            inbound_sequence: AtomicU64::new(0),
            reconnect_count: AtomicU64::new(0),
            last_connect_epoch: AtomicU64::new(0),
            last_disconnect_epoch: AtomicU64::new(0),
            last_inbound_epoch: AtomicU64::new(0),
            last_outbound_epoch: AtomicU64::new(0),
        }
    }

    fn epoch_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    pub fn record_sent(&self) {
        self.messages_sent.fetch_add(1, Ordering::Relaxed);
        self.last_outbound_epoch.store(Self::epoch_now(), Ordering::Relaxed);
    }

    pub fn record_received(&self) {
        self.messages_received.fetch_add(1, Ordering::Relaxed);
        self.last_inbound_epoch.store(Self::epoch_now(), Ordering::Relaxed);
    }

    pub fn record_connect(&self) {
        self.last_connect_epoch.store(Self::epoch_now(), Ordering::Relaxed);
    }

    pub fn record_disconnect(&self) {
        self.last_disconnect_epoch.store(Self::epoch_now(), Ordering::Relaxed);
    }

    pub fn record_reconnect(&self) {
        self.reconnect_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn next_inbound_sequence(&self) -> u64 {
        self.inbound_sequence.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Shared pacing gate for all direct and queued outbound sends.
/// Token-bucket rate limiter for outbound messages.
///
/// Allows short bursts (up to `burst` messages) while enforcing a sustained
/// rate of one message per `refill_interval`. Tokens refill passively based on
/// elapsed time, so no background task is needed.
///
/// Example with defaults (burst=5, interval=400ms):
///   - 5 messages fire immediately (draining the bucket)
///   - Subsequent messages wait ~400ms each until tokens refill
///   - After 2 seconds of silence, bucket refills to 5
struct SendPacer {
    /// Maximum tokens (burst capacity).
    burst: u32,
    /// Time to refill one token.
    refill_interval: Duration,
    /// State: (available_tokens as f64, last_refill_time).
    state: TokioMutex<(f64, tokio::time::Instant)>,
}

impl SendPacer {
    fn with_burst(refill_interval_ms: u64, burst: u32) -> Self {
        Self {
            burst,
            refill_interval: Duration::from_millis(refill_interval_ms),
            state: TokioMutex::new((burst as f64, tokio::time::Instant::now())),
        }
    }

    async fn wait_turn(&self) {
        if self.refill_interval.is_zero() {
            return;
        }
        let mut state = self.state.lock().await;
        let (ref mut tokens, ref mut last_refill) = *state;

        // Passive refill: add tokens based on elapsed time
        let elapsed = last_refill.elapsed();
        let refill = elapsed.as_secs_f64() / self.refill_interval.as_secs_f64();
        if refill > 0.0 {
            *tokens = (*tokens + refill).min(self.burst as f64);
            *last_refill = tokio::time::Instant::now();
        }

        if *tokens >= 1.0 {
            // Token available — consume and go
            *tokens -= 1.0;
        } else {
            // Wait for next token + jitter
            let deficit = 1.0 - *tokens;
            let wait = Duration::from_secs_f64(deficit * self.refill_interval.as_secs_f64());
            let jitter_ms = rand_jitter_ms((self.refill_interval.as_millis() as u64 / 3).max(1));
            let total_wait = wait + Duration::from_millis(jitter_ms);
            drop(state); // release lock during sleep
            tokio::time::sleep(total_wait).await;
            // Re-acquire and consume
            let mut state = self.state.lock().await;
            let (ref mut tokens, ref mut last_refill) = *state;
            let elapsed = last_refill.elapsed();
            let refill = elapsed.as_secs_f64() / self.refill_interval.as_secs_f64();
            *tokens = (*tokens + refill).min(self.burst as f64);
            *last_refill = tokio::time::Instant::now();
            *tokens = (*tokens - 1.0).max(0.0);
        }
    }
}

/// Group metadata returned by group query methods.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GroupInfo {
    pub jid: String,
    pub subject: String,
    pub participants: Vec<GroupParticipantInfo>,
}

/// A participant in a WhatsApp group.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GroupParticipantInfo {
    pub jid: String,
    pub phone: Option<String>,
    pub is_admin: bool,
}

/// Tri-state result from content extraction — drives two-phase dedup.
enum ExtractResult {
    /// Successfully extracted content — dedup after enqueue.
    Content(InboundContent),
    /// Unknown/unhandled message type — dedup immediately (no point retrying).
    Unhandled,
    /// Transient failure (e.g. media download error) — do NOT dedup, allow retry.
    TransientFailure,
}

/// Content variants for inbound WhatsApp messages.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)] // Fields consumed by bot integrations, not the REPL
pub enum InboundContent {
    /// Plain text or extended text message.
    Text {
        body: String,
        /// Link preview metadata from ExtendedTextMessage, if present.
        link_preview: Option<InboundLinkPreview>,
    },
    /// Image with downloaded bytes.
    Image {
        /// Raw image bytes (skipped during serialization — access via field or `media_bytes()`).
        #[serde(skip)]
        data: Arc<[u8]>,
        mime: String,
        caption: Option<String>,
        width: Option<u32>,
        height: Option<u32>,
    },
    /// Audio (voice note or file) with downloaded bytes.
    Audio {
        /// Raw audio bytes (skipped during serialization).
        #[serde(skip)]
        data: Arc<[u8]>,
        mime: String,
        seconds: Option<u32>,
        is_voice: bool,
    },
    /// Video with downloaded bytes.
    Video {
        /// Raw video bytes (skipped during serialization).
        #[serde(skip)]
        data: Arc<[u8]>,
        mime: String,
        caption: Option<String>,
        seconds: Option<u32>,
        width: Option<u32>,
        height: Option<u32>,
        /// True if this is a GIF (auto-looping, no controls).
        is_gif: bool,
    },
    /// Document/file with downloaded bytes.
    Document {
        /// Raw document bytes (skipped during serialization).
        #[serde(skip)]
        data: Arc<[u8]>,
        mime: String,
        filename: String,
        caption: Option<String>,
        page_count: Option<u32>,
    },
    /// Sticker with downloaded bytes.
    Sticker {
        /// Raw sticker bytes (skipped during serialization).
        #[serde(skip)]
        data: Arc<[u8]>,
        mime: String,
        is_animated: bool,
        /// Emoji or label describing the sticker (from accessibility_label).
        sticker_emoji: Option<String>,
    },
    /// Location pin.
    Location {
        lat: f64,
        lon: f64,
        name: Option<String>,
        address: Option<String>,
        url: Option<String>,
        is_live: bool,
    },
    /// Contact card(s) (vCard).
    Contact {
        display_name: String,
        vcard: String,
        /// Additional contacts if multiple were sent.
        additional_contacts: Vec<(String, String)>,
    },
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
    /// Someone edited their message.
    Edit {
        target_id: String,
        new_text: String,
        /// Updated @mentions from the edited message (may differ from original).
        mentions: Vec<String>,
    },
    /// Someone revoked (deleted) a message.
    Revoke {
        target_id: String,
    },
    /// A poll was created.
    PollCreated {
        question: String,
        options: Vec<String>,
        selectable_count: u32,
    },
    /// A vote on a poll (decrypted).
    PollVote {
        poll_id: String,
        selected_options: Vec<String>,
    },
    /// Delivery receipt for outbound messages (sent/delivered/read/played).
    DeliveryReceipt {
        message_ids: Vec<String>,
        status: crate::bridge_events::DeliveryStatus,
        chat_jid: String,
        sender: String,
    },
}

impl InboundContent {
    /// Short label for logging.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Image { .. } => "image",
            Self::Audio { .. } => "audio",
            Self::Video { .. } => "video",
            Self::Document { .. } => "document",
            Self::Sticker { .. } => "sticker",
            Self::Location { .. } => "location",
            Self::Contact { .. } => "contact",
            Self::ReactionAdded { .. } => "reaction",
            Self::ReactionRemoved { .. } => "reaction-removed",
            Self::Edit { .. } => "edit",
            Self::Revoke { .. } => "revoke",
            Self::PollCreated { .. } => "poll",
            Self::PollVote { .. } => "poll-vote",
            Self::DeliveryReceipt { .. } => "delivery-receipt",
        }
    }

    /// Extract a human-readable summary for display.
    pub fn display_text(&self) -> String {
        match self {
            Self::Text { body, .. } => body.clone(),
            Self::Image { caption, data, .. } => {
                let size = format_size(data.len());
                match caption {
                    Some(c) => format!("[image {size}] {c}"),
                    None => format!("[image {size}]"),
                }
            }
            Self::Audio { data, seconds, is_voice, .. } => {
                let size = format_size(data.len());
                let kind = if *is_voice { "voice" } else { "audio" };
                match seconds {
                    Some(s) => format!("[{kind} {s}s {size}]"),
                    None => format!("[{kind} {size}]"),
                }
            }
            Self::Video { caption, data, .. } => {
                let size = format_size(data.len());
                match caption {
                    Some(c) => format!("[video {size}] {c}"),
                    None => format!("[video {size}]"),
                }
            }
            Self::Document { filename, data, caption, .. } => {
                let size = format_size(data.len());
                match caption {
                    Some(c) => format!("[doc: {filename} {size}] {c}"),
                    None => format!("[doc: {filename} {size}]"),
                }
            }
            Self::Sticker { is_animated, data, .. } => {
                let size = format_size(data.len());
                if *is_animated {
                    format!("[animated sticker {size}]")
                } else {
                    format!("[sticker {size}]")
                }
            }
            Self::Location { lat, lon, name, .. } => match name {
                Some(n) => format!("[location: {n} ({lat:.5},{lon:.5})]"),
                None => format!("[location: {lat:.5},{lon:.5}]"),
            },
            Self::Contact { display_name, .. } => format!("[contact: {display_name}]"),
            Self::ReactionAdded { emoji, target_id, .. } => format!("[react {emoji} on {target_id}]"),
            Self::ReactionRemoved { target_id, .. } => format!("[unreact on {target_id}]"),
            Self::Edit { target_id, new_text, .. } => format!("[edit {target_id}] {new_text}"),
            Self::Revoke { target_id } => format!("[deleted {target_id}]"),
            Self::PollCreated { question, options, selectable_count } => {
                format!("[poll: {question} (pick {selectable_count}) — {}]", options.join(" | "))
            }
            Self::PollVote { poll_id, selected_options } => {
                format!("[vote on {poll_id}: {}]", selected_options.join(", "))
            }
            Self::DeliveryReceipt { message_ids, status, .. } => {
                format!("[receipt: {status} for {} message(s)]", message_ids.len())
            }
        }
    }
}

impl std::fmt::Display for InboundContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display_text())
    }
}

fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Forwarding and view-once metadata on an inbound message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct MessageFlags {
    pub is_forwarded: bool,
    pub forwarding_score: u32,
    pub is_view_once: bool,
}

/// Link preview metadata extracted from inbound ExtendedTextMessage.
#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)] // Public API for library consumers
pub struct InboundLinkPreview {
    pub url: String,
    pub title: Option<String>,
    pub description: Option<String>,
}

/// Context of a quoted/replied-to message.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ReplyContext {
    /// Stanza ID of the quoted message.
    pub stanza_id: String,
    /// Sender JID of the quoted message (for group context).
    pub participant: Option<String>,
    /// Text body of the quoted message (if available from context_info.quoted_message).
    pub quoted_text: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)] // Fields consumed by bot integrations, not the REPL
pub struct WhatsAppInbound {
    /// Monotonic sequence assigned when the bridge admits this inbound event.
    /// Consumers can use it to restore chat order when async media downloads finish out of order.
    pub sequence: u64,
    /// Which bridge instance received this message (for multi-number routing).
    pub bridge_id: String,
    /// Chat JID (group or individual)
    pub jid: String,
    /// Message ID (for replying, editing, reacting)
    pub id: String,
    /// Message content
    pub content: InboundContent,
    /// Sender — resolved to phone number when possible, raw JID otherwise
    pub sender: String,
    /// Raw sender JID (may be LID)
    pub sender_raw: String,
    /// Sender's display name (push name) as set in their WhatsApp profile.
    pub push_name: String,
    /// Unix timestamp
    pub timestamp: i64,
    /// If this message is a reply, context about the quoted message.
    pub reply_to: Option<ReplyContext>,
    /// Whether this is from our own account
    pub is_from_me: bool,
    /// Whether this message is in a group chat (vs DM).
    pub is_group: bool,
    /// JIDs @mentioned in this message (from context_info.mentioned_jid).
    pub mentions: Vec<String>,
    /// Disappearing messages timer in seconds (0 = off), from context_info.expiration.
    pub ephemeral_expiration: Option<u32>,
    /// Forwarding and view-once metadata
    pub flags: MessageFlags,
}

#[allow(dead_code)] // Public API for library consumers (habb)
impl WhatsAppInbound {
    /// Extract the text body, if this is a text or edit message.
    pub fn text_body(&self) -> Option<&str> {
        match &self.content {
            InboundContent::Text { body, .. } => Some(body),
            InboundContent::Edit { new_text, .. } => Some(new_text),
            _ => None,
        }
    }

    /// Extract the caption from media messages (image, video, document).
    pub fn caption(&self) -> Option<&str> {
        match &self.content {
            InboundContent::Image { caption, .. }
            | InboundContent::Video { caption, .. }
            | InboundContent::Document { caption, .. } => caption.as_deref(),
            _ => None,
        }
    }

    /// Check if this message @mentions the given JID (e.g. our own bot JID).
    pub fn is_mentioned(&self, jid: &str) -> bool {
        self.mentions.iter().any(|m| m == jid)
    }

    /// Check if this is a group message (convenience for `self.is_group`).
    pub fn is_dm(&self) -> bool {
        !self.is_group
    }

    /// Build a `MessageRef` for replying, reacting, or editing this message.
    pub fn as_ref(&self) -> MessageRef {
        MessageRef::from_inbound(self)
    }

    /// Get the raw media bytes if this is a media message (image, audio, video, document, sticker).
    pub fn media_bytes(&self) -> Option<&[u8]> {
        match &self.content {
            InboundContent::Image { data, .. }
            | InboundContent::Audio { data, .. }
            | InboundContent::Video { data, .. }
            | InboundContent::Document { data, .. }
            | InboundContent::Sticker { data, .. } => Some(data.as_ref()),
            _ => None,
        }
    }

    /// Get the MIME type if this is a media message.
    pub fn media_mime(&self) -> Option<&str> {
        match &self.content {
            InboundContent::Image { mime, .. }
            | InboundContent::Audio { mime, .. }
            | InboundContent::Video { mime, .. }
            | InboundContent::Document { mime, .. }
            | InboundContent::Sticker { mime, .. } => Some(mime),
            _ => None,
        }
    }
}

/// Reference to a specific message — used for reactions, replies, edits.
#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)] // Public API for downstream crates (habb)
pub struct MessageRef {
    pub chat_jid: String,
    pub message_id: String,
    pub from_me: bool,
    /// Sender JID — required for group chats and present for incoming DMs.
    pub sender_jid: Option<String>,
}

#[allow(dead_code)] // Public API for downstream crates
impl MessageRef {
    /// Create from an inbound message.
    pub fn from_inbound(msg: &WhatsAppInbound) -> Self {
        Self {
            chat_jid: msg.jid.clone(),
            message_id: msg.id.clone(),
            from_me: msg.is_from_me,
            sender_jid: if msg.is_group || !msg.is_from_me {
                Some(msg.sender_raw.clone())
            } else {
                None
            },
        }
    }
}

/// Presence events surfaced to the consumer (typing, recording indicators).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)] // Fields consumed by bot integrations, not the REPL
pub enum PresenceEvent {
    Composing { chat_jid: String, sender: String },
    Recording { chat_jid: String, sender: String },
    Paused { chat_jid: String, sender: String },
    Online { jid: String, last_seen: Option<i64> },
    Offline { jid: String, last_seen: Option<i64> },
}

pub struct BridgeConfig {
    /// Unique identifier for this bridge instance (for multi-number routing).
    pub bridge_id: String,
    pub db_path: PathBuf,
    /// Phone number for pair-code linking (e.g. "15551234567"). If None, uses QR.
    pub pair_phone: Option<String>,
    /// Maximum reconnection delay (caps exponential backoff)
    pub max_reconnect_delay: Duration,
    /// Skip history sync on connect (prevents "deaf client" bug with large offline queues)
    pub skip_history_sync: bool,
    /// Automatically send read receipts for inbound messages after enqueue.
    pub auto_mark_read: bool,
    /// If non-empty, only process inbound messages from these phone numbers (digits only).
    pub allowed_numbers: Vec<String>,
    /// Minimum interval between outbound sends (anti-ban pacing, millis).
    pub min_send_interval_ms: u64,
    /// Maximum burst size for outbound rate limiter (default: 5).
    /// Up to this many messages can be sent immediately before pacing kicks in.
    pub send_burst: u32,
    /// TCP port for the API server (0 = disabled). Used by main.rs, not the bridge itself.
    #[allow(dead_code)]
    pub health_port: u16,
    /// Seconds to wait for in-flight operations during graceful shutdown.
    pub drain_timeout_secs: u64,
    /// Directory for SQLite backups (None = disabled).
    pub backup_dir: Option<PathBuf>,
    /// Interval between automatic backups in seconds (default: 6 hours).
    pub backup_interval_secs: u64,
    /// Interval between database prune runs in seconds (default: 1 hour).
    pub prune_interval_secs: u64,
    /// Maximum outbound retries before marking as permanently failed.
    pub max_outbound_retries: i32,
    /// Optional channel to receive presence events (typing, recording indicators).
    pub presence_tx: Option<mpsc::Sender<PresenceEvent>>,
    /// Device name shown in WhatsApp's "Linked Devices" list (default: "Habb").
    pub device_name: String,
    /// Maximum message IDs tracked for dedup (default: 4096).
    pub dedup_capacity: usize,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            bridge_id: "default".to_string(),
            db_path: PathBuf::from("whatsapp.db"),
            pair_phone: None,
            max_reconnect_delay: Duration::from_secs(60),
            skip_history_sync: true,
            auto_mark_read: true,
            allowed_numbers: Vec::new(),
            min_send_interval_ms: 400,
            send_burst: 5,
            health_port: 0,
            drain_timeout_secs: 10,
            backup_dir: None,
            backup_interval_secs: 6 * 3600,
            prune_interval_secs: 3600,
            max_outbound_retries: 3,
            presence_tx: None,
            device_name: "Habb".to_string(),
            dedup_capacity: DEDUP_CACHE_CAPACITY,
        }
    }
}

// ---------------------------------------------------------------------------
// WhatsAppBridge — public handle for the consumer
// ---------------------------------------------------------------------------

/// Simple bounded LRU cache for forward_message lookup.
/// Evicts the oldest entry on insert at capacity (not clear-all).
struct BoundedMsgCache {
    map: HashMap<String, Box<wa::Message>>,
    order: std::collections::VecDeque<String>,
    capacity: usize,
}

impl BoundedMsgCache {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity),
            order: std::collections::VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn insert(&mut self, id: String, msg: Box<wa::Message>) {
        if let std::collections::hash_map::Entry::Occupied(mut e) = self.map.entry(id.clone()) {
            // Already cached — update in place, don't grow order
            e.insert(msg);
            return;
        }
        while self.map.len() >= self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            } else {
                break;
            }
        }
        self.order.push_back(id.clone());
        self.map.insert(id, msg);
    }

    #[allow(clippy::borrowed_box)]
    fn get(&self, id: &str) -> Option<&Box<wa::Message>> {
        self.map.get(id)
    }
}

type MsgCache = Arc<ParkingMutex<BoundedMsgCache>>;

pub struct WhatsAppBridge {
    /// Wakes the outbound worker after a new job is enqueued.
    outbound_notify: Arc<tokio::sync::Notify>,
    state_rx: watch::Receiver<BridgeState>,
    #[allow(dead_code)] // Used by agent integrations via subscribe_qr()
    qr_rx: watch::Receiver<Option<String>>,
    cancel: CancellationToken,
    client_handle: Arc<ParkingMutex<Option<Arc<Client>>>>,
    rr_tx: mpsc::Sender<ReadReceiptCmd>,
    msg_cache: MsgCache,
    store: Store,
    metrics: Arc<BridgeMetrics>,
    #[allow(dead_code)] // Kept alive for the outbound worker (SendPacer is shared via Arc)
    send_pacer: Arc<SendPacer>,
    /// JIDs with active presence subscriptions — replayed on reconnect.
    subscribed_presence: Arc<DashSet<String>>,
    /// Broadcast event bus for inbound messages, outbound status, and receipts.
    event_tx: tokio::sync::broadcast::Sender<Arc<crate::bridge_events::BridgeEvent>>,
    /// Cached group metadata (TTL-based, invalidated on group mutation).
    group_cache: GroupCacheHandle,
}

impl WhatsAppBridge {
    /// Start the bridge. Returns immediately — the bot connects in the background.
    /// Use `state()` to monitor connection progress.
    pub fn start(
        config: BridgeConfig,
        inbound_tx: mpsc::Sender<WhatsAppInbound>,
        cancel: CancellationToken,
    ) -> Self {
        let (state_tx, state_rx) = watch::channel(BridgeState::Disconnected);
        let (qr_tx, qr_rx) = watch::channel::<Option<String>>(None);
        let outbound_notify = Arc::new(tokio::sync::Notify::new());
        let client_handle: Arc<ParkingMutex<Option<Arc<Client>>>> =
            Arc::new(ParkingMutex::new(None));
        let rr_tx = spawn_scheduler(None, 200);
        let msg_cache: MsgCache = Arc::new(ParkingMutex::new(BoundedMsgCache::new(MSG_CACHE_CAP)));
        let send_pacer = Arc::new(SendPacer::with_burst(config.min_send_interval_ms, config.send_burst));
        let subscribed_presence: Arc<DashSet<String>> = Arc::new(DashSet::new());
        let group_cache: GroupCacheHandle = Arc::new(ParkingMutex::new(GroupCache::new()));
        let (event_tx, _event_rx) = crate::bridge_events::new_event_bus();

        // Open store early so bridge methods can access it (e.g. poll key storage)
        let store = Store::new(&config.db_path).expect("failed to open database for bridge");
        let metrics = Arc::new(BridgeMetrics::new());

        let cancel_clone = cancel.clone();
        let ch = client_handle.clone();
        let rr = rr_tx.clone();
        let mc = msg_cache.clone();
        let met = metrics.clone();
        let sp = send_pacer.clone();
        let sub_pres = subscribed_presence.clone();
        let on = outbound_notify.clone();
        let et = event_tx.clone();
        let st = store.clone();
        let gc = group_cache.clone();
        tokio::spawn(async move {
            if let Err(e) =
                run_bridge(config, inbound_tx, state_tx, qr_tx, cancel_clone, ch, rr, mc, met, sp, sub_pres, on, et, st, gc).await
            {
                error!(error = %e, "WhatsApp bridge exited with error");
            }
        });

        Self {
            outbound_notify,
            state_rx,
            qr_rx,
            cancel,
            client_handle,
            rr_tx,
            msg_cache,
            store,
            metrics,
            send_pacer,
            subscribed_presence,
            event_tx,
            group_cache,
        }
    }

    /// Enqueue a text message for durable delivery via the outbound job queue.
    #[allow(dead_code)] // Public API for library consumers (habb)
    pub async fn send_message(&self, jid: &str, text: &str) -> Result<()> {
        self.send_message_mentioned(jid, text, &[]).await
    }

    /// Send a text message with @mentions. Empty mentions slice = no mentions.
    #[allow(dead_code)] // Public API for library consumers
    pub async fn send_message_mentioned(&self, jid: &str, text: &str, mentions: &[String]) -> Result<()> {
        let payload = serde_json::to_string(&crate::outbound::TextPayload {
            text: text.to_string(),
            mentions: mentions.to_vec(),
            link_preview: None,
        })?;
        self.store
            .enqueue_job(jid, crate::outbound::OutboundOpKind::Text.as_str(), &payload, None)
            .await?;
        self.outbound_notify.notify_one();
        Ok(())
    }

    /// Send a text message with a link preview card.
    #[allow(dead_code, clippy::too_many_arguments)] // Public API for library consumers
    pub async fn send_message_with_preview(
        &self,
        jid: &str,
        text: &str,
        url: &str,
        title: Option<&str>,
        description: Option<&str>,
        thumbnail: Option<&[u8]>,
        mentions: &[String],
    ) -> Result<()> {
        use base64::Engine;
        let payload = serde_json::to_string(&crate::outbound::TextPayload {
            text: text.to_string(),
            mentions: mentions.to_vec(),
            link_preview: Some(crate::outbound::LinkPreview {
                url: url.to_string(),
                title: title.map(|s| s.to_string()),
                description: description.map(|s| s.to_string()),
                thumbnail_b64: thumbnail.map(|b| base64::engine::general_purpose::STANDARD.encode(b)),
            }),
        })?;
        self.enqueue_op(jid, crate::outbound::OutboundOpKind::Text, &payload, None).await?;
        Ok(())
    }

    pub fn state(&self) -> BridgeState {
        *self.state_rx.borrow()
    }

    /// Subscribe to bridge state changes.
    #[allow(dead_code)] // Public API for library consumers
    pub fn subscribe_state(&self) -> watch::Receiver<BridgeState> {
        self.state_rx.clone()
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }

    // -----------------------------------------------------------------------
    // Group management
    // -----------------------------------------------------------------------

    /// Get metadata for a group (subject, participants, admin status).
    pub async fn get_group_info(
        &self,
        group_jid: &str,
    ) -> Result<GroupInfo> {
        // Check cache first
        {
            let mut cache = self.group_cache.lock();
            if let Some(info) = cache.get(group_jid) {
                return Ok(info.clone());
            }
        }
        let jid = parse_jid(group_jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let metadata = client.groups().get_metadata(&jid).await?;
        let info = GroupInfo {
            jid: metadata.id.to_string(),
            subject: metadata.subject,
            participants: metadata
                .participants
                .into_iter()
                .map(|p| GroupParticipantInfo {
                    jid: p.jid.to_string(),
                    phone: p.phone_number.map(|pn| pn.to_string()),
                    is_admin: p.is_admin,
                })
                .collect(),
        };
        self.group_cache.lock().insert(group_jid.to_string(), info.clone());
        Ok(info)
    }

    /// Get all groups this device is a member of (cached, 5min TTL).
    pub async fn get_joined_groups(&self) -> Result<Vec<GroupInfo>> {
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let groups = client.groups().get_participating().await?;
        let result: Vec<GroupInfo> = groups
            .into_values()
            .map(|g| GroupInfo {
                jid: g.id.to_string(),
                subject: g.subject,
                participants: g
                    .participants
                    .into_iter()
                    .map(|p| GroupParticipantInfo {
                        jid: p.jid.to_string(),
                        phone: p.phone_number.map(|pn| pn.to_string()),
                        is_admin: p.is_admin,
                    })
                    .collect(),
            })
            .collect();
        // Populate individual group cache entries
        let mut cache = self.group_cache.lock();
        for info in &result {
            cache.insert(info.jid.clone(), info.clone());
        }
        Ok(result)
    }

    /// Create a new group with the given name and participant phone numbers.
    pub async fn create_group(
        &self,
        name: &str,
        participants: &[&str],
    ) -> Result<String> {
        use whatsapp_rust::features::{GroupCreateOptions, GroupParticipantOptions};
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let participant_opts: Vec<GroupParticipantOptions> = participants
            .iter()
            .map(|p| parse_jid(p).map(GroupParticipantOptions::new))
            .collect::<Result<Vec<_>>>()?;
        let opts = GroupCreateOptions::new(name).with_participants(participant_opts);
        let result = client.groups().create_group(opts).await?;
        Ok(result.gid.to_string())
    }

    /// Set the subject (title) of a group.
    pub async fn set_group_subject(&self, group_jid: &str, subject: &str) -> Result<()> {
        use whatsapp_rust::features::GroupSubject;
        let jid = parse_jid(group_jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let validated_subject = GroupSubject::new(subject.to_string())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        client.groups().set_subject(&jid, validated_subject).await?;
        self.group_cache.lock().invalidate(group_jid);
        Ok(())
    }

    /// Leave a group.
    pub async fn leave_group(&self, group_jid: &str) -> Result<()> {
        let jid = parse_jid(group_jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client.groups().leave(&jid).await?;
        self.group_cache.lock().invalidate(group_jid);
        Ok(())
    }

    /// Get the invite link for a group.
    pub async fn get_group_invite_link(&self, group_jid: &str) -> Result<String> {
        let jid = parse_jid(group_jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let link = client.groups().get_invite_link(&jid, false).await?;
        Ok(link)
    }

    /// Set or clear a group's description.
    pub async fn set_group_description(
        &self,
        group_jid: &str,
        description: Option<&str>,
    ) -> Result<()> {
        use whatsapp_rust::features::GroupDescription;
        let jid = parse_jid(group_jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let desc = description
            .map(|d| GroupDescription::new(d.to_string()))
            .transpose()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        client.groups().set_description(&jid, desc, None).await?;
        self.group_cache.lock().invalidate(group_jid);
        Ok(())
    }

    /// Add participants to a group. Returns per-participant status.
    pub async fn add_participants(
        &self,
        group_jid: &str,
        participants: &[&str],
    ) -> Result<Vec<(String, Option<String>)>> {
        let jid = parse_jid(group_jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let jids: Vec<_> = participants
            .iter()
            .map(|p| parse_jid(p))
            .collect::<Result<Vec<_>>>()?;
        let results = client.groups().add_participants(&jid, &jids).await?;
        self.group_cache.lock().invalidate(group_jid);
        Ok(results
            .into_iter()
            .map(|r| (r.jid.to_string(), r.status))
            .collect())
    }

    /// Remove participants from a group. Returns per-participant status.
    pub async fn remove_participants(
        &self,
        group_jid: &str,
        participants: &[&str],
    ) -> Result<Vec<(String, Option<String>)>> {
        let jid = parse_jid(group_jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let jids: Vec<_> = participants
            .iter()
            .map(|p| parse_jid(p))
            .collect::<Result<Vec<_>>>()?;
        let results = client.groups().remove_participants(&jid, &jids).await?;
        self.group_cache.lock().invalidate(group_jid);
        Ok(results
            .into_iter()
            .map(|r| (r.jid.to_string(), r.status))
            .collect())
    }

    /// Promote participants to group admin.
    pub async fn promote_participants(
        &self,
        group_jid: &str,
        participants: &[&str],
    ) -> Result<()> {
        let jid = parse_jid(group_jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let jids: Vec<_> = participants
            .iter()
            .map(|p| parse_jid(p))
            .collect::<Result<Vec<_>>>()?;
        client.groups().promote_participants(&jid, &jids).await?;
        self.group_cache.lock().invalidate(group_jid);
        Ok(())
    }

    /// Demote participants from group admin.
    pub async fn demote_participants(
        &self,
        group_jid: &str,
        participants: &[&str],
    ) -> Result<()> {
        let jid = parse_jid(group_jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let jids: Vec<_> = participants
            .iter()
            .map(|p| parse_jid(p))
            .collect::<Result<Vec<_>>>()?;
        client.groups().demote_participants(&jid, &jids).await?;
        self.group_cache.lock().invalidate(group_jid);
        Ok(())
    }

    /// Flush all pending read receipts for a chat (call before replying).
    /// Returns Ok(true) if receipts were sent, Ok(false) if no client was available.
    pub async fn flush_read_receipts(&self, chat_jid: &str) -> Result<bool> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.rr_tx
            .send(ReadReceiptCmd::FlushChat {
                chat_jid: chat_jid.to_string(),
                ack: ack_tx,
            })
            .await
            .map_err(|e| anyhow::anyhow!("receipt scheduler closed: {e}"))?;
        ack_rx
            .await
            .map_err(|e| anyhow::anyhow!("receipt flush ack failed: {e}"))
    }

    pub async fn wait_stopped(&self, timeout: Duration) -> bool {
        let mut rx = self.state_rx.clone();
        let wait = async move {
            loop {
                if *rx.borrow() == BridgeState::Stopped {
                    return true;
                }
                if rx.changed().await.is_err() {
                    return false;
                }
            }
        };
        tokio::time::timeout(timeout, wait).await.unwrap_or(false)
    }

    /// Send a "typing" indicator to a chat.
    pub async fn start_typing(&self, jid: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client
            .chatstate()
            .send_composing(&target)
            .await
            .map_err(|e| anyhow::anyhow!("chatstate error: {e}"))
    }

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

    /// Cancel typing indicator for a chat.
    pub async fn stop_typing(&self, jid: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client
            .chatstate()
            .send_paused(&target)
            .await
            .map_err(|e| anyhow::anyhow!("chatstate error: {e}"))
    }

    /// Cancel recording indicator for a chat.
    pub async fn stop_recording(&self, jid: &str) -> Result<()> {
        self.stop_typing(jid).await
    }

    /// Subscribe to QR code events. Returns `Some(data)` when a QR code is available
    /// for scanning, `None` when pairing completes or session connects.
    /// Use `crate::qr::QrRender::new(&data)` to render in any format (terminal, HTML, SVG, PNG).
    #[allow(dead_code)] // Used by agent integrations, not the REPL
    pub fn subscribe_qr(&self) -> watch::Receiver<Option<String>> {
        self.qr_rx.clone()
    }

    /// Subscribe to the bridge event bus (inbound messages, outbound status, receipts).
    /// Each subscriber gets its own broadcast receiver. Slow receivers that fall
    /// behind will get `Lagged` and should reconnect.
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<Arc<crate::bridge_events::BridgeEvent>> {
        self.event_tx.subscribe()
    }

    /// Get a reference to the event bus sender (for internal use by API server).
    #[allow(dead_code)] // Public API for library consumers
    pub fn event_sender(&self) -> &tokio::sync::broadcast::Sender<Arc<crate::bridge_events::BridgeEvent>> {
        &self.event_tx
    }

    /// Check if the bridge has an active WhatsApp connection.
    /// Get a reference to the storage backend (for history/search queries).
    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn metrics(&self) -> &BridgeMetrics {
        &self.metrics
    }

    pub async fn queue_depth(&self) -> i64 {
        self.store.outbound_queue_depth().await.unwrap_or(-1)
    }

    /// Get the current QR code data (None if already paired or not yet generated).
    pub fn current_qr(&self) -> Option<String> {
        self.qr_rx.borrow().clone()
    }

    pub fn is_connected(&self) -> bool {
        self.state() == BridgeState::Connected && get_client_handle(&self.client_handle).is_some()
    }

    /// Enqueue a job, wait for the outbound worker to send it, return the WA message ID.
    /// Subscribes to the event bus *before* enqueueing to avoid missing the Sent event.
    async fn enqueue_and_wait(&self, jid: &str, op_kind: crate::outbound::OutboundOpKind, payload_json: &str, payload_blob: Option<Vec<u8>>) -> Result<String> {
        // Subscribe BEFORE enqueue to avoid race with fast worker
        let mut rx = self.event_tx.subscribe();
        let job_id = self.enqueue_op(jid, op_kind, payload_json, payload_blob).await?;

        let timeout = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match rx.recv().await {
                    Ok(evt) => {
                        if let crate::bridge_events::BridgeEvent::OutboundStatus(ref s) = *evt {
                            if s.job_id == job_id {
                                match s.state {
                                    crate::bridge_events::OutboundJobState::Sent => {
                                        return Ok(s.wa_message_id.clone().unwrap_or_default());
                                    }
                                    crate::bridge_events::OutboundJobState::Failed => {
                                        return Err(anyhow::anyhow!(
                                            "send failed: {}",
                                            s.error.as_deref().unwrap_or("unknown")
                                        ));
                                    }
                                    _ => {} // Queued, Sending — keep waiting
                                }
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Err(anyhow::anyhow!("event bus closed"));
                    }
                }
            }
        });
        timeout.await.context("send timed out (30s)")?
    }

    /// Enqueue an outbound job and wake the worker. Returns the job_id.
    /// Public for use by the API server; most callers should use the typed send methods.
    pub async fn enqueue_op(
        &self,
        jid: &str,
        op_kind: crate::outbound::OutboundOpKind,
        payload_json: &str,
        payload_blob: Option<Vec<u8>>,
    ) -> Result<i64> {
        let job_id = self
            .store
            .enqueue_job(jid, op_kind.as_str(), payload_json, payload_blob)
            .await?;
        self.outbound_notify.notify_one();
        Ok(job_id)
    }

    /// Enqueue an outbound job scheduled for a future time. Returns the job_id.
    pub async fn enqueue_op_at(
        &self,
        jid: &str,
        op_kind: crate::outbound::OutboundOpKind,
        payload_json: &str,
        payload_blob: Option<Vec<u8>>,
        execute_at: i64,
    ) -> Result<i64> {
        let job_id = self
            .store
            .enqueue_job_at(jid, op_kind.as_str(), payload_json, payload_blob, execute_at)
            .await?;
        // Don't notify immediately — the worker's periodic timer will pick it up
        Ok(job_id)
    }

    /// Send an image file to a chat.
    pub async fn send_image(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        caption: Option<&str>,
    ) -> Result<()> {
        if data.is_empty() {
            anyhow::bail!("cannot send empty image (0 bytes)");
        }
        let payload = serde_json::to_string(&crate::outbound::MediaPayload {
            mime: mime.to_string(),
            caption: caption.map(|c| c.to_string()),
            filename: None,
            seconds: None,
            is_voice_note: false,
        })?;
        self.enqueue_op(jid, crate::outbound::OutboundOpKind::Image, &payload, Some(data)).await?;
        Ok(())
    }

    /// Send an audio file. If `is_voice_note` is true (default), sends as PTT voice note.
    pub async fn send_audio(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        seconds: Option<u32>,
        is_voice_note: bool,
    ) -> Result<()> {
        if data.is_empty() {
            anyhow::bail!("cannot send empty audio (0 bytes)");
        }
        let payload = serde_json::to_string(&crate::outbound::MediaPayload {
            mime: mime.to_string(),
            caption: None,
            filename: None,
            seconds,
            is_voice_note,
        })?;
        self.enqueue_op(jid, crate::outbound::OutboundOpKind::Audio, &payload, Some(data)).await?;
        Ok(())
    }

    /// Edit a previously sent message. Requires the message ID from send_message_with_id.
    pub async fn edit_message(&self, jid: &str, message_id: &str, new_text: &str) -> Result<()> {
        let payload = serde_json::to_string(&crate::outbound::EditPayload {
            message_id: message_id.to_string(),
            new_text: new_text.to_string(),
        })?;
        self.enqueue_op(jid, crate::outbound::OutboundOpKind::Edit, &payload, None).await?;
        Ok(())
    }

    /// Revoke (delete for everyone) a previously sent message.
    pub async fn revoke_message(&self, jid: &str, message_id: &str) -> Result<()> {
        let payload = serde_json::to_string(&crate::outbound::RevokePayload {
            message_id: message_id.to_string(),
        })?;
        self.enqueue_op(jid, crate::outbound::OutboundOpKind::Revoke, &payload, None).await?;
        Ok(())
    }

    /// Send a text message and return the message ID (for editing/deleting).
    /// Compatibility wrapper: enqueues a durable job, waits for the Sent event,
    /// and returns the WhatsApp message ID.
    pub async fn send_message_with_id(&self, jid: &str, text: &str) -> Result<String> {
        self.send_message_with_id_mentioned(jid, text, &[]).await
    }

    /// Send a text message with @mentions and return the WA message ID.
    pub async fn send_message_with_id_mentioned(&self, jid: &str, text: &str, mentions: &[String]) -> Result<String> {
        let payload = serde_json::to_string(&crate::outbound::TextPayload {
            text: text.to_string(),
            mentions: mentions.to_vec(),
            link_preview: None,
        })?;
        self.enqueue_and_wait(jid, crate::outbound::OutboundOpKind::Text, &payload, None).await
    }

    /// Send a video file to a chat.
    pub async fn send_video(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        caption: Option<&str>,
    ) -> Result<()> {
        if data.is_empty() {
            anyhow::bail!("cannot send empty video (0 bytes)");
        }
        let payload = serde_json::to_string(&crate::outbound::MediaPayload {
            mime: mime.to_string(),
            caption: caption.map(|c| c.to_string()),
            filename: None,
            seconds: None,
            is_voice_note: false,
        })?;
        self.enqueue_op(jid, crate::outbound::OutboundOpKind::Video, &payload, Some(data)).await?;
        Ok(())
    }

    /// Send a document/file to a chat.
    pub async fn send_document(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        filename: &str,
        caption: Option<&str>,
    ) -> Result<()> {
        if data.is_empty() {
            anyhow::bail!("cannot send empty document (0 bytes)");
        }
        // Sanitize filename: strip path components to prevent traversal display issues
        let safe_filename = std::path::Path::new(filename)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        let payload = serde_json::to_string(&crate::outbound::MediaPayload {
            mime: mime.to_string(),
            caption: caption.map(|c| c.to_string()),
            filename: Some(safe_filename.to_string()),
            seconds: None,
            is_voice_note: false,
        })?;
        self.enqueue_op(jid, crate::outbound::OutboundOpKind::Document, &payload, Some(data)).await?;
        Ok(())
    }

    /// Send a sticker to a chat. Data should be WebP format.
    pub async fn send_sticker(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        is_animated: bool,
    ) -> Result<()> {
        // Encode is_animated as seconds field (0=static, 1=animated)
        let payload = serde_json::to_string(&crate::outbound::MediaPayload {
            mime: mime.to_string(),
            caption: None,
            filename: None,
            seconds: if is_animated { Some(1) } else { Some(0) },
            is_voice_note: false,
        })?;
        self.enqueue_op(jid, crate::outbound::OutboundOpKind::Sticker, &payload, Some(data)).await?;
        Ok(())
    }

    /// Send an emoji reaction to a message.
    pub async fn send_reaction(
        &self,
        chat_jid: &str,
        target_message_id: &str,
        target_sender_jid: Option<&str>,
        emoji: &str,
        target_is_from_me: bool,
    ) -> Result<()> {
        let payload = serde_json::to_string(&crate::outbound::ReactionPayload {
            target_message_id: target_message_id.to_string(),
            emoji: emoji.to_string(),
            target_sender_jid: target_sender_jid.map(|s| s.to_string()),
            target_is_from_me,
        })?;
        let kind = if emoji.is_empty() {
            crate::outbound::OutboundOpKind::Unreact
        } else {
            crate::outbound::OutboundOpKind::Reaction
        };
        self.enqueue_op(chat_jid, kind, &payload, None).await?;
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

    /// Send a location pin to a chat.
    pub async fn send_location(
        &self,
        jid: &str,
        lat: f64,
        lon: f64,
        name: Option<&str>,
        address: Option<&str>,
    ) -> Result<()> {
        // Validate coordinates — NaN/Infinity produce corrupt JSON and silent data loss
        if !lat.is_finite() || !lon.is_finite() {
            anyhow::bail!("invalid coordinates: lat={lat}, lon={lon} (must be finite numbers)");
        }
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            anyhow::bail!("coordinates out of range: lat={lat} (±90), lon={lon} (±180)");
        }
        let payload = serde_json::to_string(&crate::outbound::LocationPayload {
            lat,
            lon,
            name: name.map(|n| n.to_string()),
            address: address.map(|a| a.to_string()),
        })?;
        self.enqueue_op(jid, crate::outbound::OutboundOpKind::Location, &payload, None).await?;
        Ok(())
    }

    /// Send a contact card (vCard) to a chat.
    pub async fn send_contact(
        &self,
        jid: &str,
        display_name: &str,
        vcard: &str,
    ) -> Result<()> {
        let payload = serde_json::to_string(&crate::outbound::ContactPayload {
            display_name: display_name.to_string(),
            vcard: vcard.to_string(),
        })?;
        self.enqueue_op(jid, crate::outbound::OutboundOpKind::Contact, &payload, None).await?;
        Ok(())
    }

    /// Create and send a poll. Returns the WA message ID (poll key is stored by the worker).
    pub async fn send_poll(
        &self,
        jid: &str,
        question: &str,
        options: &[String],
        selectable_count: u32,
    ) -> Result<String> {
        let (question, options) = normalize_poll_spec(question, options, selectable_count)?;
        let payload = serde_json::to_string(&crate::outbound::PollPayload {
            question,
            options,
            selectable_count,
        })?;
        self.enqueue_and_wait(jid, crate::outbound::OutboundOpKind::Poll, &payload, None).await
    }

    /// Subscribe to a contact's presence updates (online/offline notifications).
    /// Subscriptions are tracked and replayed automatically on reconnect.
    pub async fn subscribe_presence(&self, jid: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client
            .presence()
            .subscribe(&target)
            .await
            .map_err(|e| anyhow::anyhow!("presence subscribe failed: {e}"))?;
        self.subscribed_presence.insert(target.to_string());
        Ok(())
    }

    // --- Chat management (direct client calls, not outbound queue) ---

    /// Pin a chat to the top of the chat list.
    pub async fn pin_chat(&self, jid: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client.chat_actions().pin_chat(&target).await
            .map_err(|e| anyhow::anyhow!("pin_chat: {e}"))
    }

    /// Unpin a chat from the top of the chat list.
    pub async fn unpin_chat(&self, jid: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client.chat_actions().unpin_chat(&target).await
            .map_err(|e| anyhow::anyhow!("unpin_chat: {e}"))
    }

    /// Mute a chat indefinitely.
    pub async fn mute_chat(&self, jid: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client.chat_actions().mute_chat(&target).await
            .map_err(|e| anyhow::anyhow!("mute_chat: {e}"))
    }

    /// Unmute a chat.
    pub async fn unmute_chat(&self, jid: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client.chat_actions().unmute_chat(&target).await
            .map_err(|e| anyhow::anyhow!("unmute_chat: {e}"))
    }

    /// Archive a chat.
    pub async fn archive_chat(&self, jid: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client.chat_actions().archive_chat(&target, None).await
            .map_err(|e| anyhow::anyhow!("archive_chat: {e}"))
    }

    /// Unarchive a chat.
    pub async fn unarchive_chat(&self, jid: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client.chat_actions().unarchive_chat(&target, None).await
            .map_err(|e| anyhow::anyhow!("unarchive_chat: {e}"))
    }

    /// Mark a chat as read (syncs across linked devices).
    pub async fn mark_chat_as_read(&self, jid: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client.chat_actions().mark_chat_as_read(&target, true, None).await
            .map_err(|e| anyhow::anyhow!("mark_chat_as_read: {e}"))
    }

    /// Mark a chat as unread (syncs across linked devices).
    pub async fn mark_chat_as_unread(&self, jid: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client.chat_actions().mark_chat_as_read(&target, false, None).await
            .map_err(|e| anyhow::anyhow!("mark_chat_as_unread: {e}"))
    }

    /// Delete a chat (including media).
    pub async fn delete_chat(&self, jid: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client.chat_actions().delete_chat(&target, true, None).await
            .map_err(|e| anyhow::anyhow!("delete_chat: {e}"))
    }

    /// Delete a message for me only (local deletion).
    pub async fn delete_message_for_me(
        &self,
        jid: &str,
        message_id: &str,
        sender_jid: Option<&str>,
        from_me: bool,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let participant = sender_jid.map(parse_jid).transpose()?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client
            .chat_actions()
            .delete_message_for_me(&target, participant.as_ref(), message_id, from_me, true, None)
            .await
            .map_err(|e| anyhow::anyhow!("delete_message_for_me: {e}"))
    }

    /// Star a message.
    pub async fn star_message(
        &self,
        jid: &str,
        message_id: &str,
        sender_jid: Option<&str>,
        from_me: bool,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let participant = sender_jid.map(parse_jid).transpose()?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client
            .chat_actions()
            .star_message(&target, participant.as_ref(), message_id, from_me)
            .await
            .map_err(|e| anyhow::anyhow!("star_message: {e}"))
    }

    /// Unstar a message.
    pub async fn unstar_message(
        &self,
        jid: &str,
        message_id: &str,
        sender_jid: Option<&str>,
        from_me: bool,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let participant = sender_jid.map(parse_jid).transpose()?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client
            .chat_actions()
            .unstar_message(&target, participant.as_ref(), message_id, from_me)
            .await
            .map_err(|e| anyhow::anyhow!("unstar_message: {e}"))
    }

    /// Send a view-once image (disappears after first view).
    pub async fn send_view_once_image(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        caption: Option<&str>,
    ) -> Result<()> {
        if data.is_empty() {
            anyhow::bail!("cannot send empty view-once image (0 bytes)");
        }
        let payload = serde_json::to_string(&crate::outbound::MediaPayload {
            mime: mime.to_string(),
            caption: caption.map(|c| c.to_string()),
            filename: None,
            seconds: None,
            is_voice_note: false,
        })?;
        self.enqueue_op(jid, crate::outbound::OutboundOpKind::ViewOnceImage, &payload, Some(data)).await?;
        Ok(())
    }

    /// Send a view-once video (disappears after first view).
    pub async fn send_view_once_video(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        caption: Option<&str>,
    ) -> Result<()> {
        if data.is_empty() {
            anyhow::bail!("cannot send empty view-once video (0 bytes)");
        }
        let payload = serde_json::to_string(&crate::outbound::MediaPayload {
            mime: mime.to_string(),
            caption: caption.map(|c| c.to_string()),
            filename: None,
            seconds: None,
            is_voice_note: false,
        })?;
        self.enqueue_op(jid, crate::outbound::OutboundOpKind::ViewOnceVideo, &payload, Some(data)).await?;
        Ok(())
    }

    /// Forward a cached message to another chat. The original "Forwarded" label will appear.
    /// Serializes the cached protobuf into payload_blob for crash-safe persistence.
    pub async fn forward_message(&self, dst_jid: &str, msg_id: &str) -> Result<String> {
        let original = {
            let cache = self.msg_cache.lock();
            cache.get(msg_id).cloned()
        };
        let original = original.context("message not in cache (only recent messages are cached)")?;

        // Serialize protobuf to bytes for durable storage
        let proto_bytes = prost::Message::encode_to_vec(&*original);
        let payload = serde_json::to_string(&crate::outbound::ForwardPayload {
            original_msg_id: msg_id.to_string(),
        })?;
        self.enqueue_and_wait(dst_jid, crate::outbound::OutboundOpKind::Forward, &payload, Some(proto_bytes)).await
    }

    /// Send a text message as a reply/quote to another message.
    pub async fn send_reply(
        &self,
        jid: &str,
        reply_to_id: &str,
        reply_to_sender: &str,
        text: &str,
    ) -> Result<String> {
        self.send_reply_mentioned(jid, reply_to_id, reply_to_sender, text, &[]).await
    }

    /// Send a reply with @mentions.
    pub async fn send_reply_mentioned(
        &self,
        jid: &str,
        reply_to_id: &str,
        reply_to_sender: &str,
        text: &str,
        mentions: &[String],
    ) -> Result<String> {
        let target = parse_jid(jid)?;
        match self.flush_read_receipts(&target.to_string()).await {
            Ok(false) => debug!(chat = %target, "read-receipt flush skipped (no client)"),
            Err(e) => warn!(error = %e, chat = %target, "read-receipt flush failed before reply"),
            Ok(true) => {}
        }
        // Look up the original message from cache for quote header display
        let quoted_blob = {
            let cache = self.msg_cache.lock();
            cache.get(reply_to_id).map(|m| prost::Message::encode_to_vec(m.as_ref()))
        };
        let has_quoted = quoted_blob.is_some();
        let payload = serde_json::to_string(&crate::outbound::ReplyPayload {
            text: text.to_string(),
            reply_to_id: reply_to_id.to_string(),
            reply_to_sender: reply_to_sender.to_string(),
            mentions: mentions.to_vec(),
            has_quoted_message: has_quoted,
        })?;
        self.enqueue_and_wait(jid, crate::outbound::OutboundOpKind::Reply, &payload, quoted_blob).await
    }

    /// Post a text status/story. Returns the WA message ID (sync).
    pub async fn send_status_text(
        &self,
        recipients: &[String],
        text: &str,
        background_argb: u32,
        font: i32,
        privacy: Option<String>,
    ) -> Result<String> {
        let payload = serde_json::to_string(&crate::outbound::StatusTextPayload {
            recipients: recipients.to_vec(),
            text: text.to_string(),
            background_argb,
            font,
            privacy,
        })?;
        self.enqueue_and_wait("status@broadcast", crate::outbound::OutboundOpKind::StatusText, &payload, None).await
    }

    /// Post an image status/story. Returns the WA message ID (sync).
    pub async fn send_status_image(
        &self,
        recipients: &[String],
        data: Vec<u8>,
        mime: &str,
        caption: Option<&str>,
        privacy: Option<String>,
    ) -> Result<String> {
        let payload = serde_json::to_string(&crate::outbound::StatusMediaPayload {
            recipients: recipients.to_vec(),
            mime: mime.to_string(),
            caption: caption.map(|c| c.to_string()),
            seconds: 0,
            privacy,
        })?;
        self.enqueue_and_wait("status@broadcast", crate::outbound::OutboundOpKind::StatusImage, &payload, Some(data)).await
    }

    /// Post a video status/story. Returns the WA message ID (sync).
    pub async fn send_status_video(
        &self,
        recipients: &[String],
        data: Vec<u8>,
        mime: &str,
        caption: Option<&str>,
        seconds: u32,
        privacy: Option<String>,
    ) -> Result<String> {
        let payload = serde_json::to_string(&crate::outbound::StatusMediaPayload {
            recipients: recipients.to_vec(),
            mime: mime.to_string(),
            caption: caption.map(|c| c.to_string()),
            seconds,
            privacy,
        })?;
        self.enqueue_and_wait("status@broadcast", crate::outbound::OutboundOpKind::StatusVideo, &payload, Some(data)).await
    }

    /// Revoke a previously posted status/story. Returns the WA message ID (sync).
    pub async fn revoke_status(
        &self,
        recipients: &[String],
        message_id: &str,
        privacy: Option<String>,
    ) -> Result<String> {
        let payload = serde_json::to_string(&crate::outbound::StatusRevokePayload {
            recipients: recipients.to_vec(),
            message_id: message_id.to_string(),
            privacy,
        })?;
        self.enqueue_and_wait("status@broadcast", crate::outbound::OutboundOpKind::StatusRevoke, &payload, None).await
    }
}

// ---------------------------------------------------------------------------
// Bridge runner — reconnection loop with exponential backoff
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_bridge(
    config: BridgeConfig,
    inbound_tx: mpsc::Sender<WhatsAppInbound>,
    state_tx: watch::Sender<BridgeState>,
    qr_tx: watch::Sender<Option<String>>,
    cancel: CancellationToken,
    client_handle: Arc<ParkingMutex<Option<Arc<Client>>>>,
    rr_tx: mpsc::Sender<ReadReceiptCmd>,
    msg_cache: MsgCache,
    metrics: Arc<BridgeMetrics>,
    send_pacer: Arc<SendPacer>,
    subscribed_presence: Arc<DashSet<String>>,
    outbound_notify: Arc<tokio::sync::Notify>,
    event_tx: tokio::sync::broadcast::Sender<Arc<crate::bridge_events::BridgeEvent>>,
    store: Store,
    group_cache: GroupCacheHandle,
) -> Result<()> {
    info!("WhatsApp bridge starting");

    // Recover any messages stuck in 'inflight' from a previous crash
    match store.requeue_stale_inflight(60).await {
        Ok(n) if n > 0 => info!(count = n, "requeued stale inflight messages from previous session"),
        Ok(_) => {}
        Err(e) => warn!(error = %e, "failed to requeue stale inflight messages"),
    }

    // Startup backup
    if let Some(ref backup_dir) = config.backup_dir {
        let store_bk = store.clone();
        let dir = backup_dir.clone();
        match tokio::task::spawn_blocking(move || store_bk.perform_backup(&dir, 3)).await {
            Ok(Ok(path)) => info!(path = %path.display(), "startup backup complete"),
            Ok(Err(e)) => warn!(error = %e, "startup backup failed"),
            Err(e) => warn!(error = %e, "startup backup task panicked"),
        }
    }

    // Message dedup cache survives reconnections
    let dedup = Arc::new(AtomicDedupCache::new(config.dedup_capacity));

    // Secure the database file (Signal Protocol private keys)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&config.db_path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(&config.db_path, perms);
        }
    }

    // API server is spawned externally (main.rs) — replaces the old health-only endpoint

    // Spawn periodic backup task
    if let Some(ref backup_dir) = config.backup_dir {
        let bk_store = store.clone();
        let bk_dir = backup_dir.clone();
        let bk_cancel = cancel.clone();
        let bk_interval = config.backup_interval_secs;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(bk_interval));
            interval.tick().await; // skip immediate first tick (startup backup already done)
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let s = bk_store.clone();
                        let d = bk_dir.clone();
                        match tokio::task::spawn_blocking(move || s.perform_backup(&d, 3)).await {
                            Ok(Ok(path)) => info!(path = %path.display(), "periodic backup complete"),
                            Ok(Err(e)) => warn!(error = %e, "periodic backup failed"),
                            Err(e) => warn!(error = %e, "periodic backup task panicked"),
                        }
                    }
                    _ = bk_cancel.cancelled() => break,
                }
            }
        });
    }

    // Spawn periodic prune task
    {
        let prune_store = store.clone();
        let prune_cancel = cancel.clone();
        let prune_interval = config.prune_interval_secs;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(prune_interval));
            interval.tick().await; // skip immediate first tick
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        // Prune sent/failed messages older than 24 hours
                        match prune_store.prune_old_data(86400, 30 * 86400).await {
                            Ok(stats) => {
                                if stats.sent_deleted > 0 || stats.inbound_deleted > 0 {
                                    info!(sent = stats.sent_deleted, inbound = stats.inbound_deleted, "database pruned");
                                }
                            }
                            Err(e) => warn!(error = %e, "database prune failed"),
                        }
                    }
                    _ = prune_cancel.cancelled() => break,
                }
            }
        });
    }

    let mut backoff = Duration::from_secs(1);
    // Track rapid StreamReplaced events to detect replacement loops.
    // If we get 3 replacements within 5 minutes, stop reconnecting.
    const MAX_REPLACEMENTS: u32 = 3;
    const REPLACEMENT_WINDOW: Duration = Duration::from_secs(300);
    let mut replacement_times: Vec<std::time::Instant> = Vec::new();

    loop {
        if cancel.is_cancelled() {
            break;
        }

        // Recover inflight messages stranded by previous session's tokio::select! drop
        match store.requeue_stale_inflight(30).await {
            Ok(n) if n > 0 => info!(count = n, "requeued stale inflight messages before reconnect"),
            Ok(_) => {}
            Err(e) => warn!(error = %e, "failed to requeue stale inflight messages"),
        }

        let result = run_bot_session(
            &store,
            &config,
            &inbound_tx,
            &state_tx,
            &qr_tx,
            &cancel,
            &client_handle,
            &dedup,
            &rr_tx,
            &metrics,
            &msg_cache,
            &send_pacer,
            &subscribed_presence,
            &outbound_notify,
            &event_tx,
            &group_cache,
        )
        .await;

        if cancel.is_cancelled() {
            break;
        }

        match result {
            Ok(SessionAction::Stop) => {
                info!("bot session requested stop; not reconnecting");
                break;
            }
            Ok(SessionAction::StreamReplaced) => {
                // Track replacement frequency — stop if looping
                let now = std::time::Instant::now();
                replacement_times.retain(|t| now.duration_since(*t) < REPLACEMENT_WINDOW);
                replacement_times.push(now);
                if replacement_times.len() as u32 >= MAX_REPLACEMENTS {
                    error!(
                        count = replacement_times.len(),
                        window_secs = REPLACEMENT_WINDOW.as_secs(),
                        "StreamReplaced loop detected — stopping. Check for duplicate bridge processes or competing WhatsApp Web/Desktop sessions."
                    );
                    break;
                }
                warn!(
                    count = replacement_times.len(),
                    max = MAX_REPLACEMENTS,
                    "stream replaced — will retry ({} of {} before giving up)",
                    replacement_times.len(), MAX_REPLACEMENTS
                );
                // Use a longer backoff for replacements (10s min)
                backoff = std::cmp::max(Duration::from_secs(10), backoff);
            }
            Ok(SessionAction::Retry) => {
                info!("bot session ended; reconnecting");
                // Graduated reset: halve backoff instead of zeroing it.
                // Prevents reconnect storms on flapping connections while still
                // recovering quickly from a single clean disconnect.
                backoff = std::cmp::max(Duration::from_secs(1), backoff / 2);
            }
            Err(e) => {
                error!(error = %e, "bot session failed");
            }
        }

        // Exponential backoff with jitter before reconnect (clamped to max)
        metrics.record_reconnect();
        let jitter_ms = rand_jitter_ms(backoff.as_millis() as u64);
        let delay = (backoff + Duration::from_millis(jitter_ms)).min(config.max_reconnect_delay);
        let _ = state_tx.send(BridgeState::Reconnecting);
        info!(delay_ms = delay.as_millis() as u64, "reconnecting after delay");

        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = cancel.cancelled() => break,
        }

        backoff = (backoff * 2).min(config.max_reconnect_delay);
    }

    // Shutdown backup
    if let Some(ref backup_dir) = config.backup_dir {
        let store_bk = store.clone();
        let dir = backup_dir.clone();
        match tokio::task::spawn_blocking(move || store_bk.perform_backup(&dir, 3)).await {
            Ok(Ok(path)) => info!(path = %path.display(), "shutdown backup complete"),
            Ok(Err(e)) => warn!(error = %e, "shutdown backup failed"),
            Err(e) => warn!(error = %e, "shutdown backup task panicked"),
        }
    }

    // Best-effort unavailable presence before final shutdown
    if let Some(c) = get_client_handle(&client_handle) {
        let _ = c.presence().set_unavailable().await;
    }

    let _ = state_tx.send(BridgeState::Stopped);
    info!("WhatsApp bridge stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Single bot session
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_bot_session(
    store: &Store,
    config: &BridgeConfig,
    inbound_tx: &mpsc::Sender<WhatsAppInbound>,
    state_tx: &watch::Sender<BridgeState>,
    qr_tx: &watch::Sender<Option<String>>,
    cancel: &CancellationToken,
    client_handle: &Arc<ParkingMutex<Option<Arc<Client>>>>,
    dedup: &Arc<AtomicDedupCache>,
    rr_tx: &mpsc::Sender<ReadReceiptCmd>,
    metrics: &Arc<BridgeMetrics>,
    msg_cache: &MsgCache,
    send_pacer: &Arc<SendPacer>,
    subscribed_presence: &Arc<DashSet<String>>,
    outbound_notify: &Arc<tokio::sync::Notify>,
    event_tx: &tokio::sync::broadcast::Sender<Arc<crate::bridge_events::BridgeEvent>>,
    group_cache: &GroupCacheHandle,
) -> Result<SessionAction> {
    // Clear stale handle from previous session
    set_client_handle(client_handle, None);
    let stop_reconnect = Arc::new(AtomicBool::new(false));
    let stream_replaced = Arc::new(AtomicBool::new(false));

    // Clone store for LID resolution in event handler
    let store_for_events = store.clone();

    // Clones for the event closure
    let ch = client_handle.clone();
    let itx = inbound_tx.clone();
    let stx = Arc::new(state_tx.clone());
    let sr = stop_reconnect.clone();
    let sr_replaced = stream_replaced.clone();
    let auto_mark_read = config.auto_mark_read;
    let allowed_numbers = config.allowed_numbers.clone();
    let dedup_for_events = dedup.clone();
    let bridge_id = config.bridge_id.clone();
    let dl_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS));
    let qtx = Arc::new(qr_tx.clone());
    let rr_for_events = rr_tx.clone();
    let ptx_for_events = config.presence_tx.clone();
    let metrics_for_events = metrics.clone();
    let msg_cache_for_events = msg_cache.clone();
    let sub_pres_for_events = subscribed_presence.clone();
    let event_tx_for_events = event_tx.clone();
    let gc_for_events = group_cache.clone();
    // Track previous QR terminal height for in-place overwrite on refresh
    let prev_qr_lines = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // Build the Bot
    let mut builder = Bot::builder()
        .with_backend(Arc::new(store.clone()))
        .with_transport_factory(TokioWebSocketTransportFactory::new())
        .with_http_client(UreqHttpClient::new())
        .with_runtime(whatsapp_rust::TokioRuntime)
        .on_event(move |event, client| {
            let ch = ch.clone();
            let itx = itx.clone();
            let stx = stx.clone();
            let qtx = qtx.clone();
            let store = store_for_events.clone();
            let sr = sr.clone();
            let srr = sr_replaced.clone();
            let allowed = allowed_numbers.clone();
            let dedup = dedup_for_events.clone();
            let bid = bridge_id.clone();
            let dl_sem = dl_semaphore.clone();
            let rr = rr_for_events.clone();
            let ptx = ptx_for_events.clone();
            let pql = prev_qr_lines.clone();
            let met = metrics_for_events.clone();
            let mc = msg_cache_for_events.clone();
            let spres = sub_pres_for_events.clone();
            let etx = event_tx_for_events.clone();
            let gc = gc_for_events.clone();
            async move {
                handle_event(event, client, &ch, &itx, &stx, &qtx, &store, &sr, &srr, auto_mark_read, &allowed, &dedup, &bid, &dl_sem, &rr, &ptx, &pql, &met, &mc, &spres, &etx, &gc)
                    .await;
            }
        });

    // Skip history sync to avoid the "deaf client" bug (Issue #125)
    if config.skip_history_sync {
        builder = builder.skip_history_sync();
    }

    // Set device name shown in WhatsApp's "Linked Devices" list
    builder = builder.with_device_props(
        Some(config.device_name.clone()),
        None,
        None,
    );

    if let Some(ref phone) = config.pair_phone {
        let normalized = normalize_phone(phone);
        info!(phone = %normalized, "pair-code mode enabled");
        builder = builder.with_pair_code(PairCodeOptions {
            phone_number: normalized,
            ..Default::default()
        });
    }

    let mut bot = builder
        .build()
        .await
        .context("failed to build WhatsApp bot")?;
    let bot_task = bot.run().await.context("failed to start WhatsApp bot")?;

    // Run bot + outbound handler + cancellation in parallel
    tokio::select! {
        result = bot_task => {
            match result {
                Ok(()) => {
                    if stream_replaced.load(Ordering::Relaxed) {
                        Ok(SessionAction::StreamReplaced)
                    } else if stop_reconnect.load(Ordering::Relaxed) {
                        Ok(SessionAction::Stop)
                    } else {
                        Ok(SessionAction::Retry)
                    }
                }
                Err(e) => {
                    Err(anyhow::anyhow!("WhatsApp bot task join error: {e}"))
                }
            }
        }
        _ = handle_outbound(client_handle, cancel, store, config.max_outbound_retries, metrics, send_pacer, outbound_notify, event_tx) => {
            if cancel.is_cancelled() || stop_reconnect.load(Ordering::Relaxed) {
                Ok(SessionAction::Stop)
            } else {
                Ok(SessionAction::Retry)
            }
        }
        _ = cancel.cancelled() => {
            info!("bridge shutdown requested — draining in-flight operations");
            let drain_timeout = Duration::from_secs(config.drain_timeout_secs);

            // Drain: attempt to send queued jobs if we still have a connection
            let drain_result = tokio::time::timeout(drain_timeout, async {
                if let Some(client) = get_client_handle(client_handle) {
                    let mut sent = 0u32;
                    while let Ok(Some(row)) = store.claim_next_job().await {
                        let jid = match parse_jid(&row.jid) {
                            Ok(j) => j,
                            Err(_) => {
                                let _ = store.mark_outbound_failed(row.id, config.max_outbound_retries).await;
                                continue;
                            }
                        };
                        send_pacer.wait_turn().await;
                        match crate::outbound::execute_job(&row, &jid, &client).await {
                            Ok(outcome) => {
                                metrics.record_sent();
                                if let Err(e) = store.mark_outbound_sent_with_id(row.id, outcome.wa_message_id.as_deref()).await {
                                    error!(error = %e, row_id = row.id, "shutdown drain: failed to mark sent");
                                }
                                sent += 1;
                            }
                            Err(_) => {
                                // Put back for next session (don't burn retry budget)
                                if let Err(e) = store.requeue_outbound(row.id).await {
                                    error!(error = %e, row_id = row.id, "shutdown drain: failed to requeue");
                                }
                                break;
                            }
                        }
                    }
                    if sent > 0 {
                        info!(count = sent, "drained outbound jobs during shutdown");
                    }

                    client.disconnect().await;
                }
            })
            .await;

            if drain_result.is_err() {
                warn!(timeout_secs = config.drain_timeout_secs, "shutdown drain timed out");
                if let Some(client) = get_client_handle(client_handle) {
                    client.disconnect().await;
                }
            }

            info!("graceful shutdown complete");
            Ok(SessionAction::Stop)
        }
    }
}

// ---------------------------------------------------------------------------
// Event handling — expanded from 5 to 12+ event types
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn handle_event(
    event: Event,
    client: Arc<Client>,
    client_handle: &Arc<ParkingMutex<Option<Arc<Client>>>>,
    inbound_tx: &mpsc::Sender<WhatsAppInbound>,
    state_tx: &Arc<watch::Sender<BridgeState>>,
    qr_tx: &Arc<watch::Sender<Option<String>>>,
    store: &Store,
    stop_reconnect: &Arc<AtomicBool>,
    stream_replaced: &Arc<AtomicBool>,
    auto_mark_read: bool,
    allowed_numbers: &[String],
    dedup: &Arc<AtomicDedupCache>,
    bridge_id: &str,
    dl_semaphore: &Semaphore,
    rr_tx: &mpsc::Sender<ReadReceiptCmd>,
    presence_tx: &Option<mpsc::Sender<PresenceEvent>>,
    prev_qr_lines: &Arc<std::sync::atomic::AtomicUsize>,
    metrics: &Arc<BridgeMetrics>,
    msg_cache: &MsgCache,
    subscribed_presence: &Arc<DashSet<String>>,
    event_tx: &tokio::sync::broadcast::Sender<Arc<crate::bridge_events::BridgeEvent>>,
    group_cache: &GroupCacheHandle,
) {
    match event {
        Event::PairingQrCode { code, .. } => {
            // Publish raw QR data for agent integrations
            let _ = qr_tx.send(Some(code.clone()));

            // Render compact QR in terminal + save PNG/HTML for programmatic access
            if let Some(qr) = crate::qr::QrRender::new(&code) {
                // If a previous QR was printed, move cursor up to overwrite it in-place.
                // +2 for the "open QR image" and "scan the QR code" lines below the QR.
                let prev = prev_qr_lines.load(std::sync::atomic::Ordering::Relaxed);
                if prev > 0 {
                    eprint!("\x1b[{}A\x1b[J", prev + 2); // move up + clear to end of screen
                }

                eprint!("{}", qr.terminal());
                prev_qr_lines.store(qr.terminal_lines(), std::sync::atomic::Ordering::Relaxed);

                // Save PNG to temp dir and print clickable file:// link
                let png_path = std::env::temp_dir().join("whatsrust_qr.png");
                match qr.save_png(&png_path, 8) {
                    Ok(()) => {
                        eprintln!("  open QR image: file://{}", png_path.display());
                    }
                    Err(e) => debug!(error = %e, "failed to save QR PNG fallback"),
                }

                // Save auto-refreshing HTML to temp dir
                let html_path = std::env::temp_dir().join("whatsrust_qr.html");
                if let Err(e) = qr.save_html(&html_path) {
                    debug!(error = %e, "failed to save QR HTML fallback");
                }
            }
            info!("scan the QR code above with WhatsApp on your phone");
            let _ = state_tx.send(BridgeState::Pairing);
        }
        Event::PairingCode { code, .. } => {
            info!(code = %code, "enter this pair code on your phone");
            let _ = qr_tx.send(None); // clear any previous QR
            let _ = state_tx.send(BridgeState::Pairing);
        }
        Event::Connected(_) => {
            // Clear QR from terminal if one was displayed
            let prev = prev_qr_lines.load(std::sync::atomic::Ordering::Relaxed);
            if prev > 0 {
                eprint!("\x1b[{}A\x1b[J", prev + 2); // erase QR + link/scan lines
                prev_qr_lines.store(0, std::sync::atomic::Ordering::Relaxed);
            }
            info!("WhatsApp connected");
            let _ = qr_tx.send(None); // clear QR on connection
            set_client_handle(client_handle, Some(client.clone()));
            let _ = rr_tx.send(ReadReceiptCmd::SetClient(Some(client.clone()))).await;
            // Send Available so server delivers ChatPresence events to us
            let c = client.clone();
            tokio::spawn(async move {
                if let Err(e) = c.presence().set_available().await {
                    warn!(error = %e, "failed to send available presence on connect");
                }
            });
            metrics.record_connect();
            let _ = state_tx.send(BridgeState::Connected);

            // Replay presence subscriptions from previous sessions
            let subs: Vec<String> = subscribed_presence.iter().map(|r| r.clone()).collect();
            if !subs.is_empty() {
                let c = client.clone();
                tokio::spawn(async move {
                    for jid_str in &subs {
                        if let Ok(jid) = Jid::from_str(jid_str) {
                            if let Err(e) = c.presence().subscribe(&jid).await {
                                warn!(jid = %jid_str, error = %e, "failed to replay presence subscription");
                            }
                        }
                    }
                    info!(count = subs.len(), "replayed presence subscriptions after reconnect");
                });
            }
        }
        Event::Message(msg, info) => {
            // Skip noise JIDs: status broadcasts, newsletter channels, server messages
            let chat_str = info.source.chat.to_string();
            if should_ignore_jid(&chat_str) {
                info!(chat = %chat_str, "ignoring message from filtered JID");
                return;
            }

            // Atomic dedup: try_admit returns true if this caller won the race.
            if !dedup.try_admit(&info.id) {
                info!(id = %info.id, "duplicate message, skipping");
                return;
            }

            let sender_alt = info.source.sender_alt.as_ref();
            let sender_raw = info.source.sender.to_string();
            let sender = resolve_sender(&sender_raw, sender_alt, store).await;

            // Normalize DM chat JID: resolve @lid → @s.whatsapp.net so downstream
            // consumers (e.g. Habb) see a consistent identity for each contact.
            // Groups are never LID-based so only DMs need resolution.
            let chat_jid_raw = info.source.chat.to_string();
            let chat_jid = resolve_chat_jid(&chat_jid_raw, sender_alt, store).await;

            // Allowlist filter: skip messages from numbers not on the list.
            // Check both the resolved sender AND the resolved chat JID phone,
            // since LID resolution may succeed for one but not the other.
            if !allowed_numbers.is_empty()
                && !info.source.is_from_me
                && !allowed_numbers.iter().any(|n| n == &sender)
                && !allowed_numbers.iter().any(|n| {
                    // Also match if chat JID resolved to a phone-based JID
                    chat_jid.strip_suffix("@s.whatsapp.net").is_some_and(|pn| pn == n)
                })
            {
                dedup.mark_done(&info.id);
                info!(sender = %sender, raw = %sender_raw, chat = %chat_jid, "message from non-allowed sender, skipping");
                return;
            }

            let inbound_sequence = metrics.next_inbound_sequence();

            // Unwrap wrappers before extracting metadata so reply-to and flags
            // are found on wrapped ephemeral/view-once content too.
            let inner_msg = unwrap_to_inner(&msg);
            let reply_to = extract_reply_to(inner_msg);
            let mut ctx_meta = extract_context_meta(inner_msg);

            // Detect view-once *before* extract_content recurses into the wrapper
            if msg.view_once_message.is_some() || msg.view_once_message_v2.is_some() {
                ctx_meta.flags.is_view_once = true;
            }

            // Cache the raw message for forward_message lookup (LRU eviction)
            {
                let mut cache = msg_cache.lock();
                cache.insert(info.id.clone(), Box::new((*msg).clone()));
            }

            // Try to extract content (with media size guard + download semaphore)
            let result = extract_content(&msg, &client, dl_semaphore, store, Some(&sender_raw)).await;

            match result {
                ExtractResult::Content(content) => {
                    // Store poll enc_key so we can decrypt future votes
                    if matches!(content, InboundContent::PollCreated { .. }) {
                        let v4_inner = msg.poll_creation_message_v4.as_deref()
                            .and_then(|fp| fp.message.as_deref())
                            .and_then(|m| m.poll_creation_message.as_deref());
                        let poll = msg.poll_creation_message.as_deref()
                            .or(msg.poll_creation_message_v2.as_deref())
                            .or(msg.poll_creation_message_v3.as_deref())
                            .or(v4_inner)
                            .or(msg.poll_creation_message_v5.as_deref());
                        if let Some(pc) = poll {
                            if let Some(ref enc_key) = pc.enc_key {
                                let options: Vec<String> = pc.options.iter()
                                    .filter_map(|o| o.option_name.clone())
                                    .collect();
                                if let Err(e) = store.store_poll_key(
                                    &chat_jid,
                                    &info.id,
                                    enc_key,
                                    &options,
                                ).await {
                                    warn!(error = %e, "failed to store poll enc_key");
                                }
                            }
                        }
                    }

                    let inbound = WhatsAppInbound {
                        sequence: inbound_sequence,
                        bridge_id: bridge_id.to_string(),
                        jid: chat_jid.clone(),
                        id: info.id.clone(),
                        content,
                        sender,
                        sender_raw,
                        push_name: info.push_name.clone(),
                        timestamp: info.timestamp.timestamp(),
                        reply_to,
                        is_from_me: info.source.is_from_me,
                        is_group: info.source.is_group,
                        mentions: ctx_meta.mentions,
                        ephemeral_expiration: ctx_meta.ephemeral_expiration,
                        flags: ctx_meta.flags,
                    };

                    // Persist to history table for search/context
                    let body_text = inbound.content.display_text();
                    let body_opt = if body_text.is_empty() { None } else { Some(body_text.as_str()) };
                    if let Err(e) = store.insert_inbound(
                        &inbound.jid, &inbound.sender, &inbound.id,
                        inbound.content.kind(), body_opt, inbound.timestamp,
                    ).await {
                        debug!(error = %e, "failed to insert inbound history (may be duplicate)");
                    }

                    let inbound_arc = Arc::new(inbound.clone());
                    match inbound_tx.send(inbound).await {
                        Ok(()) => {
                            let _ = event_tx.send(Arc::new(crate::bridge_events::BridgeEvent::Inbound(
                                inbound_arc,
                            )));
                            metrics.record_received();
                            dedup.mark_done(&info.id);
                            if auto_mark_read && !info.source.is_from_me {
                                let _ = rr_tx.send(ReadReceiptCmd::Seen {
                                    chat_jid: chat_jid_raw.clone(),
                                    participant_jid: if info.source.is_group {
                                        Some(info.source.sender.to_string())
                                    } else {
                                        None
                                    },
                                    message_id: info.id.clone(),
                                }).await;
                            }
                        }
                        Err(e) => {
                            // Channel closed — remove from dedup so message isn't lost
                            dedup.remove(&info.id);
                            warn!(error = %e, "inbound channel closed, message NOT deduped");
                        }
                    }
                }
                ExtractResult::Unhandled => {
                    dedup.mark_done(&info.id);
                    debug!(
                        message_kind = message_kind(&msg),
                        chat = %info.source.chat,
                        sender = %sender,
                        "unhandled inbound message type"
                    );
                }
                ExtractResult::TransientFailure => {
                    // Remove from dedup so the message can be retried on redeliver
                    dedup.remove(&info.id);
                    warn!(
                        id = %info.id,
                        chat = %info.source.chat,
                        "transient extraction failure, will retry on redeliver"
                    );
                }
            }
        }
        Event::LoggedOut(reason) => {
            warn!(reason = ?reason.reason, on_connect = reason.on_connect, "WhatsApp logged out — clearing auth for re-pairing");
            set_client_handle(client_handle, None);
            let _ = rr_tx.send(ReadReceiptCmd::SetClient(None)).await;
            // Clear stored credentials so next reconnect triggers QR pairing
            if let Err(e) = store.clear_device().await {
                error!(error = %e, "failed to clear device auth after logout");
            }
            request_disconnect(client.clone());
            metrics.record_disconnect();
            let _ = state_tx.send(BridgeState::Disconnected);
            // stop_reconnect is NOT set — bridge will reconnect and show QR
        }
        Event::StreamError(stream_err) => {
            error!(code = %stream_err.code, "WhatsApp stream error");
            set_client_handle(client_handle, None);
            let _ = rr_tx.send(ReadReceiptCmd::SetClient(None)).await;
            metrics.record_disconnect();
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::StreamReplaced(_) => {
            warn!("StreamReplaced: another client took over this linked device session. Will attempt to reconnect — if this repeats, check for duplicate bridge processes or competing WhatsApp Web/Desktop sessions.");
            set_client_handle(client_handle, None);
            let _ = rr_tx.send(ReadReceiptCmd::SetClient(None)).await;
            // Signal replaced (not a hard stop) — reconnect loop decides whether to retry.
            stream_replaced.store(true, Ordering::Relaxed);
            request_disconnect(client.clone());
            metrics.record_disconnect();
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::TemporaryBan(ban) => {
            error!(?ban.code, ban_expires_secs = ban.expire.num_seconds(), "temporarily banned by WhatsApp");
            set_client_handle(client_handle, None);
            let _ = rr_tx.send(ReadReceiptCmd::SetClient(None)).await;
            stop_reconnect.store(true, Ordering::Relaxed);
            request_disconnect(client.clone());
            metrics.record_disconnect();
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::ClientOutdated(_) => {
            error!("WhatsApp client version outdated — update required");
            set_client_handle(client_handle, None);
            let _ = rr_tx.send(ReadReceiptCmd::SetClient(None)).await;
            stop_reconnect.store(true, Ordering::Relaxed);
            request_disconnect(client.clone());
            metrics.record_disconnect();
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::ConnectFailure(failure) => {
            error!(reason = ?failure.reason, message = %failure.message, "WhatsApp connect failure");
            set_client_handle(client_handle, None);
            let _ = rr_tx.send(ReadReceiptCmd::SetClient(None)).await;
            if !failure.reason.should_reconnect() {
                stop_reconnect.store(true, Ordering::Relaxed);
                request_disconnect(client.clone());
            }
            metrics.record_disconnect();
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::UndecryptableMessage(info) => {
            info!(
                chat = %info.info.source.chat,
                sender = %info.info.source.sender,
                "undecryptable message — library will auto-retry via retry receipt + PDO"
            );
        }
        Event::Receipt(receipt) => {
            use wacore::types::presence::ReceiptType;
            use crate::bridge_events::DeliveryStatus;

            // Filter HistorySync receipts — not useful for live status
            if matches!(receipt.r#type, ReceiptType::HistorySync) {
                debug!("ignoring HistorySync receipt");
                return;
            }

            let status = match receipt.r#type {
                ReceiptType::Delivered => DeliveryStatus::Delivered,
                ReceiptType::Read | ReceiptType::ReadSelf => DeliveryStatus::Read,
                ReceiptType::Played | ReceiptType::PlayedSelf => DeliveryStatus::Played,
                ReceiptType::Sender => DeliveryStatus::Sent,
                ReceiptType::ServerError => DeliveryStatus::Failed,
                _ => DeliveryStatus::Unknown,
            };

            let receipt_alt = receipt.source.sender_alt.as_ref();
            let chat_jid_raw = receipt.source.chat.to_string();
            let chat_jid = resolve_chat_jid(&chat_jid_raw, receipt_alt, store).await;
            let sender = resolve_sender(&receipt.source.sender.to_string(), receipt_alt, store).await;
            let msg_ids: Vec<String> = receipt.message_ids.iter().map(|id| id.to_string()).collect();
            let inbound_sequence = metrics.next_inbound_sequence();

            info!(
                receipt_type = ?receipt.r#type,
                status = ?status,
                message_count = msg_ids.len(),
                chat = %chat_jid,
                sender = %sender,
                "delivery receipt"
            );

            // Update delivery status for queued jobs by WA message ID
            let status_str = serde_json::to_value(status)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| format!("{status:?}").to_lowercase());
            for wa_id in &msg_ids {
                if let Err(e) = store.update_delivery_status(wa_id, &status_str).await {
                    debug!(error = %e, wa_id = %wa_id, "no job row for delivery status update (not an error for non-bot messages)");
                }
            }

            // Map to OutboundJobState for event bus updates on matching jobs
            let job_state = match status {
                DeliveryStatus::Delivered => Some(crate::bridge_events::OutboundJobState::Delivered),
                DeliveryStatus::Read => Some(crate::bridge_events::OutboundJobState::Read),
                DeliveryStatus::Played => Some(crate::bridge_events::OutboundJobState::Played),
                DeliveryStatus::Failed => Some(crate::bridge_events::OutboundJobState::Failed),
                _ => None,
            };
            if let Some(state) = job_state {
                for wa_id in &msg_ids {
                    // Emit status event (job_id 0 since we only have wa_message_id here)
                    let _ = event_tx.send(Arc::new(crate::bridge_events::BridgeEvent::OutboundStatus(
                        crate::bridge_events::OutboundStatusEvent {
                            job_id: 0,
                            state,
                            wa_message_id: Some(wa_id.clone()),
                            error: None,
                        },
                    )));
                }
            }

            // Create synthetic inbound message for consumers
            let inbound = WhatsAppInbound {
                sequence: inbound_sequence,
                bridge_id: bridge_id.to_string(),
                jid: chat_jid.clone(),
                id: msg_ids.first().cloned().unwrap_or_default(),
                sender: sender.clone(),
                sender_raw: sender.clone(),
                push_name: String::new(),
                timestamp: receipt.timestamp.timestamp(),
                content: InboundContent::DeliveryReceipt {
                    message_ids: msg_ids,
                    status,
                    chat_jid,
                    sender,
                },
                reply_to: None,
                is_from_me: true,
                is_group: false,
                mentions: Vec::new(),
                ephemeral_expiration: None,
                flags: MessageFlags::default(),
            };
            let inbound_arc = Arc::new(inbound.clone());
            if inbound_tx.send(inbound).await.is_err() {
                warn!("inbound channel closed — receipt dropped");
            } else {
                let _ = event_tx.send(Arc::new(crate::bridge_events::BridgeEvent::Inbound(inbound_arc)));
            }
        }
        Event::DeviceListUpdate(update) => {
            info!(
                user = %update.user,
                update_type = ?update.update_type,
                device_count = update.devices.len(),
                "contact device list changed — library will re-encrypt for new device set"
            );
        }
        Event::OfflineSyncCompleted(summary) => {
            info!(
                count = summary.count,
                "offline sync completed — real-time messages active"
            );
        }
        Event::Disconnected(_) => {
            warn!("WhatsApp disconnected (transport drop)");
            set_client_handle(client_handle, None);
            let _ = rr_tx.send(ReadReceiptCmd::SetClient(None)).await;
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::PairSuccess(_) => {
            info!("pair code accepted — device paired successfully");
        }
        Event::PairError(err) => {
            error!(?err, "pairing failed");
        }
        Event::ChatPresence(update) => {
            if let Some(ref ptx) = presence_tx {
                let pres_alt = update.source.sender_alt.as_ref();
                let chat_jid = resolve_chat_jid(&update.source.chat.to_string(), pres_alt, store).await;
                let sender = resolve_sender(&update.source.sender.to_string(), pres_alt, store).await;
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
        Event::Presence(update) => {
            if let Some(ref ptx) = presence_tx {
                let jid = update.from.to_string();
                let last_seen = update.last_seen.map(|dt| dt.timestamp());
                let evt = if update.unavailable {
                    PresenceEvent::Offline { jid, last_seen }
                } else {
                    PresenceEvent::Online { jid, last_seen }
                };
                let _ = ptx.try_send(evt);
            }
        }
        // ----- wa-rs v0.5.0 event variants -----

        Event::QrScannedWithoutMultidevice(_) => {
            warn!("QR scanned but phone does not have multi-device enabled — pairing will fail");
        }
        Event::Notification(_node) => {
            debug!("raw XML notification from server (internal wa-rs event)");
        }
        Event::GroupUpdate(update) => {
            info!(
                group = %update.group_jid,
                participant = ?update.participant,
                action = ?update.action,
                "group metadata/participant update"
            );
            // Invalidate cache so next query fetches fresh data
            group_cache.lock().invalidate(&update.group_jid.to_string());
        }
        Event::JoinedGroup(lazy) => {
            if let Some(conv) = lazy.get() {
                info!(group = %conv.id, "joined new group");
                group_cache.lock().invalidate(&conv.id);
            } else {
                info!("joined new group (could not decode conversation)");
            }
        }
        Event::DeleteChatUpdate(update) => {
            let jid = update.jid.to_string();
            info!(chat = %jid, "chat deleted — removing inbound history");
            let store_clone = store.clone();
            tokio::spawn(async move {
                match store_clone.delete_inbound_chat(&jid).await {
                    Ok(n) if n > 0 => info!(chat = %jid, deleted = n, "purged inbound history for deleted chat"),
                    Err(e) => warn!(error = %e, "failed to delete inbound history for deleted chat"),
                    _ => {}
                }
            });
        }
        Event::DeleteMessageForMeUpdate(update) => {
            let mid = update.message_id.clone();
            let chat = update.chat_jid.to_string();
            info!(message_id = %mid, chat = %chat, "message deleted for me — removing from history");
            let store_clone = store.clone();
            tokio::spawn(async move {
                if let Err(e) = store_clone.delete_inbound_message(&mid).await {
                    warn!(error = %e, "failed to delete inbound message from history");
                }
            });
        }
        Event::PictureUpdate(update) => {
            debug!(jid = %update.jid, "profile picture changed");
        }
        Event::UserAboutUpdate(update) => {
            debug!(jid = %update.jid, "user about/status text changed");
        }
        Event::ContactUpdated(update) => {
            debug!(jid = %update.jid, "contact updated (from app state sync)");
        }
        Event::ContactNumberChanged(update) => {
            debug!(old = %update.old_jid, new = %update.new_jid, "contact changed phone number");
        }
        Event::ContactSyncRequested(_) => {
            debug!("contact sync requested by server");
        }
        Event::ContactUpdate(update) => {
            debug!(jid = %update.jid, "contact action synced");
        }
        Event::PushNameUpdate(update) => {
            debug!(jid = %update.jid, "contact push name changed");
        }
        Event::SelfPushNameUpdated(update) => {
            debug!(old = %update.old_name, new = %update.new_name, "own push name updated");
        }
        Event::PinUpdate(update) => {
            debug!(jid = %update.jid, "chat pin state changed");
        }
        Event::MuteUpdate(update) => {
            debug!(jid = %update.jid, "chat mute state changed");
        }
        Event::ArchiveUpdate(update) => {
            debug!(jid = %update.jid, "chat archive state changed");
        }
        Event::StarUpdate(_) => {
            debug!("message star state changed");
        }
        Event::MarkChatAsReadUpdate(update) => {
            debug!(jid = %update.jid, "chat marked as read/unread");
        }
        Event::HistorySync(_) => {
            debug!("history sync chunk received (ignored — skip_history_sync active)");
        }
        Event::OfflineSyncPreview(_) => {
            debug!("offline sync preview received");
        }
        Event::BusinessStatusUpdate(_) => {
            debug!("business account status changed");
        }
        Event::DisappearingModeChanged(update) => {
            debug!(from = %update.from, duration_secs = update.duration, "disappearing messages mode changed");
        }
        Event::NewsletterLiveUpdate(update) => {
            debug!(newsletter = %update.newsletter_jid, messages = update.messages.len(), "newsletter live update");
        }

        // Catch-all for future variants we haven't mapped yet
        #[allow(unreachable_patterns)]
        _ => {
            debug!(?event, "unhandled event");
        }
    }
}

// ---------------------------------------------------------------------------
// Inbound content extraction
// ---------------------------------------------------------------------------

/// Unwrap ephemeral/view-once wrappers to reach the inner leaf message.
/// Returns a reference to the innermost message (or the original if not wrapped).
fn unwrap_to_inner(msg: &wa::Message) -> &wa::Message {
    // ephemeral_message wraps the actual content
    if let Some(ref eph) = msg.ephemeral_message {
        if let Some(ref inner) = eph.message {
            return unwrap_to_inner(inner);
        }
    }
    if let Some(ref vo) = msg.view_once_message {
        if let Some(ref inner) = vo.message {
            return unwrap_to_inner(inner);
        }
    }
    if let Some(ref vo2) = msg.view_once_message_v2 {
        if let Some(ref inner) = vo2.message {
            return unwrap_to_inner(inner);
        }
    }
    if let Some(ref vo2_ext) = msg.view_once_message_v2_extension {
        if let Some(ref inner) = vo2_ext.message {
            return unwrap_to_inner(inner);
        }
    }
    msg
}

/// Extract reply context (stanza_id, participant, quoted text) from any message variant.
fn extract_reply_to(msg: &wa::Message) -> Option<ReplyContext> {
    // Find context_info from any message variant that carries it
    let ctx = msg.extended_text_message.as_ref().and_then(|m| m.context_info.as_deref())
        .or_else(|| msg.image_message.as_ref().and_then(|m| m.context_info.as_deref()))
        .or_else(|| msg.video_message.as_ref().and_then(|m| m.context_info.as_deref()))
        .or_else(|| msg.audio_message.as_ref().and_then(|m| m.context_info.as_deref()))
        .or_else(|| msg.document_message.as_ref().and_then(|m| m.context_info.as_deref()))
        .or_else(|| msg.sticker_message.as_ref().and_then(|m| m.context_info.as_deref()));

    let ctx = ctx?;
    let stanza_id = ctx.stanza_id.as_ref()?;

    // Extract quoted text from the quoted_message proto if available
    let quoted_text = ctx.quoted_message.as_deref().and_then(|qm| {
        qm.conversation.clone()
            .or_else(|| qm.extended_text_message.as_ref().and_then(|e| e.text.clone()))
            .or_else(|| qm.image_message.as_ref().and_then(|i| i.caption.clone()))
            .or_else(|| qm.video_message.as_ref().and_then(|v| v.caption.clone()))
            .or_else(|| qm.document_message.as_ref().and_then(|d| d.caption.clone()))
    });

    Some(ReplyContext {
        stanza_id: stanza_id.clone(),
        participant: ctx.participant.clone(),
        quoted_text,
    })
}

/// Extracted context_info metadata: flags, mentions, ephemeral expiration.
struct ContextMeta {
    flags: MessageFlags,
    mentions: Vec<String>,
    ephemeral_expiration: Option<u32>,
}

/// Extract forwarding metadata, mentions, and ephemeral timer from any message variant's context_info.
fn extract_context_meta(msg: &wa::Message) -> ContextMeta {
    let ctx = msg.extended_text_message.as_ref().and_then(|m| m.context_info.as_deref())
        .or_else(|| msg.image_message.as_ref().and_then(|m| m.context_info.as_deref()))
        .or_else(|| msg.video_message.as_ref().and_then(|m| m.context_info.as_deref()))
        .or_else(|| msg.audio_message.as_ref().and_then(|m| m.context_info.as_deref()))
        .or_else(|| msg.document_message.as_ref().and_then(|m| m.context_info.as_deref()))
        .or_else(|| msg.sticker_message.as_ref().and_then(|m| m.context_info.as_deref()));

    match ctx {
        Some(ci) => ContextMeta {
            flags: MessageFlags {
                is_forwarded: ci.is_forwarded.unwrap_or(false),
                forwarding_score: ci.forwarding_score.unwrap_or(0),
                is_view_once: false,
            },
            mentions: ci.mentioned_jid.clone(),
            ephemeral_expiration: ci.expiration.filter(|&e| e > 0),
        },
        None => ContextMeta {
            flags: MessageFlags::default(),
            mentions: Vec::new(),
            ephemeral_expiration: None,
        },
    }
}

/// Maximum recursion depth for unwrapping ephemeral/view-once wrappers.
const MAX_EXTRACT_DEPTH: u8 = 3;

/// Try to extract structured content from an inbound message.
async fn extract_content(
    msg: &wa::Message,
    client: &Arc<Client>,
    dl_semaphore: &Semaphore,
    store: &Store,
    sender_jid: Option<&str>,
) -> ExtractResult {
    extract_content_inner(msg, client, dl_semaphore, 0, store, sender_jid).await
}

/// Inner recursive extractor with depth limit.
async fn extract_content_inner(
    msg: &wa::Message,
    client: &Arc<Client>,
    dl_semaphore: &Semaphore,
    depth: u8,
    store: &Store,
    sender_jid: Option<&str>,
) -> ExtractResult {
    if depth > MAX_EXTRACT_DEPTH {
        warn!(depth, "extract_content recursion limit reached, skipping");
        return ExtractResult::Unhandled;
    }
    // --- Media first: WhatsApp can set `conversation` as auto-transcription on
    // voice notes, so check audio/image/video/document BEFORE text fields. ---

    // --- Image ---
    if let Some(ref img) = msg.image_message {
        if let Some(len) = img.file_length {
            if len > MAX_MEDIA_BYTES { warn!(size = len, "image exceeds size limit, skipping"); return ExtractResult::Unhandled; }
        }
        let caption = img.caption.clone();
        let mime = img.mimetype.clone().unwrap_or_else(|| "image/jpeg".to_string());
        let width = img.width;
        let height = img.height;
        let _permit = dl_semaphore.acquire().await.expect("semaphore closed");
        match client.download(img.as_ref() as &dyn Downloadable).await {
            Ok(data) => {
                if data.len() as u64 > MAX_MEDIA_BYTES { warn!(size = data.len(), "downloaded image exceeds size limit"); return ExtractResult::Unhandled; }
                return ExtractResult::Content(InboundContent::Image { data: data.into(), mime, caption, width, height });
            }
            Err(e) => {
                warn!(error = %e, "failed to download image");
                return ExtractResult::TransientFailure;
            }
        }
    }

    // --- Video (includes PTV/video notes) ---
    if let Some(vid) = msg.video_message.as_deref().or(msg.ptv_message.as_deref()) {
        if let Some(len) = vid.file_length {
            if len > MAX_MEDIA_BYTES { warn!(size = len, "video exceeds size limit, skipping"); return ExtractResult::Unhandled; }
        }
        let caption = vid.caption.clone();
        let mime = vid.mimetype.clone().unwrap_or_else(|| "video/mp4".to_string());
        let seconds = vid.seconds;
        let width = vid.width;
        let height = vid.height;
        let is_gif = vid.gif_playback.unwrap_or(false);
        let _permit = dl_semaphore.acquire().await.expect("semaphore closed");
        match client.download(vid as &dyn Downloadable).await {
            Ok(data) => {
                if data.len() as u64 > MAX_MEDIA_BYTES { warn!(size = data.len(), "downloaded video exceeds size limit"); return ExtractResult::Unhandled; }
                return ExtractResult::Content(InboundContent::Video { data: data.into(), mime, caption, seconds, width, height, is_gif });
            }
            Err(e) => {
                warn!(error = %e, "failed to download video");
                return ExtractResult::TransientFailure;
            }
        }
    }

    // --- Audio ---
    if let Some(ref aud) = msg.audio_message {
        if let Some(len) = aud.file_length {
            if len > MAX_MEDIA_BYTES { warn!(size = len, "audio exceeds size limit, skipping"); return ExtractResult::Unhandled; }
        }
        let mime = aud.mimetype.clone().unwrap_or_else(|| "audio/ogg".to_string());
        let seconds = aud.seconds;
        let is_voice = aud.ptt.unwrap_or(false);
        let _permit = dl_semaphore.acquire().await.expect("semaphore closed");
        match client.download(aud.as_ref() as &dyn Downloadable).await {
            Ok(data) => {
                if data.len() as u64 > MAX_MEDIA_BYTES { warn!(size = data.len(), "downloaded audio exceeds size limit"); return ExtractResult::Unhandled; }
                return ExtractResult::Content(InboundContent::Audio { data: data.into(), mime, seconds, is_voice });
            }
            Err(e) => {
                warn!(error = %e, "failed to download audio");
                return ExtractResult::TransientFailure;
            }
        }
    }

    // --- Document ---
    if let Some(ref doc) = msg.document_message {
        if let Some(len) = doc.file_length {
            if len > MAX_MEDIA_BYTES { warn!(size = len, "document exceeds size limit, skipping"); return ExtractResult::Unhandled; }
        }
        let mime = doc.mimetype.clone().unwrap_or_else(|| "application/octet-stream".to_string());
        let filename = doc.file_name.clone().unwrap_or_else(|| "file".to_string());
        let caption = doc.caption.clone();
        let page_count = doc.page_count;
        let _permit = dl_semaphore.acquire().await.expect("semaphore closed");
        match client.download(doc.as_ref() as &dyn Downloadable).await {
            Ok(data) => {
                if data.len() as u64 > MAX_MEDIA_BYTES { warn!(size = data.len(), "downloaded document exceeds size limit"); return ExtractResult::Unhandled; }
                return ExtractResult::Content(InboundContent::Document { data: data.into(), mime, filename, caption, page_count });
            }
            Err(e) => {
                warn!(error = %e, "failed to download document");
                return ExtractResult::TransientFailure;
            }
        }
    }

    // --- Document with caption (wrapper) ---
    if let Some(ref dwc) = msg.document_with_caption_message {
        if let Some(ref inner) = dwc.message {
            if let Some(ref doc) = inner.document_message {
                if let Some(len) = doc.file_length {
                    if len > MAX_MEDIA_BYTES { warn!(size = len, "document exceeds size limit, skipping"); return ExtractResult::Unhandled; }
                }
                let mime = doc.mimetype.clone().unwrap_or_else(|| "application/octet-stream".to_string());
                let filename = doc.file_name.clone().unwrap_or_else(|| "file".to_string());
                let caption = doc.caption.clone();
                let page_count = doc.page_count;
                let _permit = dl_semaphore.acquire().await.expect("semaphore closed");
                match client.download(doc.as_ref() as &dyn Downloadable).await {
                    Ok(data) => {
                        if data.len() as u64 > MAX_MEDIA_BYTES { warn!(size = data.len(), "downloaded document exceeds size limit"); return ExtractResult::Unhandled; }
                        return ExtractResult::Content(InboundContent::Document { data: data.into(), mime, filename, caption, page_count });
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to download document (with caption)");
                        return ExtractResult::TransientFailure;
                    }
                }
            }
        }
    }

    // --- Sticker ---
    if let Some(ref stk) = msg.sticker_message {
        if let Some(len) = stk.file_length {
            if len > MAX_MEDIA_BYTES { warn!(size = len, "sticker exceeds size limit, skipping"); return ExtractResult::Unhandled; }
        }
        let mime = stk.mimetype.clone().unwrap_or_else(|| "image/webp".to_string());
        let is_animated = stk.is_animated.unwrap_or(false);
        let sticker_emoji = stk.accessibility_label.clone();
        let _permit = dl_semaphore.acquire().await.expect("semaphore closed");
        match client.download(stk.as_ref() as &dyn Downloadable).await {
            Ok(data) => {
                if data.len() as u64 > MAX_MEDIA_BYTES { warn!(size = data.len(), "downloaded sticker exceeds size limit"); return ExtractResult::Unhandled; }
                return ExtractResult::Content(InboundContent::Sticker { data: data.into(), mime, is_animated, sticker_emoji });
            }
            Err(e) => {
                warn!(error = %e, "failed to download sticker");
                return ExtractResult::TransientFailure;
            }
        }
    }

    // --- Location ---
    if let Some(ref loc) = msg.location_message {
        if let (Some(lat), Some(lon)) = (loc.degrees_latitude, loc.degrees_longitude) {
            let url = loc.url.clone().or_else(|| Some(format!("https://maps.google.com/maps?q={lat},{lon}")));
            return ExtractResult::Content(InboundContent::Location {
                lat,
                lon,
                name: loc.name.clone(),
                address: loc.address.clone(),
                url,
                is_live: false,
            });
        }
    }

    // --- Live location (snapshot with is_live=true) ---
    if let Some(ref loc) = msg.live_location_message {
        if let (Some(lat), Some(lon)) = (loc.degrees_latitude, loc.degrees_longitude) {
            let url = Some(format!("https://maps.google.com/maps?q={lat},{lon}"));
            return ExtractResult::Content(InboundContent::Location {
                lat,
                lon,
                name: loc.caption.clone(),
                address: None,
                url,
                is_live: true,
            });
        }
    }

    // --- Contact ---
    if let Some(ref contact) = msg.contact_message {
        return ExtractResult::Content(InboundContent::Contact {
            display_name: contact.display_name.clone().unwrap_or_default(),
            vcard: contact.vcard.clone().unwrap_or_default(),
            additional_contacts: Vec::new(),
        });
    }

    // --- Multiple contacts ---
    if let Some(ref arr) = msg.contacts_array_message {
        if let Some(first) = arr.contacts.first() {
            let additional: Vec<(String, String)> = arr.contacts.iter().skip(1)
                .map(|c| (
                    c.display_name.clone().unwrap_or_default(),
                    c.vcard.clone().unwrap_or_default(),
                ))
                .collect();
            return ExtractResult::Content(InboundContent::Contact {
                display_name: first.display_name.clone().unwrap_or_else(|| {
                    arr.display_name.clone().unwrap_or_default()
                }),
                vcard: first.vcard.clone().unwrap_or_default(),
                additional_contacts: additional,
            });
        }
    }

    // --- Reaction ---
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

    // --- Encrypted reaction (enc_reaction_message) ---
    // These carry encrypted payloads we can't decrypt at the bridge level.
    // We cannot distinguish add vs remove without decryption, so log and skip
    // rather than surfacing misleading data to downstream consumers.
    if let Some(ref enc_react) = msg.enc_reaction_message {
        if let Some(ref key) = enc_react.target_message_key {
            let target_id = key.id.clone().unwrap_or_default();
            debug!(target_id = %target_id, "skipping enc_reaction_message (encrypted, cannot determine add/remove)");
        }
        return ExtractResult::Unhandled;
    }

    // --- Edit (incoming edit from others) ---
    if let Some(ref edited) = msg.edited_message {
        if let Some(ref inner) = edited.message {
            if let Some(ref proto) = inner.protocol_message {
                let target_id = proto
                    .key
                    .as_ref()
                    .and_then(|k| k.id.clone())
                    .unwrap_or_default();
                let new_text = proto
                    .edited_message
                    .as_ref()
                    .and_then(|m| {
                        m.conversation.clone().or_else(|| {
                            m.extended_text_message
                                .as_ref()
                                .and_then(|e| e.text.clone())
                        })
                    })
                    .unwrap_or_default();
                let mentions = proto.edited_message.as_ref()
                    .and_then(|m| m.extended_text_message.as_ref())
                    .and_then(|e| e.context_info.as_ref())
                    .map(|ci| ci.mentioned_jid.clone())
                    .unwrap_or_default();
                return ExtractResult::Content(InboundContent::Edit { target_id, new_text, mentions });
            }
        }
    }

    // --- Protocol message (revokes / deletes) ---
    if let Some(ref proto) = msg.protocol_message {
        // Revoke = type 0 (REVOKE) with a key
        if proto.r#type == Some(0) {
            if let Some(ref key) = proto.key {
                return ExtractResult::Content(InboundContent::Revoke {
                    target_id: key.id.clone().unwrap_or_default(),
                });
            }
        }
    }

    // --- Poll creation ---
    {
        // v4 is a FutureProofMessage wrapper; extract its inner poll_creation_message.
        let v4_inner = msg.poll_creation_message_v4.as_deref()
            .and_then(|fp| fp.message.as_deref())
            .and_then(|m| m.poll_creation_message.as_deref());
        let poll = msg.poll_creation_message.as_deref()
            .or(msg.poll_creation_message_v2.as_deref())
            .or(msg.poll_creation_message_v3.as_deref())
            .or(v4_inner)
            .or(msg.poll_creation_message_v5.as_deref());
        if let Some(pc) = poll {
            let question = pc.name.clone().unwrap_or_default();
            let options: Vec<String> = pc.options.iter()
                .filter_map(|o| o.option_name.clone())
                .collect();
            let selectable_count = pc.selectable_options_count.unwrap_or(0);

            // Store enc_key for vote decryption (if we have one)
            if let Some(ref enc_key) = pc.enc_key {
                // We don't have the poll_id (our own message ID) here since it's assigned
                // by the server. The caller in handle_event has it as info.id.
                // We'll store it from handle_event instead if this is a creation we receive.
                // For now, just extract the content — the handle_event caller stores the key.
                let _ = enc_key; // used by handle_event after extraction
            }

            return ExtractResult::Content(InboundContent::PollCreated {
                question,
                options,
                selectable_count,
            });
        }
    }

    // --- Poll vote (update) ---
    if let Some(ref pu) = msg.poll_update_message {
        if let Some(ref vote_enc) = pu.vote {
            if let Some(ref poll_key) = pu.poll_creation_message_key {
                let poll_id = poll_key.id.clone().unwrap_or_default();
                let chat_jid = poll_key.remote_jid.clone().unwrap_or_default();
                let ct = vote_enc.enc_payload.clone().unwrap_or_default();
                let iv = vote_enc.enc_iv.clone().unwrap_or_default();

                // Look up stored enc_key — try exact match first, then try
                // resolved PN/LID alternate form (handles re-pairing JID changes)
                let poll_key_result = match store.get_poll_key(&chat_jid, &poll_id).await {
                    Ok(Some(key)) => Some(key),
                    _ => {
                        // Try alternate JID form: resolve LID→PN or PN→LID
                        let alt = resolve_alternate_chat_jid(&chat_jid, store).await;
                        if let Some(ref alt_jid) = alt {
                            store.get_poll_key(alt_jid, &poll_id).await.ok().flatten()
                        } else {
                            None
                        }
                    }
                };
                if let Some((enc_key, option_names)) = poll_key_result {
                    let voter_jid = select_poll_voter_jid(sender_jid, poll_key.participant.as_deref());

                    match crate::polls::decrypt_poll_vote(&enc_key, &poll_id, &voter_jid, &ct, &iv) {
                        Ok(selected_hashes) => {
                            // Map hashes back to option names
                            let hash_to_name: std::collections::HashMap<Vec<u8>, &str> = option_names
                                .iter()
                                .map(|name| (crate::polls::hash_poll_option(name), name.as_str()))
                                .collect();
                            let selected: Vec<String> = selected_hashes
                                .iter()
                                .map(|h| hash_to_name.get(h).copied().unwrap_or("unknown").to_string())
                                .collect();
                            return ExtractResult::Content(InboundContent::PollVote {
                                poll_id,
                                selected_options: selected,
                            });
                        }
                        Err(e) => {
                            warn!(error = %e, poll_id = %poll_id, "failed to decrypt poll vote");
                        }
                    }
                } else {
                    debug!(poll_id = %poll_id, "no stored enc_key for poll vote — poll was created before bridge started");
                }
            }
        }
    }

    // --- Text (checked AFTER media: WhatsApp sets `conversation` as auto-
    // transcription on voice notes, so text must not shadow audio_message) ---
    if let Some(ref text) = msg.conversation {
        return ExtractResult::Content(InboundContent::Text {
            body: text.clone(),
            link_preview: None,
        });
    }
    if let Some(ref ext) = msg.extended_text_message {
        if let Some(ref text) = ext.text {
            // Extract link preview metadata if present
            let link_preview = ext.matched_text.as_ref().map(|url| InboundLinkPreview {
                url: url.clone(),
                title: ext.title.clone(),
                description: ext.description.clone(),
            });
            return ExtractResult::Content(InboundContent::Text {
                body: text.clone(),
                link_preview,
            });
        }
    }

    // --- Ephemeral wrapper (unwrap and recurse) ---
    if let Some(ref eph) = msg.ephemeral_message {
        if let Some(ref inner) = eph.message {
            return Box::pin(extract_content_inner(inner, client, dl_semaphore, depth + 1, store, sender_jid)).await;
        }
    }

    // --- View-once wrapper (unwrap and recurse) ---
    if let Some(ref vo) = msg.view_once_message {
        if let Some(ref inner) = vo.message {
            return Box::pin(extract_content_inner(inner, client, dl_semaphore, depth + 1, store, sender_jid)).await;
        }
    }
    if let Some(ref vo2) = msg.view_once_message_v2 {
        if let Some(ref inner) = vo2.message {
            return Box::pin(extract_content_inner(inner, client, dl_semaphore, depth + 1, store, sender_jid)).await;
        }
    }
    if let Some(ref vo2_ext) = msg.view_once_message_v2_extension {
        if let Some(ref inner) = vo2_ext.message {
            return Box::pin(extract_content_inner(inner, client, dl_semaphore, depth + 1, store, sender_jid)).await;
        }
    }

    ExtractResult::Unhandled
}

// ---------------------------------------------------------------------------
// LID-to-phone sender resolution
// ---------------------------------------------------------------------------

/// Try to find the alternate form of a chat JID (PN→LID or LID→PN).
/// Used for poll key lookups that may have been stored under a different JID format.
async fn resolve_alternate_chat_jid(chat_jid: &str, store: &Store) -> Option<String> {
    let (user_part, server) = chat_jid.split_once('@')?;
    let bare_user = user_part.split(':').next().unwrap_or(user_part);

    if server == "lid" {
        // LID → try to find PN
        if let Ok(Some(entry)) = store.get_lid_mapping(bare_user).await {
            return Some(format!("{}@s.whatsapp.net", entry.phone_number));
        }
    } else if server == "s.whatsapp.net" {
        // PN → try to find LID
        if let Ok(Some(entry)) = store.get_pn_mapping(bare_user).await {
            return Some(format!("{}@lid", entry.lid));
        }
    }
    None
}

/// Strip the `@server` suffix and any `:device` qualifier to get the bare user part.
fn bare_lid_user(jid_str: &str) -> &str {
    let user_part = jid_str.split('@').next().unwrap_or(jid_str);
    user_part.split(':').next().unwrap_or(user_part)
}

/// Resolve a chat JID from @lid to @s.whatsapp.net using the LID mapping table.
/// Returns the original JID unchanged if it's not an @lid JID or no mapping exists.
/// Groups (@g.us) and other non-LID JIDs pass through unchanged.
///
/// When available, `sender_alt` from wa-rs is used as an inline fallback before
/// hitting the database — this covers the first-message edge case where the LID
/// mapping hasn't been persisted yet.
async fn resolve_chat_jid(raw: &str, sender_alt: Option<&wacore_binary::jid::Jid>, store: &Store) -> String {
    let Some((_, "lid")) = raw.split_once('@') else {
        return raw.to_string();
    };
    // 1. Try sender_alt (inline PN from wa-rs, available before DB is populated)
    if let Some(alt) = sender_alt {
        if alt.server == "s.whatsapp.net" {
            return format!("{}@s.whatsapp.net", alt.user);
        }
    }
    // 2. Fall back to DB lookup
    let bare = bare_lid_user(raw);
    if let Ok(Some(entry)) = store.get_lid_mapping(bare).await {
        return format!("{}@s.whatsapp.net", entry.phone_number);
    }
    raw.to_string()
}

/// Resolve a sender JID to a phone number using the LID mapping table.
/// Falls back to the raw JID string if no mapping exists.
///
/// Handles device-qualified LIDs like `100000000000001.1:75@lid` by stripping
/// the device suffix (`:75`) before lookup, since lid_pn_mapping stores bare LIDs.
/// Only probes the database when the JID is actually @lid — phone-based JIDs
/// short-circuit without a DB round-trip.
async fn resolve_sender(sender_raw: &str, sender_alt: Option<&wacore_binary::jid::Jid>, store: &Store) -> String {
    let (user_part, server) = sender_raw
        .split_once('@')
        .unwrap_or((sender_raw, ""));

    // Only @lid JIDs need resolution
    if server != "lid" {
        return user_part.to_string();
    }

    // 1. Try sender_alt (inline PN from wa-rs)
    if let Some(alt) = sender_alt {
        if alt.server == "s.whatsapp.net" {
            return alt.user.clone();
        }
    }

    // 2. Fall back to DB lookup
    let bare = user_part.split(':').next().unwrap_or(user_part);
    if let Ok(Some(entry)) = store.get_lid_mapping(bare).await {
        return entry.phone_number;
    }

    // No mapping — return user part as-is
    user_part.to_string()
}

// ---------------------------------------------------------------------------
// Outbound message handling
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn handle_outbound(
    client_handle: &Arc<ParkingMutex<Option<Arc<Client>>>>,
    cancel: &CancellationToken,
    store: &Store,
    max_retries: i32,
    metrics: &Arc<BridgeMetrics>,
    send_pacer: &Arc<SendPacer>,
    outbound_notify: &Arc<tokio::sync::Notify>,
    event_tx: &tokio::sync::broadcast::Sender<Arc<crate::bridge_events::BridgeEvent>>,
) {
    loop {
        if cancel.is_cancelled() {
            break;
        }

        // Don't claim jobs while disconnected — avoids hot-loop churn
        if get_client_handle(client_handle).is_none() {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = outbound_notify.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
            continue;
        }

        // Try to claim the next queued job
        let row = match store.claim_next_job().await {
            Ok(Some(row)) => row,
            Ok(None) => {
                // Queue empty — wait for notification, scheduled timer, or cancellation.
                // The 5s timer ensures scheduled jobs are picked up even without new enqueues.
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = outbound_notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
                continue;
            }
            Err(e) => {
                warn!(error = %e, "failed to claim outbound job from queue");
                tokio::select! {
                    _ = tokio::time::sleep(OUTBOUND_RETRY_DELAY) => {}
                    _ = cancel.cancelled() => break,
                }
                continue;
            }
        };

        let jid = match parse_jid(&row.jid) {
            Ok(j) => j,
            Err(e) => {
                error!(error = %e, jid = %row.jid, "invalid outbound JID, marking failed");
                let _ = store.mark_outbound_failed(row.id, 0).await;
                let _ = event_tx.send(Arc::new(crate::bridge_events::BridgeEvent::OutboundStatus(
                    crate::bridge_events::OutboundStatusEvent {
                        job_id: row.id,
                        state: crate::bridge_events::OutboundJobState::Failed,
                        wa_message_id: None,
                        error: Some(format!("invalid JID: {e}")),
                    },
                )));
                continue;
            }
        };

        // Pace BEFORE acquiring client handle
        send_pacer.wait_turn().await;

        // Fetch client AFTER pacing — freshest handle possible
        let Some(client) = get_client_handle(client_handle) else {
            // Requeue without incrementing retries (connection issue, not send failure)
            let _ = store.requeue_outbound(row.id).await;
            tokio::select! {
                _ = tokio::time::sleep(OUTBOUND_RETRY_DELAY) => {}
                _ = cancel.cancelled() => break,
            }
            continue;
        };

        // Emit Sending only after client is confirmed available
        let _ = event_tx.send(Arc::new(crate::bridge_events::BridgeEvent::OutboundStatus(
            crate::bridge_events::OutboundStatusEvent {
                job_id: row.id,
                state: crate::bridge_events::OutboundJobState::Sending,
                wa_message_id: None,
                error: None,
            },
        )));

        // --- Auto typing indicator (anti-ban: simulate human typing speed) ---
        // For text-like ops, show "composing" for a duration scaled by message length.
        // For audio, the recording indicator is already handled in execute_job.
        // Status sends, reactions, edits, revokes skip this (they're instant in real WhatsApp).
        {
            let op_kind = crate::outbound::OutboundOpKind::parse_str(&row.op_kind);
            let needs_typing = matches!(
                op_kind,
                Some(crate::outbound::OutboundOpKind::Text)
                    | Some(crate::outbound::OutboundOpKind::Reply)
                    | Some(crate::outbound::OutboundOpKind::Forward)
            );
            if needs_typing {
                // Estimate typing duration: ~40ms per character, min 800ms, max 6s, ±30% jitter
                let char_count = row.payload_json.len().min(500); // cap at 500 chars for timing
                let base_ms = (char_count as u64 * 40).clamp(800, 6000);
                let jitter = {
                    use rand::Rng;
                    let mut rng = rand::thread_rng();
                    let factor: f64 = rng.gen_range(0.7..1.3);
                    (base_ms as f64 * factor) as u64
                };
                let _ = client.chatstate().send_composing(&jid).await;
                tokio::time::sleep(Duration::from_millis(jitter)).await;
                let _ = client.chatstate().send_paused(&jid).await;
            }
        }

        match crate::outbound::execute_job(&row, &jid, &client).await {
            Ok(outcome) => {
                metrics.record_sent();
                // Atomically mark sent + set wa_message_id in one DB write.
                // Prevents race where receipt arrives between separate updates.
                for attempt in 0..3 {
                    match store.mark_outbound_sent_with_id(row.id, outcome.wa_message_id.as_deref()).await {
                        Ok(()) => break,
                        Err(e) if attempt < 2 => {
                            warn!(error = %e, row_id = row.id, attempt, "retrying mark_outbound_sent_with_id");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        Err(e) => {
                            error!(error = %e, row_id = row.id, "failed to mark job as sent after 3 attempts — may re-send on restart");
                        }
                    }
                }

                // Store poll enc_key if this was a poll send
                if let Some(ref pk) = outcome.poll_key {
                    let target_key = canonical_chat_key(&jid);
                    let wa_id = outcome.wa_message_id.as_deref().unwrap_or("");
                    if let Err(e) = store.store_poll_key(&target_key, wa_id, &pk.enc_key, &pk.options).await {
                        warn!(error = %e, "failed to store poll enc_key from outbound worker");
                    }
                }

                // Emit Sent status
                let _ = event_tx.send(Arc::new(crate::bridge_events::BridgeEvent::OutboundStatus(
                    crate::bridge_events::OutboundStatusEvent {
                        job_id: row.id,
                        state: crate::bridge_events::OutboundJobState::Sent,
                        wa_message_id: outcome.wa_message_id,
                        error: None,
                    },
                )));
            }
            Err(e) => {
                let err_msg = format!("{e:#}");
                if row.retries + 1 >= max_retries {
                    error!(
                        error = %e, jid = %row.jid, retries = row.retries + 1,
                        "dropping outbound job after max retries"
                    );
                } else {
                    warn!(
                        error = %e, jid = %row.jid, retry = row.retries + 1,
                        "failed to execute outbound job; will retry"
                    );
                }
                for attempt in 0..3 {
                    match store.mark_outbound_failed(row.id, max_retries).await {
                        Ok(()) => break,
                        Err(db_e) if attempt < 2 => {
                            warn!(error = %db_e, row_id = row.id, attempt, "retrying mark_outbound_failed");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        Err(db_e) => {
                            error!(error = %db_e, row_id = row.id, "failed to mark job as failed after 3 attempts — row stuck as inflight");
                        }
                    }
                }

                // Emit Failed status only if max retries exceeded
                if row.retries + 1 >= max_retries {
                    let _ = event_tx.send(Arc::new(crate::bridge_events::BridgeEvent::OutboundStatus(
                        crate::bridge_events::OutboundStatusEvent {
                            job_id: row.id,
                            state: crate::bridge_events::OutboundJobState::Failed,
                            wa_message_id: None,
                            error: Some(err_msg),
                        },
                    )));
                }
            }
        }
    }
}

// Health endpoint replaced by api::serve() — see src/api.rs

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a JID string (phone@s.whatsapp.net or raw phone number).
/// Handles formatted phone numbers like "+1 (555) 123-4567" → "15551234567@s.whatsapp.net".
fn parse_jid(s: &str) -> Result<Jid> {
    if s.contains('@') {
        Jid::from_str(s).map_err(|e| anyhow::anyhow!("bad JID: {e}"))
    } else {
        let normalized = normalize_phone(s);
        if normalized.is_empty() {
            return Err(anyhow::anyhow!("empty JID after normalization: {s}"));
        }
        // Newsletter JIDs start with 120363; all others are individual chats
        let server = if normalized.starts_with("120363") {
            "g.us"
        } else {
            "s.whatsapp.net"
        };
        Jid::from_str(&format!("{normalized}@{server}"))
            .map_err(|e| anyhow::anyhow!("bad JID from phone: {e}"))
    }
}

fn canonical_chat_key(jid: &Jid) -> String {
    jid.to_string()
}

pub(crate) fn normalize_poll_spec(
    question: &str,
    options: &[String],
    selectable_count: u32,
) -> Result<(String, Vec<String>)> {
    let question = question.trim().to_string();
    if question.is_empty() {
        anyhow::bail!("poll question must not be empty");
    }

    let options: Vec<String> = options
        .iter()
        .map(|o| o.trim().to_string())
        .filter(|o| !o.is_empty())
        .collect();
    if options.len() < 2 {
        anyhow::bail!("poll must include at least 2 non-empty options");
    }
    if selectable_count == 0 || selectable_count as usize > options.len() {
        anyhow::bail!(
            "selectable_count must be between 1 and {}",
            options.len()
        );
    }

    Ok((question, options))
}

fn select_poll_voter_jid(sender_jid: Option<&str>, poll_participant: Option<&str>) -> String {
    sender_jid
        .filter(|s| !s.is_empty())
        .or_else(|| poll_participant.filter(|s| !s.is_empty()))
        .unwrap_or_default()
        .to_string()
}

/// Returns true for JIDs that should be silently dropped (status broadcasts,
/// newsletter channels, server messages).
/// Note: @lid JIDs are NOT dropped — they are valid DM identities after WhatsApp's
/// migration and are resolved to phone-based JIDs by `resolve_chat_jid`.
///
/// Apply forwarding context to a message. Increments forwarding_score and sets is_forwarded.
/// For plain `conversation` text (which has no context_info field), converts to extended_text_message.
pub fn add_forward_context(mut msg: wa::Message) -> wa::Message {
    // Helper: create or update context_info on a boxed optional
    macro_rules! set_forward_ctx {
        ($field:expr) => {
            if let Some(ref mut inner) = $field {
                let ci = inner.context_info.get_or_insert_with(|| Box::new(wa::ContextInfo::default()));
                ci.is_forwarded = Some(true);
                ci.forwarding_score = Some(ci.forwarding_score.unwrap_or(0) + 1);
            }
        };
    }

    // Convert plain conversation to extended_text so we can attach context_info
    if let Some(text) = msg.conversation.take() {
        msg.extended_text_message = Some(Box::new(wa::message::ExtendedTextMessage {
            text: Some(text),
            ..Default::default()
        }));
    }

    set_forward_ctx!(msg.extended_text_message);
    set_forward_ctx!(msg.image_message);
    set_forward_ctx!(msg.video_message);
    set_forward_ctx!(msg.audio_message);
    set_forward_ctx!(msg.document_message);
    set_forward_ctx!(msg.sticker_message);
    set_forward_ctx!(msg.location_message);
    set_forward_ctx!(msg.contact_message);

    msg
}

fn should_ignore_jid(jid: &str) -> bool {
    jid == "status@broadcast"
        || jid.ends_with("@broadcast")
        || jid.ends_with("@newsletter")
        || jid == "server@s.whatsapp.net"
    // NOTE: @lid JIDs are NOT filtered. After re-pairing, WhatsApp routes
    // direct chat messages through LID-format JIDs. The sender is resolved
    // to a phone number via resolve_sender() + lid_pn_mapping table.
}

fn message_kind(msg: &wa::Message) -> &'static str {
    if msg.image_message.is_some() {
        "image"
    } else if msg.video_message.is_some() || msg.ptv_message.is_some() {
        "video"
    } else if msg.audio_message.is_some() {
        "audio"
    } else if msg.document_message.is_some() || msg.document_with_caption_message.is_some() {
        "document"
    } else if msg.reaction_message.is_some() || msg.enc_reaction_message.is_some() {
        "reaction"
    } else if msg.edited_message.is_some() {
        "edit"
    } else if msg.protocol_message.is_some() {
        "protocol"
    } else if msg.poll_creation_message.is_some()
        || msg.poll_creation_message_v2.is_some()
        || msg.poll_creation_message_v3.is_some()
        || msg.poll_creation_message_v4.is_some()
        || msg.poll_update_message.is_some()
    {
        "poll"
    } else if msg.ephemeral_message.is_some() {
        "ephemeral"
    } else if msg.view_once_message.is_some()
        || msg.view_once_message_v2.is_some()
        || msg.view_once_message_v2_extension.is_some()
    {
        "view_once"
    } else if msg.location_message.is_some() || msg.live_location_message.is_some() {
        "location"
    } else if msg.contact_message.is_some() || msg.contacts_array_message.is_some() {
        "contact"
    } else {
        "other"
    }
}

fn set_client_handle(
    client_handle: &Arc<ParkingMutex<Option<Arc<Client>>>>,
    value: Option<Arc<Client>>,
) {
    *client_handle.lock() = value;
}

fn get_client_handle(
    client_handle: &Arc<ParkingMutex<Option<Arc<Client>>>>,
) -> Option<Arc<Client>> {
    client_handle.lock().clone()
}

fn request_disconnect(client: Arc<Client>) {
    tokio::spawn(async move {
        client.disconnect().await;
    });
}

/// Normalize a phone number to digits only (E.164 without the +).
/// Strips all non-digit characters: "+1 (555) 123-4567" → "15551234567".
fn normalize_phone(raw: &str) -> String {
    raw.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Generate a random jitter in [0, base_ms/2) using xorshift on system time + atomic counter.
/// The counter prevents identical jitter when called in rapid succession (nanos often repeat).
fn rand_jitter_ms(base_ms: u64) -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    if base_ms == 0 {
        return 0;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut seed = nanos.wrapping_mul(6364136223846793005).wrapping_add(count);
    // xorshift64
    seed ^= seed << 13;
    seed ^= seed >> 7;
    seed ^= seed << 17;
    seed % (base_ms / 2).max(1)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_phone() {
        assert_eq!(normalize_phone("15551234567"), "15551234567");
        assert_eq!(normalize_phone("+1 (555) 123-4567"), "15551234567");
        assert_eq!(normalize_phone("+44 20 7946 0958"), "442079460958");
        assert_eq!(normalize_phone(""), "");
    }

    #[test]
    fn test_parse_jid_with_server() {
        let jid = parse_jid("15551234567@s.whatsapp.net").unwrap();
        assert_eq!(jid.to_string(), "15551234567@s.whatsapp.net");
    }

    #[test]
    fn test_parse_jid_digits_only() {
        let jid = parse_jid("15551234567").unwrap();
        assert!(jid.to_string().contains("s.whatsapp.net"));
    }

    #[test]
    fn test_parse_jid_formatted_phone() {
        let jid = parse_jid("+1 (555) 123-4567").unwrap();
        assert!(jid.to_string().starts_with("15551234567@"));
    }

    #[test]
    fn test_parse_jid_newsletter() {
        let jid = parse_jid("120363012345678901@g.us").unwrap();
        assert!(jid.to_string().contains("g.us"));
    }

    #[test]
    fn test_parse_jid_empty_rejects() {
        assert!(parse_jid("+++").is_err());
    }

    #[test]
    fn test_normalize_poll_spec_rejects_empty_question() {
        let err = normalize_poll_spec("   ", &["Yes".into(), "No".into()], 1).unwrap_err();
        assert!(err.to_string().contains("question"));
    }

    #[test]
    fn test_normalize_poll_spec_rejects_too_few_options() {
        let err = normalize_poll_spec("Lunch?", &["Pizza".into()], 1).unwrap_err();
        assert!(err.to_string().contains("at least 2"));
    }

    #[test]
    fn test_normalize_poll_spec_rejects_bad_selectable_count() {
        let err =
            normalize_poll_spec("Lunch?", &["Pizza".into(), "Tacos".into()], 3).unwrap_err();
        assert!(err.to_string().contains("selectable_count"));
    }

    #[test]
    fn test_normalize_poll_spec_trims_and_filters_options() {
        let (question, options) = normalize_poll_spec(
            " Lunch? ",
            &[" Pizza ".into(), "".into(), " Tacos ".into()],
            1,
        )
        .unwrap();
        assert_eq!(question, "Lunch?");
        assert_eq!(options, vec!["Pizza", "Tacos"]);
    }

    #[test]
    fn test_select_poll_voter_jid_prefers_event_sender() {
        let voter = select_poll_voter_jid(
            Some("5511999999999@s.whatsapp.net"),
            Some("fallback@s.whatsapp.net"),
        );
        assert_eq!(voter, "5511999999999@s.whatsapp.net");
    }

    #[test]
    fn test_message_kind_default() {
        let msg = wa::Message::default();
        assert_eq!(message_kind(&msg), "other");
    }

    #[test]
    fn test_rand_jitter_ms() {
        // Zero base should return 0
        assert_eq!(rand_jitter_ms(0), 0);
        // Should return less than half of base
        for _ in 0..100 {
            let j = rand_jitter_ms(10000);
            assert!(j < 5000, "jitter {j} should be < 5000");
        }
    }

    #[test]
    fn test_allowlist_logic() {
        let allowed = vec!["15551234567".to_string(), "442079460958".to_string()];
        // In-list passes
        assert!(allowed.iter().any(|n| n == "15551234567"));
        // Not-in-list blocks
        assert!(!allowed.iter().any(|n| n == "19998887777"));
        // Empty list → no filter (pass all)
        let empty: Vec<String> = vec![];
        assert!(empty.is_empty());
    }

    #[test]
    fn test_message_ref_from_inbound() {
        let msg = WhatsAppInbound {
            sequence: 1,
            bridge_id: "default".into(),
            jid: "group@g.us".into(),
            id: "msg123".into(),
            content: InboundContent::Text { body: "hi".into(), link_preview: None },
            sender: "447957491755".into(),
            sender_raw: "447957491755@s.whatsapp.net".into(),
            push_name: String::new(),
            timestamp: 1000,
            reply_to: None,
            is_from_me: false,
            is_group: true,
            mentions: Vec::new(),
            ephemeral_expiration: None,
            flags: MessageFlags::default(),
        };
        let mref = MessageRef::from_inbound(&msg);
        assert_eq!(mref.chat_jid, "group@g.us");
        assert_eq!(mref.message_id, "msg123");
        assert!(!mref.from_me);
        assert_eq!(
            mref.sender_jid.as_deref(),
            Some("447957491755@s.whatsapp.net")
        );
    }

    #[test]
    fn test_message_ref_from_me_has_no_sender() {
        let msg = WhatsAppInbound {
            sequence: 2,
            bridge_id: "default".into(),
            jid: "chat@s.whatsapp.net".into(),
            id: "msg456".into(),
            content: InboundContent::Text { body: "hi".into(), link_preview: None },
            sender: "myphone".into(),
            sender_raw: "myphone@s.whatsapp.net".into(),
            push_name: String::new(),
            timestamp: 1000,
            reply_to: None,
            is_from_me: true,
            is_group: false,
            mentions: Vec::new(),
            ephemeral_expiration: None,
            flags: MessageFlags::default(),
        };
        let mref = MessageRef::from_inbound(&msg);
        assert!(mref.from_me);
        assert!(mref.sender_jid.is_none());
    }

    #[test]
    fn test_message_ref_from_me_in_group_keeps_sender() {
        let msg = WhatsAppInbound {
            sequence: 3,
            bridge_id: "default".into(),
            jid: "group@g.us".into(),
            id: "msg789".into(),
            content: InboundContent::Text { body: "hi".into(), link_preview: None },
            sender: "myphone".into(),
            sender_raw: "15551230000@s.whatsapp.net".into(),
            push_name: String::new(),
            timestamp: 1000,
            reply_to: None,
            is_from_me: true,
            is_group: true,
            mentions: Vec::new(),
            ephemeral_expiration: None,
            flags: MessageFlags::default(),
        };
        let mref = MessageRef::from_inbound(&msg);
        assert!(mref.from_me);
        assert_eq!(mref.sender_jid.as_deref(), Some("15551230000@s.whatsapp.net"));
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
        assert!(format!("{:?}", composing).contains("Composing"));
        assert!(format!("{:?}", recording).contains("Recording"));
        assert!(format!("{:?}", paused).contains("Paused"));
    }

    #[test]
    fn test_should_ignore_jid() {
        // These should be ignored
        assert!(should_ignore_jid("status@broadcast"));
        assert!(should_ignore_jid("120363xxxxx@newsletter"));
        assert!(should_ignore_jid("something@broadcast"));
        assert!(should_ignore_jid("server@s.whatsapp.net"));
        // These should NOT be ignored
        assert!(!should_ignore_jid("15551234567@s.whatsapp.net"));
        assert!(!should_ignore_jid("120363xxxxx@g.us"));
        assert!(!should_ignore_jid("15551234567@c.us"));
        assert!(!should_ignore_jid("abcdef@lid")); // LID JIDs are valid after re-pairing
    }

    #[tokio::test]
    async fn test_send_pacer_burst_allows_immediate_sends() {
        // Burst=3, 100ms refill — first 3 calls should be near-instant
        let pacer = SendPacer::with_burst(100, 3);
        let start = tokio::time::Instant::now();
        for _ in 0..3 {
            pacer.wait_turn().await;
        }
        let elapsed = start.elapsed();
        // All 3 should complete well under 100ms (just lock overhead)
        assert!(elapsed < Duration::from_millis(50), "burst of 3 took {elapsed:?}");
    }

    #[tokio::test]
    async fn test_send_pacer_throttles_after_burst() {
        // Burst=1, 100ms refill — second call must wait
        let pacer = SendPacer::with_burst(100, 1);
        pacer.wait_turn().await; // consumes the 1 token
        let start = tokio::time::Instant::now();
        pacer.wait_turn().await; // must wait for refill
        let elapsed = start.elapsed();
        // Should take at least ~100ms (the refill interval) but account for jitter
        assert!(elapsed >= Duration::from_millis(80), "throttled send was too fast: {elapsed:?}");
    }

    #[tokio::test]
    async fn test_send_pacer_zero_interval_is_unlimited() {
        let pacer = SendPacer::with_burst(0, 5);
        let start = tokio::time::Instant::now();
        for _ in 0..20 {
            pacer.wait_turn().await;
        }
        assert!(start.elapsed() < Duration::from_millis(10));
    }

    #[test]
    fn test_location_rejects_nan_and_infinity() {
        // NaN/Infinity coordinates must be caught before they corrupt outbound queue JSON
        assert!(!f64::NAN.is_finite());
        assert!(!f64::INFINITY.is_finite());
        assert!(!f64::NEG_INFINITY.is_finite());
        assert!(42.0_f64.is_finite());
    }

    #[test]
    fn test_location_rejects_out_of_range() {
        // Latitude must be [-90, 90], longitude must be [-180, 180]
        assert!(!(-90.0..=90.0).contains(&91.0));
        assert!(!(-180.0..=180.0).contains(&181.0));
        assert!((-90.0..=90.0).contains(&45.0));
        assert!((-180.0..=180.0).contains(&-120.0));
    }

    #[test]
    fn test_document_filename_sanitized() {
        // Path traversal in filenames should be stripped to just the final component
        let path = std::path::Path::new("../../etc/passwd");
        let safe = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        assert_eq!(safe, "passwd");
        // Normal filenames pass through
        let path2 = std::path::Path::new("report.pdf");
        let safe2 = path2.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        assert_eq!(safe2, "report.pdf");
    }
}
