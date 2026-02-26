//! WhatsApp bridge — connects to WhatsApp Web via whatsapp-rust (wa-rs).
//!
//! Adapted from ZeroClaw's whatsapp_web.rs but leaner:
//!   - Channel-based architecture (mpsc inbound/outbound)
//!   - Reconnection with exponential backoff
//!   - QR + pair-code pairing
//!   - LID-to-phone sender normalization
//!   - Expanded event handling (ban, outdated, stream errors)

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::Mutex as ParkingMutex;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use wacore::store::traits::ProtocolStore;
use wacore_binary::jid::Jid;
use waproto::whatsapp as wa;

use whatsapp_rust::bot::Bot;
use whatsapp_rust::types::events::Event;
use whatsapp_rust::Client;
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust::download::{Downloadable, MediaType};
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

// ---------------------------------------------------------------------------
// Dedup cache — bounded ring-buffer set to prevent double-processing
// ---------------------------------------------------------------------------

/// A bounded set that evicts the oldest entry when full.
/// Uses a VecDeque as a ring buffer + HashSet for O(1) lookup.
struct DedupCache {
    order: VecDeque<String>,
    seen: HashSet<String>,
    capacity: usize,
}

impl DedupCache {
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::with_capacity(capacity),
            seen: HashSet::with_capacity(capacity),
            capacity,
        }
    }

    /// Returns true if the ID was already seen (check only, does not insert).
    fn is_seen(&self, id: &str) -> bool {
        self.seen.contains(id)
    }

    /// Insert an ID into the dedup set (evicts oldest if full).
    fn insert(&mut self, id: &str) {
        if self.seen.contains(id) {
            return;
        }
        if self.order.len() >= self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        let owned = id.to_string();
        self.seen.insert(owned.clone());
        self.order.push_back(owned);
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeState {
    Disconnected,
    Pairing,
    Connected,
    Reconnecting,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionAction {
    Retry,
    Stop,
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
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields consumed by bot integrations, not the REPL
pub enum InboundContent {
    /// Plain text or extended text message.
    Text {
        body: String,
    },
    /// Image with downloaded bytes.
    Image {
        data: Vec<u8>,
        mime: String,
        caption: Option<String>,
    },
    /// Audio (voice note or file) with downloaded bytes.
    Audio {
        data: Vec<u8>,
        mime: String,
        seconds: Option<u32>,
        is_voice: bool,
    },
    /// Video with downloaded bytes.
    Video {
        data: Vec<u8>,
        mime: String,
        caption: Option<String>,
    },
    /// Document/file with downloaded bytes.
    Document {
        data: Vec<u8>,
        mime: String,
        filename: String,
    },
    /// Sticker with downloaded bytes.
    Sticker {
        data: Vec<u8>,
        mime: String,
        is_animated: bool,
    },
    /// Location pin.
    Location {
        lat: f64,
        lon: f64,
        name: Option<String>,
        address: Option<String>,
    },
    /// Contact card (vCard).
    Contact {
        display_name: String,
        vcard: String,
    },
    /// Emoji reaction on a message.
    Reaction {
        target_id: String,
        emoji: String,
    },
    /// Someone edited their message.
    Edit {
        target_id: String,
        new_text: String,
    },
    /// Someone revoked (deleted) a message.
    Revoke {
        target_id: String,
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
            Self::Reaction { .. } => "reaction",
            Self::Edit { .. } => "edit",
            Self::Revoke { .. } => "revoke",
        }
    }

    /// Extract a human-readable summary for display.
    pub fn display_text(&self) -> String {
        match self {
            Self::Text { body } => body.clone(),
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
            Self::Document { filename, data, .. } => {
                let size = format_size(data.len());
                format!("[doc: {filename} {size}]")
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
            Self::Reaction { emoji, target_id } => format!("[react {emoji} on {target_id}]"),
            Self::Edit { target_id, new_text } => format!("[edit {target_id}] {new_text}"),
            Self::Revoke { target_id } => format!("[deleted {target_id}]"),
        }
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

#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields consumed by bot integrations, not the REPL
pub struct WhatsAppInbound {
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
    /// Unix timestamp
    pub timestamp: i64,
    /// If this message is a reply, the ID of the quoted message
    pub reply_to: Option<String>,
    /// Whether this is from our own account
    pub is_from_me: bool,
}

#[derive(Debug, Clone)]
pub struct WhatsAppOutbound {
    pub jid: String,
    pub text: String,
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
    /// Minimum delay before sending a read receipt (anti-ban pacing, millis).
    pub read_receipt_delay_min_ms: u64,
    /// Maximum delay before sending a read receipt (anti-ban pacing, millis).
    pub read_receipt_delay_max_ms: u64,
    /// Minimum interval between outbound sends (anti-ban pacing, millis).
    pub min_send_interval_ms: u64,
    /// TCP port for the health endpoint (0 = disabled).
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
            read_receipt_delay_min_ms: 500,
            read_receipt_delay_max_ms: 3000,
            min_send_interval_ms: 400,
            health_port: 0,
            drain_timeout_secs: 10,
            backup_dir: None,
            backup_interval_secs: 6 * 3600,
            prune_interval_secs: 3600,
            max_outbound_retries: 3,
        }
    }
}

// ---------------------------------------------------------------------------
// WhatsAppBridge — public handle for the consumer
// ---------------------------------------------------------------------------

pub struct WhatsAppBridge {
    #[allow(dead_code)] // Used by bot integrations via the outbound channel
    outbound_tx: mpsc::Sender<WhatsAppOutbound>,
    state_rx: watch::Receiver<BridgeState>,
    #[allow(dead_code)] // Used by agent integrations via subscribe_qr()
    qr_rx: watch::Receiver<Option<String>>,
    cancel: CancellationToken,
    client_handle: Arc<ParkingMutex<Option<Arc<Client>>>>,
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
        let (outbound_tx, outbound_rx) = mpsc::channel::<WhatsAppOutbound>(256);
        let client_handle: Arc<ParkingMutex<Option<Arc<Client>>>> =
            Arc::new(ParkingMutex::new(None));

        let cancel_clone = cancel.clone();
        let ch = client_handle.clone();
        tokio::spawn(async move {
            if let Err(e) =
                run_bridge(config, inbound_tx, outbound_rx, state_tx, qr_tx, cancel_clone, ch).await
            {
                error!(error = %e, "WhatsApp bridge exited with error");
            }
        });

        Self {
            outbound_tx,
            state_rx,
            qr_rx,
            cancel,
            client_handle,
        }
    }

    #[allow(dead_code)] // Used by bot integrations via the outbound channel
    pub async fn send_message(&self, jid: &str, text: &str) -> Result<()> {
        self.outbound_tx
            .send(WhatsAppOutbound {
                jid: jid.to_string(),
                text: text.to_string(),
            })
            .await
            .context("outbound channel closed")
    }

    pub fn state(&self) -> BridgeState {
        *self.state_rx.borrow()
    }

    /// Subscribe to bridge state changes.
    pub fn subscribe_state(&self) -> watch::Receiver<BridgeState> {
        self.state_rx.clone()
    }

    pub fn stop(&self) {
        self.cancel.cancel();
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

    /// Subscribe to QR code events. Returns `Some(data)` when a QR code is available
    /// for scanning, `None` when pairing completes or session connects.
    /// Use `crate::qr::QrRender::new(&data)` to render in any format (terminal, HTML, SVG, PNG).
    #[allow(dead_code)] // Used by agent integrations, not the REPL
    pub fn subscribe_qr(&self) -> watch::Receiver<Option<String>> {
        self.qr_rx.clone()
    }

    /// Check if the bridge has an active WhatsApp connection.
    pub fn is_connected(&self) -> bool {
        self.state() == BridgeState::Connected && get_client_handle(&self.client_handle).is_some()
    }

    /// Send an image file to a chat.
    pub async fn send_image(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        caption: Option<&str>,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let upload = client
            .upload(data, MediaType::Image)
            .await
            .context("image upload failed")?;
        let msg = wa::Message {
            image_message: Some(Box::new(wa::message::ImageMessage {
                mimetype: Some(mime.to_string()),
                caption: caption.map(|c| c.to_string()),
                url: Some(upload.url),
                direct_path: Some(upload.direct_path),
                media_key: Some(upload.media_key),
                file_enc_sha256: Some(upload.file_enc_sha256),
                file_sha256: Some(upload.file_sha256),
                file_length: Some(upload.file_length),
                ..Default::default()
            })),
            ..Default::default()
        };
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send image failed: {e}"))?;
        Ok(())
    }

    /// Send an audio file as a voice note (PTT).
    pub async fn send_audio(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        seconds: Option<u32>,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let upload = client
            .upload(data, MediaType::Audio)
            .await
            .context("audio upload failed")?;
        let msg = wa::Message {
            audio_message: Some(Box::new(wa::message::AudioMessage {
                mimetype: Some(mime.to_string()),
                ptt: Some(true),
                seconds,
                url: Some(upload.url),
                direct_path: Some(upload.direct_path),
                media_key: Some(upload.media_key),
                file_enc_sha256: Some(upload.file_enc_sha256),
                file_sha256: Some(upload.file_sha256),
                file_length: Some(upload.file_length),
                ..Default::default()
            })),
            ..Default::default()
        };
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send audio failed: {e}"))?;
        Ok(())
    }

    /// Edit a previously sent message. Requires the message ID from send_message_with_id.
    pub async fn edit_message(&self, jid: &str, message_id: &str, new_text: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let new_content = wa::Message {
            conversation: Some(new_text.to_string()),
            ..Default::default()
        };
        client
            .edit_message(target, message_id.to_string(), new_content)
            .await
            .map_err(|e| anyhow::anyhow!("edit failed: {e}"))?;
        Ok(())
    }

    /// Revoke (delete for everyone) a previously sent message.
    pub async fn revoke_message(&self, jid: &str, message_id: &str) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        client
            .revoke_message(
                target,
                message_id.to_string(),
                whatsapp_rust::RevokeType::Sender,
            )
            .await
            .map_err(|e| anyhow::anyhow!("revoke failed: {e}"))?;
        Ok(())
    }

    /// Send a text message and return the message ID (for editing/deleting).
    pub async fn send_message_with_id(&self, jid: &str, text: &str) -> Result<String> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let msg = wa::Message {
            conversation: Some(text.to_string()),
            ..Default::default()
        };
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send failed: {e}"))
    }

    /// Send a video file to a chat.
    pub async fn send_video(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        caption: Option<&str>,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let upload = client
            .upload(data, MediaType::Video)
            .await
            .context("video upload failed")?;
        let msg = wa::Message {
            video_message: Some(Box::new(wa::message::VideoMessage {
                mimetype: Some(mime.to_string()),
                caption: caption.map(|c| c.to_string()),
                url: Some(upload.url),
                direct_path: Some(upload.direct_path),
                media_key: Some(upload.media_key),
                file_enc_sha256: Some(upload.file_enc_sha256),
                file_sha256: Some(upload.file_sha256),
                file_length: Some(upload.file_length),
                ..Default::default()
            })),
            ..Default::default()
        };
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send video failed: {e}"))?;
        Ok(())
    }

    /// Send a document/file to a chat.
    pub async fn send_document(
        &self,
        jid: &str,
        data: Vec<u8>,
        mime: &str,
        filename: &str,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let upload = client
            .upload(data, MediaType::Document)
            .await
            .context("document upload failed")?;
        let msg = wa::Message {
            document_message: Some(Box::new(wa::message::DocumentMessage {
                mimetype: Some(mime.to_string()),
                file_name: Some(filename.to_string()),
                url: Some(upload.url),
                direct_path: Some(upload.direct_path),
                media_key: Some(upload.media_key),
                file_enc_sha256: Some(upload.file_enc_sha256),
                file_sha256: Some(upload.file_sha256),
                file_length: Some(upload.file_length),
                ..Default::default()
            })),
            ..Default::default()
        };
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send document failed: {e}"))?;
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
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let upload = client
            .upload(data, MediaType::Sticker)
            .await
            .context("sticker upload failed")?;
        let msg = wa::Message {
            sticker_message: Some(Box::new(wa::message::StickerMessage {
                mimetype: Some(mime.to_string()),
                is_animated: Some(is_animated),
                url: Some(upload.url),
                direct_path: Some(upload.direct_path),
                media_key: Some(upload.media_key),
                file_enc_sha256: Some(upload.file_enc_sha256),
                file_sha256: Some(upload.file_sha256),
                file_length: Some(upload.file_length),
                ..Default::default()
            })),
            ..Default::default()
        };
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send sticker failed: {e}"))?;
        Ok(())
    }

    /// Send an emoji reaction to a message.
    pub async fn send_reaction(
        &self,
        jid: &str,
        target_message_id: &str,
        emoji: &str,
        target_is_from_me: bool,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let msg = wa::Message {
            reaction_message: Some(wa::message::ReactionMessage {
                key: Some(wa::MessageKey {
                    remote_jid: Some(target.to_string()),
                    id: Some(target_message_id.to_string()),
                    from_me: Some(target_is_from_me),
                    participant: None,
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

    /// Send a location pin to a chat.
    pub async fn send_location(
        &self,
        jid: &str,
        lat: f64,
        lon: f64,
        name: Option<&str>,
        address: Option<&str>,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let msg = wa::Message {
            location_message: Some(Box::new(wa::message::LocationMessage {
                degrees_latitude: Some(lat),
                degrees_longitude: Some(lon),
                name: name.map(|n| n.to_string()),
                address: address.map(|a| a.to_string()),
                ..Default::default()
            })),
            ..Default::default()
        };
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send location failed: {e}"))?;
        Ok(())
    }

    /// Send a contact card (vCard) to a chat.
    pub async fn send_contact(
        &self,
        jid: &str,
        display_name: &str,
        vcard: &str,
    ) -> Result<()> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let msg = wa::Message {
            contact_message: Some(Box::new(wa::message::ContactMessage {
                display_name: Some(display_name.to_string()),
                vcard: Some(vcard.to_string()),
                ..Default::default()
            })),
            ..Default::default()
        };
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send contact failed: {e}"))?;
        Ok(())
    }

    /// Send a text message as a reply/quote to another message.
    pub async fn send_reply(
        &self,
        jid: &str,
        reply_to_id: &str,
        reply_to_sender: &str,
        text: &str,
    ) -> Result<String> {
        let target = parse_jid(jid)?;
        let client = get_client_handle(&self.client_handle).context("not connected")?;
        let context_info = wa::ContextInfo {
            stanza_id: Some(reply_to_id.to_string()),
            participant: Some(reply_to_sender.to_string()),
            ..Default::default()
        };
        let msg = wa::Message {
            extended_text_message: Some(Box::new(wa::message::ExtendedTextMessage {
                text: Some(text.to_string()),
                context_info: Some(Box::new(context_info)),
                ..Default::default()
            })),
            ..Default::default()
        };
        client
            .send_message(target, msg)
            .await
            .map_err(|e| anyhow::anyhow!("send reply failed: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Bridge runner — reconnection loop with exponential backoff
// ---------------------------------------------------------------------------

async fn run_bridge(
    config: BridgeConfig,
    inbound_tx: mpsc::Sender<WhatsAppInbound>,
    mut outbound_rx: mpsc::Receiver<WhatsAppOutbound>,
    state_tx: watch::Sender<BridgeState>,
    qr_tx: watch::Sender<Option<String>>,
    cancel: CancellationToken,
    client_handle: Arc<ParkingMutex<Option<Arc<Client>>>>,
) -> Result<()> {
    info!("WhatsApp bridge starting");

    // Open our lean rusqlite storage backend
    let store = Store::new(&config.db_path).context("failed to open WhatsApp storage database")?;

    // DB integrity check on startup
    {
        let store_check = store.clone();
        let check_result = tokio::task::spawn_blocking(move || {
            let conn = store_check.conn_for_check();
            conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
        })
        .await
        .context("integrity check task panicked")?;
        match check_result {
            Ok(ref result) if result == "ok" => {
                info!("database integrity check passed");
            }
            Ok(result) => {
                warn!(result = %result, "database integrity check returned warnings");
            }
            Err(e) => {
                error!(error = %e, "database integrity check failed");
            }
        }
    }

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
    let dedup = Arc::new(ParkingMutex::new(DedupCache::new(DEDUP_CACHE_CAPACITY)));

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

    // Spawn health endpoint
    if config.health_port > 0 {
        let health_state_rx = state_tx.subscribe();
        let health_store = store.clone();
        let health_cancel = cancel.clone();
        tokio::spawn(async move {
            spawn_health_server(config.health_port, health_state_rx, health_store, health_cancel).await;
        });
        info!(port = config.health_port, "health endpoint started");
    }

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
                        match prune_store.prune_old_data(86400).await {
                            Ok(stats) => {
                                if stats.sent_deleted > 0 {
                                    info!(sent = stats.sent_deleted, "database pruned");
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
            &mut outbound_rx,
            &state_tx,
            &qr_tx,
            &cancel,
            &client_handle,
            &dedup,
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
            Ok(SessionAction::Retry) => {
                info!("bot session ended; reconnecting");
                backoff = Duration::from_secs(1); // reset on clean exit
            }
            Err(e) => {
                error!(error = %e, "bot session failed");
            }
        }

        // Exponential backoff with jitter before reconnect (clamped to max)
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

    let _ = state_tx.send(BridgeState::Stopped);
    info!("WhatsApp bridge stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Single bot session
// ---------------------------------------------------------------------------

async fn run_bot_session(
    store: &Store,
    config: &BridgeConfig,
    inbound_tx: &mpsc::Sender<WhatsAppInbound>,
    outbound_rx: &mut mpsc::Receiver<WhatsAppOutbound>,
    state_tx: &watch::Sender<BridgeState>,
    qr_tx: &watch::Sender<Option<String>>,
    cancel: &CancellationToken,
    client_handle: &Arc<ParkingMutex<Option<Arc<Client>>>>,
    dedup: &Arc<ParkingMutex<DedupCache>>,
) -> Result<SessionAction> {
    // Clear stale handle from previous session
    set_client_handle(client_handle, None);
    let stop_reconnect = Arc::new(AtomicBool::new(false));

    // Clone store for LID resolution in event handler
    let store_for_events = store.clone();

    // Clones for the event closure
    let ch = client_handle.clone();
    let itx = inbound_tx.clone();
    let stx = Arc::new(state_tx.clone());
    let sr = stop_reconnect.clone();
    let auto_mark_read = config.auto_mark_read;
    let allowed_numbers = config.allowed_numbers.clone();
    let dedup_for_events = dedup.clone();
    let bridge_id = config.bridge_id.clone();
    let dl_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_DOWNLOADS));
    let rr_delay_min = config.read_receipt_delay_min_ms;
    let rr_delay_max = config.read_receipt_delay_max_ms;
    let qtx = Arc::new(qr_tx.clone());

    // Build the Bot
    let mut builder = Bot::builder()
        .with_backend(Arc::new(store.clone()))
        .with_transport_factory(TokioWebSocketTransportFactory::new())
        .with_http_client(UreqHttpClient::new())
        .on_event(move |event, client| {
            let ch = ch.clone();
            let itx = itx.clone();
            let stx = stx.clone();
            let qtx = qtx.clone();
            let store = store_for_events.clone();
            let sr = sr.clone();
            let allowed = allowed_numbers.clone();
            let dedup = dedup_for_events.clone();
            let bid = bridge_id.clone();
            let dl_sem = dl_semaphore.clone();
            async move {
                handle_event(event, client, &ch, &itx, &stx, &qtx, &store, &sr, auto_mark_read, &allowed, &dedup, &bid, &dl_sem, rr_delay_min, rr_delay_max)
                    .await;
            }
        });

    // Skip history sync to avoid the "deaf client" bug (Issue #125)
    if config.skip_history_sync {
        // NOTE: Uncomment when skip_history_sync() is available in pinned commit (PR #277)
        // builder = builder.skip_history_sync();
    }

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
                    if stop_reconnect.load(Ordering::Relaxed) {
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
        _ = handle_outbound(outbound_rx, &client_handle, cancel, config.min_send_interval_ms, store, config.max_outbound_retries) => {
            if cancel.is_cancelled() || stop_reconnect.load(Ordering::Relaxed) {
                Ok(SessionAction::Stop)
            } else {
                Ok(SessionAction::Retry)
            }
        }
        _ = cancel.cancelled() => {
            info!("bridge shutdown requested — draining in-flight operations");
            let drain_timeout = Duration::from_secs(config.drain_timeout_secs);

            // Drain: persist any remaining channel messages to SQLite, then attempt to send inflight
            let drain_result = tokio::time::timeout(drain_timeout, async {
                // Persist remaining channel messages to SQLite queue
                while let Ok(out) = outbound_rx.try_recv() {
                    if let Err(e) = store.enqueue_outbound(&out.jid, &out.text).await {
                        warn!(error = %e, "failed to persist outbound message during shutdown");
                    }
                }

                // Try to send any inflight messages if we still have a connection
                if let Some(client) = get_client_handle(&client_handle) {
                    let mut sent = 0u32;
                    while let Ok(Some(row)) = store.claim_next_outbound().await {
                        let jid = match parse_jid(&row.jid) {
                            Ok(j) => j,
                            Err(_) => {
                                let _ = store.mark_outbound_failed(row.id, config.max_outbound_retries).await;
                                continue;
                            }
                        };
                        let msg = wa::Message {
                            conversation: Some(row.payload.clone()),
                            ..Default::default()
                        };
                        match client.send_message(jid, msg).await {
                            Ok(_) => {
                                let _ = store.mark_outbound_sent(row.id).await;
                                sent += 1;
                            }
                            Err(_) => {
                                // Put back for next session
                                let _ = store.mark_outbound_failed(row.id, config.max_outbound_retries).await;
                                break;
                            }
                        }
                    }
                    if sent > 0 {
                        info!(count = sent, "drained outbound messages during shutdown");
                    }

                    client.disconnect().await;
                }
            })
            .await;

            if drain_result.is_err() {
                warn!(timeout_secs = config.drain_timeout_secs, "shutdown drain timed out");
                if let Some(client) = get_client_handle(&client_handle) {
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

async fn handle_event(
    event: Event,
    client: Arc<Client>,
    client_handle: &Arc<ParkingMutex<Option<Arc<Client>>>>,
    inbound_tx: &mpsc::Sender<WhatsAppInbound>,
    state_tx: &Arc<watch::Sender<BridgeState>>,
    qr_tx: &Arc<watch::Sender<Option<String>>>,
    store: &Store,
    stop_reconnect: &Arc<AtomicBool>,
    auto_mark_read: bool,
    allowed_numbers: &[String],
    dedup: &Arc<ParkingMutex<DedupCache>>,
    bridge_id: &str,
    dl_semaphore: &Semaphore,
    rr_delay_min_ms: u64,
    rr_delay_max_ms: u64,
) {
    match event {
        Event::PairingQrCode { code, .. } => {
            // Publish raw QR data for agent integrations
            let _ = qr_tx.send(Some(code.clone()));

            // Render compact half-block QR in terminal
            if let Some(qr) = crate::qr::QrRender::new(&code) {
                print!("{}", qr.terminal());

                // Save PNG fallback to temp dir
                let png_path = std::env::temp_dir().join("picoclaw_qr.png");
                match qr.save_png(&png_path, 8) {
                    Ok(()) => info!(path = %png_path.display(), "QR code PNG saved"),
                    Err(e) => debug!(error = %e, "failed to save QR PNG fallback"),
                }

                // Save HTML fallback to temp dir
                let html_path = std::env::temp_dir().join("picoclaw_qr.html");
                match qr.save_html(&html_path) {
                    Ok(()) => info!(path = %html_path.display(), "QR code HTML saved"),
                    Err(e) => debug!(error = %e, "failed to save QR HTML fallback"),
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
            info!("WhatsApp connected");
            let _ = qr_tx.send(None); // clear QR on connection
            set_client_handle(client_handle, Some(client.clone()));
            let _ = state_tx.send(BridgeState::Connected);
        }
        Event::Message(msg, info) => {
            // Two-phase dedup: check first (don't insert yet)
            if dedup.lock().is_seen(&info.id) {
                debug!(id = %info.id, "duplicate message, skipping");
                return;
            }

            let sender_raw = info.source.sender.to_string();
            let sender = resolve_sender(&sender_raw, store).await;

            // Allowlist filter: skip messages from numbers not on the list
            if !allowed_numbers.is_empty()
                && !info.source.is_from_me
                && !allowed_numbers.iter().any(|n| n == &sender)
            {
                dedup.lock().insert(&info.id);
                debug!(sender = %sender, "message from non-allowed sender, skipping");
                return;
            }

            // Extract reply-to context from any message type
            let reply_to = extract_reply_to(&msg);

            // Try to extract content (with media size guard + download semaphore)
            let result = extract_content(&msg, &client, dl_semaphore).await;

            match result {
                ExtractResult::Content(content) => {
                    let inbound = WhatsAppInbound {
                        bridge_id: bridge_id.to_string(),
                        jid: info.source.chat.to_string(),
                        id: info.id.clone(),
                        content,
                        sender,
                        sender_raw,
                        timestamp: info.timestamp.timestamp(),
                        reply_to,
                        is_from_me: info.source.is_from_me,
                    };

                    match inbound_tx.send(inbound).await {
                        Ok(()) => {
                            // Commit to dedup only after successful enqueue
                            dedup.lock().insert(&info.id);
                        }
                        Err(e) => {
                            // Channel closed — do NOT insert into dedup so message isn't lost
                            warn!(error = %e, "inbound channel closed, message NOT deduped");
                        }
                    }

                    // Jittered read receipt (anti-ban pacing)
                    if auto_mark_read && !info.source.is_from_me {
                        let range = rr_delay_max_ms.saturating_sub(rr_delay_min_ms).max(1);
                        let jitter = rand_jitter_ms(range * 2) % range; // full [0, range) distribution
                        let delay = Duration::from_millis(rr_delay_min_ms + jitter);
                        tokio::time::sleep(delay).await;
                        let group_sender = if info.source.is_group {
                            Some(&info.source.sender)
                        } else {
                            None
                        };
                        if let Err(e) = client
                            .mark_as_read(&info.source.chat, group_sender, vec![info.id.clone()])
                            .await
                        {
                            warn!(
                                error = %e,
                                chat = %info.source.chat,
                                "failed to send read receipt"
                            );
                        }
                    }
                }
                ExtractResult::Unhandled => {
                    // Unknown type — dedup immediately (no point retrying)
                    dedup.lock().insert(&info.id);
                    debug!(
                        message_kind = message_kind(&msg),
                        chat = %info.source.chat,
                        sender = %sender,
                        "unhandled inbound message type"
                    );
                }
                ExtractResult::TransientFailure => {
                    // Transient failure — do NOT dedup so the message can be retried on redeliver
                    warn!(
                        id = %info.id,
                        chat = %info.source.chat,
                        "transient extraction failure, will retry on redeliver"
                    );
                }
            }
        }
        Event::LoggedOut(reason) => {
            warn!(reason = ?reason.reason, on_connect = reason.on_connect, "WhatsApp logged out");
            set_client_handle(client_handle, None);
            stop_reconnect.store(true, Ordering::Relaxed);
            request_disconnect(client.clone());
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::StreamError(stream_err) => {
            error!(code = %stream_err.code, "WhatsApp stream error");
            set_client_handle(client_handle, None);
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::StreamReplaced(_) => {
            error!("StreamReplaced: another client connected using this same linked device session — stopping to avoid replacement loop. Check for duplicate bridge processes (ps aux | grep picoclaw) or a competing WhatsApp Web/Desktop session.");
            set_client_handle(client_handle, None);
            stop_reconnect.store(true, Ordering::Relaxed);
            request_disconnect(client.clone());
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::TemporaryBan(ban) => {
            error!(?ban.code, ban_expires_secs = ban.expire.num_seconds(), "temporarily banned by WhatsApp");
            set_client_handle(client_handle, None);
            stop_reconnect.store(true, Ordering::Relaxed);
            request_disconnect(client.clone());
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::ClientOutdated(_) => {
            error!("WhatsApp client version outdated — update required");
            set_client_handle(client_handle, None);
            stop_reconnect.store(true, Ordering::Relaxed);
            request_disconnect(client.clone());
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::ConnectFailure(failure) => {
            error!(reason = ?failure.reason, message = %failure.message, "WhatsApp connect failure");
            set_client_handle(client_handle, None);
            if !failure.reason.should_reconnect() {
                stop_reconnect.store(true, Ordering::Relaxed);
                request_disconnect(client.clone());
            }
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
            info!(
                receipt_type = ?receipt.r#type,
                message_ids = ?receipt.message_ids,
                chat = %receipt.source.chat,
                sender = %receipt.source.sender,
                "receipt"
            );
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
            let _ = state_tx.send(BridgeState::Disconnected);
        }
        Event::PairSuccess(_) => {
            info!("pair code accepted — device paired successfully");
        }
        Event::PairError(err) => {
            error!(?err, "pairing failed");
        }
        _ => {
            debug!(?event, "unhandled event");
        }
    }
}

// ---------------------------------------------------------------------------
// Inbound content extraction
// ---------------------------------------------------------------------------

/// Extract the stanza_id of a quoted/replied-to message, if present.
fn extract_reply_to(msg: &wa::Message) -> Option<String> {
    // Check extended_text_message context
    if let Some(ref ext) = msg.extended_text_message {
        if let Some(ref ctx) = ext.context_info {
            if ctx.stanza_id.is_some() {
                return ctx.stanza_id.clone();
            }
        }
    }
    // Check image_message context
    if let Some(ref img) = msg.image_message {
        if let Some(ref ctx) = img.context_info {
            if ctx.stanza_id.is_some() {
                return ctx.stanza_id.clone();
            }
        }
    }
    // Check video_message context
    if let Some(ref vid) = msg.video_message {
        if let Some(ref ctx) = vid.context_info {
            if ctx.stanza_id.is_some() {
                return ctx.stanza_id.clone();
            }
        }
    }
    // Check audio_message context
    if let Some(ref aud) = msg.audio_message {
        if let Some(ref ctx) = aud.context_info {
            if ctx.stanza_id.is_some() {
                return ctx.stanza_id.clone();
            }
        }
    }
    // Check document_message context
    if let Some(ref doc) = msg.document_message {
        if let Some(ref ctx) = doc.context_info {
            if ctx.stanza_id.is_some() {
                return ctx.stanza_id.clone();
            }
        }
    }
    None
}

/// Maximum recursion depth for unwrapping ephemeral/view-once wrappers.
const MAX_EXTRACT_DEPTH: u8 = 3;

/// Try to extract structured content from an inbound message.
async fn extract_content(msg: &wa::Message, client: &Arc<Client>, dl_semaphore: &Semaphore) -> ExtractResult {
    extract_content_inner(msg, client, dl_semaphore, 0).await
}

/// Inner recursive extractor with depth limit.
async fn extract_content_inner(
    msg: &wa::Message,
    client: &Arc<Client>,
    dl_semaphore: &Semaphore,
    depth: u8,
) -> ExtractResult {
    if depth > MAX_EXTRACT_DEPTH {
        warn!(depth, "extract_content recursion limit reached, skipping");
        return ExtractResult::Unhandled;
    }
    // --- Text ---
    if let Some(ref text) = msg.conversation {
        return ExtractResult::Content(InboundContent::Text {
            body: text.clone(),
        });
    }
    if let Some(ref ext) = msg.extended_text_message {
        if let Some(ref text) = ext.text {
            return ExtractResult::Content(InboundContent::Text {
                body: text.clone(),
            });
        }
    }

    // --- Image ---
    if let Some(ref img) = msg.image_message {
        if let Some(len) = img.file_length {
            if len > MAX_MEDIA_BYTES { warn!(size = len, "image exceeds size limit, skipping"); return ExtractResult::Unhandled; }
        }
        let caption = img.caption.clone();
        let mime = img.mimetype.clone().unwrap_or_else(|| "image/jpeg".to_string());
        let _permit = dl_semaphore.acquire().await.expect("semaphore closed");
        match client.download(img.as_ref() as &dyn Downloadable).await {
            Ok(data) => {
                if data.len() as u64 > MAX_MEDIA_BYTES { warn!(size = data.len(), "downloaded image exceeds size limit"); return ExtractResult::Unhandled; }
                return ExtractResult::Content(InboundContent::Image { data, mime, caption });
            }
            Err(e) => {
                warn!(error = %e, "failed to download image");
                return ExtractResult::TransientFailure;
            }
        }
    }

    // --- Video (includes PTV/video notes) ---
    if let Some(ref vid) = msg.video_message {
        if let Some(len) = vid.file_length {
            if len > MAX_MEDIA_BYTES { warn!(size = len, "video exceeds size limit, skipping"); return ExtractResult::Unhandled; }
        }
        let caption = vid.caption.clone();
        let mime = vid.mimetype.clone().unwrap_or_else(|| "video/mp4".to_string());
        let _permit = dl_semaphore.acquire().await.expect("semaphore closed");
        match client.download(vid.as_ref() as &dyn Downloadable).await {
            Ok(data) => {
                if data.len() as u64 > MAX_MEDIA_BYTES { warn!(size = data.len(), "downloaded video exceeds size limit"); return ExtractResult::Unhandled; }
                return ExtractResult::Content(InboundContent::Video { data, mime, caption });
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
                return ExtractResult::Content(InboundContent::Audio { data, mime, seconds, is_voice });
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
        let _permit = dl_semaphore.acquire().await.expect("semaphore closed");
        match client.download(doc.as_ref() as &dyn Downloadable).await {
            Ok(data) => {
                if data.len() as u64 > MAX_MEDIA_BYTES { warn!(size = data.len(), "downloaded document exceeds size limit"); return ExtractResult::Unhandled; }
                return ExtractResult::Content(InboundContent::Document { data, mime, filename });
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
                let _permit = dl_semaphore.acquire().await.expect("semaphore closed");
                match client.download(doc.as_ref() as &dyn Downloadable).await {
                    Ok(data) => {
                        if data.len() as u64 > MAX_MEDIA_BYTES { warn!(size = data.len(), "downloaded document exceeds size limit"); return ExtractResult::Unhandled; }
                        return ExtractResult::Content(InboundContent::Document { data, mime, filename });
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
        let _permit = dl_semaphore.acquire().await.expect("semaphore closed");
        match client.download(stk.as_ref() as &dyn Downloadable).await {
            Ok(data) => {
                if data.len() as u64 > MAX_MEDIA_BYTES { warn!(size = data.len(), "downloaded sticker exceeds size limit"); return ExtractResult::Unhandled; }
                return ExtractResult::Content(InboundContent::Sticker { data, mime, is_animated });
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
            return ExtractResult::Content(InboundContent::Location {
                lat,
                lon,
                name: loc.name.clone(),
                address: loc.address.clone(),
            });
        }
    }

    // --- Live location (bridge as static snapshot) ---
    if let Some(ref loc) = msg.live_location_message {
        if let (Some(lat), Some(lon)) = (loc.degrees_latitude, loc.degrees_longitude) {
            return ExtractResult::Content(InboundContent::Location {
                lat,
                lon,
                name: loc.caption.clone(),
                address: None,
            });
        }
    }

    // --- Contact ---
    if let Some(ref contact) = msg.contact_message {
        return ExtractResult::Content(InboundContent::Contact {
            display_name: contact.display_name.clone().unwrap_or_default(),
            vcard: contact.vcard.clone().unwrap_or_default(),
        });
    }

    // --- Multiple contacts ---
    if let Some(ref arr) = msg.contacts_array_message {
        // Bridge as first contact (most common case is single)
        if let Some(first) = arr.contacts.first() {
            return ExtractResult::Content(InboundContent::Contact {
                display_name: first.display_name.clone().unwrap_or_else(|| {
                    arr.display_name.clone().unwrap_or_default()
                }),
                vcard: first.vcard.clone().unwrap_or_default(),
            });
        }
    }

    // --- Reaction ---
    if let Some(ref reaction) = msg.reaction_message {
        if let Some(ref key) = reaction.key {
            return ExtractResult::Content(InboundContent::Reaction {
                target_id: key.id.clone().unwrap_or_default(),
                emoji: reaction.text.clone().unwrap_or_default(),
            });
        }
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
                return ExtractResult::Content(InboundContent::Edit { target_id, new_text });
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

    // --- Ephemeral wrapper (unwrap and recurse) ---
    if let Some(ref eph) = msg.ephemeral_message {
        if let Some(ref inner) = eph.message {
            return Box::pin(extract_content_inner(inner, client, dl_semaphore, depth + 1)).await;
        }
    }

    // --- View-once wrapper (unwrap and recurse) ---
    if let Some(ref vo) = msg.view_once_message {
        if let Some(ref inner) = vo.message {
            return Box::pin(extract_content_inner(inner, client, dl_semaphore, depth + 1)).await;
        }
    }
    if let Some(ref vo2) = msg.view_once_message_v2 {
        if let Some(ref inner) = vo2.message {
            return Box::pin(extract_content_inner(inner, client, dl_semaphore, depth + 1)).await;
        }
    }

    ExtractResult::Unhandled
}

// ---------------------------------------------------------------------------
// LID-to-phone sender resolution
// ---------------------------------------------------------------------------

/// Resolve a sender JID to a phone number using the LID mapping table.
/// Falls back to the raw JID string if no mapping exists.
async fn resolve_sender(sender_raw: &str, store: &Store) -> String {
    // Extract the user part (before @)
    let user_part = sender_raw
        .split('@')
        .next()
        .unwrap_or(sender_raw)
        .to_string();

    // Try LID → phone lookup
    if let Ok(Some(entry)) = store.get_lid_mapping(&user_part).await {
        return entry.phone_number;
    }

    // Already a phone number or no mapping — return as-is
    user_part
}

// ---------------------------------------------------------------------------
// Outbound message handling
// ---------------------------------------------------------------------------

async fn handle_outbound(
    outbound_rx: &mut mpsc::Receiver<WhatsAppOutbound>,
    client_handle: &Arc<ParkingMutex<Option<Arc<Client>>>>,
    cancel: &CancellationToken,
    min_send_interval_ms: u64,
    store: &Store,
    max_retries: i32,
) {
    let mut outbound_closed = false;
    let mut last_send = tokio::time::Instant::now() - Duration::from_secs(60);

    loop {
        if cancel.is_cancelled() {
            break;
        }

        // Drain channel into SQLite queue (persist immediately for crash safety)
        while !outbound_closed {
            match outbound_rx.try_recv() {
                Ok(out) => {
                    if let Err(e) = store.enqueue_outbound(&out.jid, &out.text).await {
                        error!(error = %e, jid = %out.jid, "failed to enqueue outbound message");
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    outbound_closed = true;
                    break;
                }
            }
        }

        // Try to claim and send the next queued message
        let row = match store.claim_next_outbound().await {
            Ok(Some(row)) => row,
            Ok(None) => {
                // Queue empty — wait for new messages or cancellation
                if outbound_closed {
                    break;
                }
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    msg = outbound_rx.recv() => {
                        match msg {
                            Some(out) => {
                                if let Err(e) = store.enqueue_outbound(&out.jid, &out.text).await {
                                    error!(error = %e, "failed to enqueue outbound message");
                                }
                            }
                            None => outbound_closed = true,
                        }
                    }
                }
                continue;
            }
            Err(e) => {
                warn!(error = %e, "failed to claim outbound message from queue");
                tokio::select! {
                    _ = tokio::time::sleep(OUTBOUND_RETRY_DELAY) => {}
                    _ = cancel.cancelled() => break,
                }
                continue;
            }
        };

        // Wait for a connected client — do NOT burn retries for "no connection"
        let Some(client) = get_client_handle(client_handle) else {
            // Requeue without incrementing retries (connection issue, not send failure)
            let _ = store.requeue_outbound(row.id).await;
            tokio::select! {
                _ = tokio::time::sleep(OUTBOUND_RETRY_DELAY) => {}
                _ = cancel.cancelled() => break,
            }
            continue;
        };

        let jid = match parse_jid(&row.jid) {
            Ok(j) => j,
            Err(e) => {
                error!(error = %e, jid = %row.jid, "invalid outbound JID, marking failed");
                let _ = store.mark_outbound_failed(row.id, 0).await; // immediate fail
                continue;
            }
        };

        // Anti-ban pacing: enforce minimum interval between sends
        let min_interval = Duration::from_millis(min_send_interval_ms);
        let elapsed = last_send.elapsed();
        if elapsed < min_interval {
            tokio::time::sleep(min_interval - elapsed).await;
        }

        let msg = wa::Message {
            conversation: Some(row.payload.clone()),
            ..Default::default()
        };

        match client.send_message(jid, msg).await {
            Ok(_msg_id) => {
                last_send = tokio::time::Instant::now();
                let _ = store.mark_outbound_sent(row.id).await;
            }
            Err(e) => {
                if row.retries + 1 >= max_retries {
                    error!(
                        error = %e, jid = %row.jid, retries = row.retries + 1,
                        "dropping outbound message after max retries"
                    );
                } else {
                    warn!(
                        error = %e, jid = %row.jid, retry = row.retries + 1,
                        "failed to send outbound message; will retry"
                    );
                }
                let _ = store.mark_outbound_failed(row.id, max_retries).await;
                tokio::select! {
                    _ = tokio::time::sleep(OUTBOUND_RETRY_DELAY) => {}
                    _ = cancel.cancelled() => break,
                }
            }
        }

        if outbound_closed {
            // Check if there are remaining queued messages
            match store.outbound_queue_depth().await {
                Ok(0) => break,
                Ok(_) => continue,
                Err(_) => break,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Health endpoint — raw TCP, no framework dependencies
// ---------------------------------------------------------------------------

async fn spawn_health_server(
    port: u16,
    state_rx: watch::Receiver<BridgeState>,
    store: Store,
    cancel: CancellationToken,
) {
    let listener = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            error!(error = %e, port = port, "failed to bind health endpoint");
            return;
        }
    };

    loop {
        tokio::select! {
            result = listener.accept() => {
                let Ok((mut stream, _)) = result else { continue };

                // Read the HTTP request before responding (prevents client hangs)
                let mut buf = [0u8; 1024];
                let _ = tokio::time::timeout(
                    Duration::from_secs(2),
                    tokio::io::AsyncReadExt::read(&mut stream, &mut buf),
                ).await;

                let state = *state_rx.borrow();
                let connected = state == BridgeState::Connected;
                let queue_depth = store.outbound_queue_depth().await.unwrap_or(-1);

                let (status_line, status_str) = if connected {
                    ("HTTP/1.1 200 OK", "ok")
                } else {
                    ("HTTP/1.1 503 Service Unavailable", "disconnected")
                };

                let body = format!(
                    r#"{{"status":"{}","state":"{:?}","queue_depth":{}}}"#,
                    status_str, state, queue_depth
                );
                let response = format!(
                    "{}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                    status_line,
                    body.len(),
                    body
                );

                let _ = stream.write_all(response.as_bytes()).await;
            }
            _ = cancel.cancelled() => break,
        }
    }
}

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
    fn test_message_kind_default() {
        let msg = wa::Message::default();
        assert_eq!(message_kind(&msg), "other");
    }

    #[test]
    fn test_dedup_cache() {
        let mut cache = DedupCache::new(3);
        assert!(!cache.is_seen("a"));
        cache.insert("a");
        assert!(cache.is_seen("a")); // now seen
        cache.insert("b");
        cache.insert("c"); // cache: [a, b, c] (full)
        // Inserting "d" evicts "a"
        cache.insert("d"); // cache: [b, c, d]
        assert!(!cache.is_seen("a")); // "a" was evicted
        cache.insert("a"); // cache: [c, d, a]
        assert!(cache.is_seen("d")); // "d" still in cache
        assert!(cache.is_seen("c")); // "c" still in cache
        // Re-inserting existing key is a no-op (no double eviction)
        cache.insert("d");
        assert!(cache.is_seen("c"));
        assert!(cache.is_seen("d"));
        assert!(cache.is_seen("a"));
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
}
