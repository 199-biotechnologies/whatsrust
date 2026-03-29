//! whatsrust — Pure Rust WhatsApp bridge.
//!
//! Lean replacement for the Baileys (Node.js) sidecar.
//! Uses whatsapp-rust (wa-rs) for the WhatsApp Web protocol
//! and our own rusqlite backend for Signal Protocol storage.
//!
//! Two modes:
//!   whatsrust              — daemon mode (bridge + REPL + API server)
//!   whatsrust <command>    — CLI mode (sends HTTP to running daemon, prints JSON)

mod api;
mod bridge;
mod bridge_events;
mod dedup;
mod instance_lock;
mod outbound;
mod polls;
pub mod qr;
mod read_receipts;
mod storage;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use bridge::{BridgeConfig, WhatsAppBridge};
use whatsrust::mcp;

const MAX_LOCAL_MEDIA_READ_BYTES: u64 = 50 * 1024 * 1024;

/// Read the API port from env, with fallback chain.
fn get_port() -> u16 {
    std::env::var("WHATSRUST_PORT")
        .or_else(|_| std::env::var("HEALTH_PORT"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7270)
}

fn read_local_media_file(path: &Path) -> Result<Vec<u8>> {
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        anyhow::bail!("path is not a regular file: {}", path.display());
    }
    if meta.len() > MAX_LOCAL_MEDIA_READ_BYTES {
        anyhow::bail!(
            "file exceeds size limit ({} bytes > {} bytes): {}",
            meta.len(),
            MAX_LOCAL_MEDIA_READ_BYTES,
            path.display()
        );
    }
    Ok(std::fs::read(path)?)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // CLI mode: fire request to running daemon
    if args.len() > 1 {
        return cli_main(&args[1..]).await;
    }

    // Daemon mode
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "whatsrust=info,whatsapp_rust=info".parse().unwrap()),
        )
        .init();

    info!("whatsrust v{}", env!("CARGO_PKG_VERSION"));

    let (inbound_tx, mut inbound_rx) = mpsc::channel(256);
    let cancel = CancellationToken::new();

    // Allowed numbers: only bridge messages from these senders (empty = all)
    let allowed: Vec<String> = std::env::var("WHATSAPP_ALLOWED")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.chars().filter(|c| c.is_ascii_digit()).collect::<String>())
        .filter(|s| !s.is_empty())
        .collect();

    if !allowed.is_empty() {
        info!(allowed = ?allowed, "sender allowlist active");
    }

    let api_port = get_port();

    let backup_dir = std::env::var("BACKUP_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from("whatsapp.db.backups")));

    let send_burst: u32 = std::env::var("WHATSRUST_SEND_BURST")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let config = BridgeConfig {
        db_path: PathBuf::from("whatsapp.db"),
        pair_phone: std::env::var("WHATSAPP_PAIR_PHONE").ok(),
        allowed_numbers: allowed,
        health_port: api_port,
        backup_dir,
        send_burst,
        ..Default::default()
    };

    // Single-instance guard: prevent two bridges from using the same session
    let _instance_lock = match instance_lock::InstanceLock::acquire(
        &config.db_path,
        Duration::from_secs(2),
    ) {
        Ok(lock) => {
            info!(lock = %lock.path().display(), "session lock acquired");
            lock
        }
        Err(e) => {
            error!(error = %e, "another bridge instance is already running — refusing to start");
            std::process::exit(1);
        }
    };

    let bridge = Arc::new(WhatsAppBridge::start(config, inbound_tx, cancel.clone()));

    info!("bridge started, state: {:?}", bridge.state());
    info!("waiting for WhatsApp connection (scan QR code or enter pair code)...");

    // Spawn API server
    if api_port > 0 {
        let api_bridge = bridge.clone();
        let api_cancel = cancel.clone();
        tokio::spawn(async move {
            api::serve(api_bridge, api_port, api_cancel).await;
        });
    }

    // Print inbound messages
    let bridge_for_rx = bridge.clone();
    tokio::spawn(async move {
        while let Some(msg) = inbound_rx.recv().await {
            let reply_tag = msg
                .reply_to
                .as_deref()
                .map(|id| format!(" (reply to {id})"))
                .unwrap_or_default();
            let flags_tag = {
                let mut parts = Vec::new();
                if msg.flags.is_forwarded {
                    parts.push(format!("fwd:{}", msg.flags.forwarding_score));
                }
                if msg.flags.is_view_once {
                    parts.push("view-once".to_string());
                }
                if parts.is_empty() { String::new() } else { format!(" [{}]", parts.join(",")) }
            };
            println!(
                "\n<< [{}] {} ({}){}{}: {}",
                msg.jid,
                msg.sender,
                msg.content.kind(),
                reply_tag,
                flags_tag,
                msg.content.display_text()
            );
            if bridge_for_rx.is_connected() {
                print!("> ");
            }
        }
    });

    // Interactive REPL for testing (stdin)
    let bridge_for_repl = bridge.clone();
    let cancel_for_repl = cancel.clone();
    tokio::spawn(async move {
        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();

        println!("Commands:");
        println!("  send <jid> <message>           — send text (prints msg ID)");
        println!("  reply <jid> <id> <sender> <msg>— reply quoting a message");
        println!("  edit <jid> <id> <new text>     — edit a sent message");
        println!("  react <jid> <id> <emoji> [from_me] [sender_jid] — react to a message");
        println!("  unreact <jid> <id> [from_me] [sender_jid] — remove a reaction");
        println!("  revoke <jid> <id>              — delete a message for everyone");
        println!("  image <jid> <path>             — send an image file");
        println!("  audio <jid> <path>             — send audio as voice note");
        println!("  video <jid> <path>             — send a video file");
        println!("  doc <jid> <path>               — send a document/file");
        println!("  sticker <jid> <path>           — send a WebP sticker");
        println!("  location <jid> <lat> <lon>     — send a location pin");
        println!("  contact <jid> <name> <phone>   — send a contact card");
        println!("  forward <jid> <msg_id>         — forward a cached message (fwd)");
        println!("  vo-image <jid> <path> [cap]    — send view-once image");
        println!("  vo-video <jid> <path> [cap]    — send view-once video");
        println!("  poll <jid> <N> <Q> | opt | ... — create a poll (N = selectable count)");
        println!("  subscribe <jid>                — subscribe to contact presence");
        println!("  typing <jid>                   — show typing indicator");
        println!("  stop-typing <jid>              — cancel typing indicator");
        println!("  recording <jid>                — show recording indicator");
        println!("  stop-recording <jid>           — cancel recording indicator");
        println!("  status                         — show bridge state");
        println!("  quit                           — shut down");
        println!();

        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }

                    let parts: Vec<&str> = line.splitn(3, ' ').collect();
                    match parts[0] {
                        "send" | "s" => {
                            if parts.len() < 3 {
                                println!("usage: send <jid> <message>");
                                continue;
                            }
                            match bridge_for_repl
                                .send_message_with_id(parts[1], parts[2])
                                .await
                            {
                                Ok(id) => println!(">> sent to {} (id: {})", parts[1], id),
                                Err(e) => println!("!! send failed: {e}"),
                            }
                        }
                        "edit" | "e" => {
                            let edit_parts: Vec<&str> = line.splitn(4, ' ').collect();
                            if edit_parts.len() < 4 {
                                println!("usage: edit <jid> <msg_id> <new text>");
                                continue;
                            }
                            match bridge_for_repl
                                .edit_message(edit_parts[1], edit_parts[2], edit_parts[3])
                                .await
                            {
                                Ok(()) => println!(">> edited {}", edit_parts[2]),
                                Err(e) => println!("!! edit failed: {e}"),
                            }
                        }
                        "image" | "img" => {
                            if parts.len() < 3 {
                                println!("usage: image <jid> <path>");
                                continue;
                            }
                            let path = std::path::Path::new(parts[2]);
                            match read_local_media_file(path) {
                                Ok(data) => {
                                    let mime = match path.extension().and_then(|e| e.to_str()) {
                                        Some("png") => "image/png",
                                        Some("gif") => "image/gif",
                                        Some("webp") => "image/webp",
                                        _ => "image/jpeg",
                                    };
                                    match bridge_for_repl
                                        .send_image(parts[1], data, mime, None)
                                        .await
                                    {
                                        Ok(()) => println!(">> image sent to {}", parts[1]),
                                        Err(e) => println!("!! image send failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! cannot read file: {e}"),
                            }
                        }
                        "audio" | "voice" => {
                            if parts.len() < 3 {
                                println!("usage: audio <jid> <path>");
                                continue;
                            }
                            let path = std::path::Path::new(parts[2]);
                            match read_local_media_file(path) {
                                Ok(data) => {
                                    let mime = match path.extension().and_then(|e| e.to_str()) {
                                        Some("ogg") | Some("opus") => "audio/ogg; codecs=opus",
                                        Some("mp3") => "audio/mpeg",
                                        Some("m4a") | Some("aac") => "audio/mp4",
                                        _ => "audio/ogg; codecs=opus",
                                    };
                                    match bridge_for_repl
                                        .send_audio(parts[1], data, mime, None)
                                        .await
                                    {
                                        Ok(()) => println!(">> audio sent to {}", parts[1]),
                                        Err(e) => println!("!! audio send failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! cannot read file: {e}"),
                            }
                        }
                        "video" | "vid" => {
                            if parts.len() < 3 {
                                println!("usage: video <jid> <path>");
                                continue;
                            }
                            let path = std::path::Path::new(parts[2]);
                            match read_local_media_file(path) {
                                Ok(data) => {
                                    let mime = match path.extension().and_then(|e| e.to_str()) {
                                        Some("webm") => "video/webm",
                                        Some("mov") => "video/quicktime",
                                        Some("3gp") => "video/3gpp",
                                        _ => "video/mp4",
                                    };
                                    match bridge_for_repl
                                        .send_video(parts[1], data, mime, None)
                                        .await
                                    {
                                        Ok(()) => println!(">> video sent to {}", parts[1]),
                                        Err(e) => println!("!! video send failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! cannot read file: {e}"),
                            }
                        }
                        "doc" | "document" => {
                            if parts.len() < 3 {
                                println!("usage: doc <jid> <path>");
                                continue;
                            }
                            let path = std::path::Path::new(parts[2]);
                            let filename = path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("file");
                            match read_local_media_file(path) {
                                Ok(data) => {
                                    let mime = match path.extension().and_then(|e| e.to_str()) {
                                        Some("pdf") => "application/pdf",
                                        Some("zip") => "application/zip",
                                        Some("txt") => "text/plain",
                                        _ => "application/octet-stream",
                                    };
                                    match bridge_for_repl
                                        .send_document(parts[1], data, mime, filename)
                                        .await
                                    {
                                        Ok(()) => println!(">> doc sent to {}", parts[1]),
                                        Err(e) => println!("!! doc send failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! cannot read file: {e}"),
                            }
                        }
                        "reply" | "r" => {
                            let reply_parts: Vec<&str> = line.splitn(5, ' ').collect();
                            if reply_parts.len() < 5 {
                                println!("usage: reply <jid> <msg_id> <sender_jid> <message>");
                                continue;
                            }
                            match bridge_for_repl
                                .send_reply(
                                    reply_parts[1],
                                    reply_parts[2],
                                    reply_parts[3],
                                    reply_parts[4],
                                )
                                .await
                            {
                                Ok(id) => println!(">> replied (id: {})", id),
                                Err(e) => println!("!! reply failed: {e}"),
                            }
                        }
                        "react" => {
                            // react <jid> <msg_id> <emoji> [from_me|sender_jid] [sender_jid]
                            let react_parts: Vec<&str> = line.splitn(6, ' ').collect();
                            if react_parts.len() < 4 {
                                println!("usage: react <jid> <msg_id> <emoji> [from_me|sender_jid] [sender_jid]");
                                continue;
                            }
                            let (from_me, sender_jid) = parse_react_target(
                                react_parts.get(4).copied(),
                                react_parts.get(5).copied(),
                            );
                            match bridge_for_repl
                                .send_reaction(
                                    react_parts[1],
                                    react_parts[2],
                                    sender_jid.as_deref(),
                                    react_parts[3],
                                    from_me,
                                )
                                .await
                            {
                                Ok(()) => println!(">> reacted {} (from_me={})", react_parts[3], from_me),
                                Err(e) => println!("!! react failed: {e}"),
                            }
                        }
                        "unreact" => {
                            // unreact <jid> <msg_id> [from_me|sender_jid] [sender_jid]
                            let react_parts: Vec<&str> = line.splitn(5, ' ').collect();
                            if react_parts.len() < 3 {
                                println!("usage: unreact <jid> <msg_id> [from_me|sender_jid] [sender_jid]");
                                continue;
                            }
                            let (from_me, sender_jid) = parse_react_target(
                                react_parts.get(3).copied(),
                                react_parts.get(4).copied(),
                            );
                            match bridge_for_repl
                                .remove_reaction(react_parts[1], react_parts[2], sender_jid.as_deref(), from_me)
                                .await
                            {
                                Ok(()) => println!(">> reaction removed (from_me={})", from_me),
                                Err(e) => println!("!! unreact failed: {e}"),
                            }
                        }
                        "sticker" | "stk" => {
                            if parts.len() < 3 {
                                println!("usage: sticker <jid> <path>");
                                continue;
                            }
                            let path = std::path::Path::new(parts[2]);
                            match read_local_media_file(path) {
                                Ok(data) => {
                                    match bridge_for_repl
                                        .send_sticker(parts[1], data, "image/webp", false)
                                        .await
                                    {
                                        Ok(()) => println!(">> sticker sent to {}", parts[1]),
                                        Err(e) => println!("!! sticker send failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! cannot read file: {e}"),
                            }
                        }
                        "vo-image" => {
                            let vo_parts: Vec<&str> = line.splitn(4, ' ').collect();
                            if vo_parts.len() < 3 {
                                println!("usage: vo-image <jid> <path> [caption]");
                                continue;
                            }
                            let path = std::path::Path::new(vo_parts[2]);
                            match read_local_media_file(path) {
                                Ok(data) => {
                                    let mime = match path.extension().and_then(|e| e.to_str()) {
                                        Some("png") => "image/png",
                                        Some("gif") => "image/gif",
                                        Some("webp") => "image/webp",
                                        _ => "image/jpeg",
                                    };
                                    let caption = vo_parts.get(3).copied();
                                    match bridge_for_repl
                                        .send_view_once_image(vo_parts[1], data, mime, caption)
                                        .await
                                    {
                                        Ok(()) => println!(">> view-once image sent to {}", vo_parts[1]),
                                        Err(e) => println!("!! vo-image failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! cannot read file: {e}"),
                            }
                        }
                        "vo-video" => {
                            let vo_parts: Vec<&str> = line.splitn(4, ' ').collect();
                            if vo_parts.len() < 3 {
                                println!("usage: vo-video <jid> <path> [caption]");
                                continue;
                            }
                            let path = std::path::Path::new(vo_parts[2]);
                            match read_local_media_file(path) {
                                Ok(data) => {
                                    let mime = match path.extension().and_then(|e| e.to_str()) {
                                        Some("webm") => "video/webm",
                                        Some("mov") => "video/quicktime",
                                        Some("3gp") => "video/3gpp",
                                        _ => "video/mp4",
                                    };
                                    let caption = vo_parts.get(3).copied();
                                    match bridge_for_repl
                                        .send_view_once_video(vo_parts[1], data, mime, caption)
                                        .await
                                    {
                                        Ok(()) => println!(">> view-once video sent to {}", vo_parts[1]),
                                        Err(e) => println!("!! vo-video failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! cannot read file: {e}"),
                            }
                        }
                        "poll" => {
                            // poll <jid> <count> <question> | <opt1> | <opt2> ...
                            let poll_parts: Vec<&str> = line.splitn(4, ' ').collect();
                            if poll_parts.len() < 4 {
                                println!("usage: poll <jid> <count> <question> | opt1 | opt2 ...");
                                continue;
                            }
                            let jid = poll_parts[1];
                            let count: u32 = match poll_parts[2].parse() {
                                Ok(v) => v,
                                Err(_) => {
                                    println!("!! invalid selectable count");
                                    continue;
                                }
                            };
                            let rest = poll_parts[3];
                            let segments: Vec<&str> = rest.split('|').collect();
                            if segments.len() < 2 {
                                println!("!! need at least a question and one option separated by |");
                                continue;
                            }
                            let question = segments[0].trim();
                            let options: Vec<String> = segments[1..]
                                .iter()
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect();
                            if options.is_empty() {
                                println!("!! need at least one option");
                                continue;
                            }
                            match bridge_for_repl.send_poll(jid, question, &options, count).await {
                                Ok(id) => println!(">> poll sent to {} (id: {})", jid, id),
                                Err(e) => println!("!! poll failed: {e}"),
                            }
                        }
                        "subscribe" | "sub" => {
                            if parts.len() < 2 {
                                println!("usage: subscribe <jid>");
                                continue;
                            }
                            match bridge_for_repl.subscribe_presence(parts[1]).await {
                                Ok(()) => println!(">> subscribed to presence of {}", parts[1]),
                                Err(e) => println!("!! subscribe failed: {e}"),
                            }
                        }
                        "forward" | "fwd" => {
                            if parts.len() < 3 {
                                println!("usage: forward <jid> <msg_id>");
                                continue;
                            }
                            match bridge_for_repl.forward_message(parts[1], parts[2]).await {
                                Ok(id) => println!(">> forwarded to {} (id: {})", parts[1], id),
                                Err(e) => println!("!! forward failed: {e}"),
                            }
                        }
                        "location" | "loc" => {
                            let loc_parts: Vec<&str> = line.splitn(4, ' ').collect();
                            if loc_parts.len() < 4 {
                                println!("usage: location <jid> <lat> <lon>");
                                continue;
                            }
                            let lat: f64 = match loc_parts[2].parse() {
                                Ok(v) => v,
                                Err(_) => {
                                    println!("!! invalid latitude");
                                    continue;
                                }
                            };
                            let lon: f64 = match loc_parts[3].parse() {
                                Ok(v) => v,
                                Err(_) => {
                                    println!("!! invalid longitude");
                                    continue;
                                }
                            };
                            match bridge_for_repl
                                .send_location(loc_parts[1], lat, lon, None, None)
                                .await
                            {
                                Ok(()) => println!(">> location sent to {}", loc_parts[1]),
                                Err(e) => println!("!! location send failed: {e}"),
                            }
                        }
                        "contact" => {
                            let contact_parts: Vec<&str> = line.splitn(4, ' ').collect();
                            if contact_parts.len() < 4 {
                                println!("usage: contact <jid> <name> <phone>");
                                continue;
                            }
                            let name = contact_parts[2];
                            let phone = contact_parts[3];
                            let vcard = format!(
                                "BEGIN:VCARD\nVERSION:3.0\nFN:{name}\nTEL;type=CELL:+{phone}\nEND:VCARD"
                            );
                            match bridge_for_repl
                                .send_contact(contact_parts[1], name, &vcard)
                                .await
                            {
                                Ok(()) => println!(">> contact sent to {}", contact_parts[1]),
                                Err(e) => println!("!! contact send failed: {e}"),
                            }
                        }
                        "typing" | "t" => {
                            if parts.len() < 2 {
                                println!("usage: typing <jid>");
                                continue;
                            }
                            if let Err(e) = bridge_for_repl.start_typing(parts[1]).await {
                                println!("!! typing failed: {e}");
                            }
                        }
                        "stop-typing" | "st" => {
                            if parts.len() < 2 {
                                println!("usage: stop-typing <jid>");
                                continue;
                            }
                            if let Err(e) = bridge_for_repl.stop_typing(parts[1]).await {
                                println!("!! stop-typing failed: {e}");
                            }
                        }
                        "recording" | "rec" => {
                            if parts.len() < 2 {
                                println!("usage: recording <jid>");
                                continue;
                            }
                            if let Err(e) = bridge_for_repl.start_recording(parts[1]).await {
                                println!("!! recording failed: {e}");
                            }
                        }
                        "stop-recording" | "sr" => {
                            if parts.len() < 2 {
                                println!("usage: stop-recording <jid>");
                                continue;
                            }
                            if let Err(e) = bridge_for_repl.stop_recording(parts[1]).await {
                                println!("!! stop-recording failed: {e}");
                            }
                        }
                        "edit-test" | "et" => {
                            if parts.len() < 2 {
                                println!("usage: edit-test <jid>");
                                continue;
                            }
                            let jid = parts[1].to_string();
                            println!(">> sending original message...");
                            match bridge_for_repl
                                .send_message_with_id(&jid, "EDIT-TEST: This will change in 3 seconds...")
                                .await
                            {
                                Ok(id) => {
                                    println!(">> sent (id: {}), waiting 3s then editing...", id);
                                    tokio::time::sleep(Duration::from_secs(3)).await;
                                    match bridge_for_repl
                                        .edit_message(&jid, &id, "EDITED: whatsrust edited this!")
                                        .await
                                    {
                                        Ok(()) => println!(">> edit sent for {}", id),
                                        Err(e) => println!("!! edit failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! send failed: {e}"),
                            }
                        }
                        "revoke" | "del" => {
                            if parts.len() < 3 {
                                println!("usage: revoke <jid> <msg_id>");
                                continue;
                            }
                            match bridge_for_repl.revoke_message(parts[1], parts[2]).await {
                                Ok(()) => println!(">> message revoked: {}", parts[2]),
                                Err(e) => println!("!! revoke failed: {e}"),
                            }
                        }
                        "revoke-test" | "rt" => {
                            if parts.len() < 2 {
                                println!("usage: revoke-test <jid>");
                                continue;
                            }
                            let jid = parts[1].to_string();
                            println!(">> sending message to delete in 5s...");
                            match bridge_for_repl
                                .send_message_with_id(&jid, "DELETE-TEST: This will be deleted in 5 seconds...")
                                .await
                            {
                                Ok(id) => {
                                    println!(">> sent (id: {}), waiting 5s then revoking...", id);
                                    tokio::time::sleep(Duration::from_secs(5)).await;
                                    match bridge_for_repl.revoke_message(&jid, &id).await {
                                        Ok(()) => println!(">> revoke sent for {}", id),
                                        Err(e) => println!("!! revoke failed: {e}"),
                                    }
                                }
                                Err(e) => println!("!! send failed: {e}"),
                            }
                        }
                        "status" => {
                            println!(
                                "state: {:?}, connected: {}",
                                bridge_for_repl.state(),
                                bridge_for_repl.is_connected()
                            );
                        }
                        "groups" => {
                            match bridge_for_repl.get_joined_groups().await {
                                Ok(groups) => {
                                    println!("joined {} groups:", groups.len());
                                    for g in &groups {
                                        println!("  {} — {} ({} members)", g.jid, g.subject, g.participants.len());
                                    }
                                }
                                Err(e) => eprintln!("error: {e}"),
                            }
                        }
                        "group-info" => {
                            if parts.len() < 2 {
                                eprintln!("usage: group-info <group-jid>");
                            } else {
                                let jid = parts[1];
                                match bridge_for_repl.get_group_info(jid).await {
                                    Ok(info) => {
                                        println!("group: {} ({})", info.subject, info.jid);
                                        for p in &info.participants {
                                            let role = if p.is_admin { " [admin]" } else { "" };
                                            let phone = p.phone.as_deref().unwrap_or("?");
                                            println!("  {} ({}){}", p.jid, phone, role);
                                        }
                                    }
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-desc" => {
                            if parts.len() < 2 {
                                eprintln!("usage: group-desc <group-jid> [description]");
                            } else {
                                let jid = parts[1];
                                let desc = if parts.len() > 2 {
                                    Some(parts[2..].join(" "))
                                } else {
                                    None
                                };
                                match bridge_for_repl
                                    .set_group_description(jid, desc.as_deref())
                                    .await
                                {
                                    Ok(()) => println!("description updated"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-add" => {
                            if parts.len() < 3 {
                                eprintln!("usage: group-add <group-jid> <phone> [phone...]");
                            } else {
                                let jid = parts[1];
                                let phones: Vec<&str> = parts[2].split_whitespace().collect();
                                match bridge_for_repl.add_participants(jid, &phones).await {
                                    Ok(results) => {
                                        for (p, status) in &results {
                                            println!("  {} → {}", p, status.as_deref().unwrap_or("ok"));
                                        }
                                    }
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-remove" => {
                            if parts.len() < 3 {
                                eprintln!("usage: group-remove <group-jid> <phone> [phone...]");
                            } else {
                                let jid = parts[1];
                                let phones: Vec<&str> = parts[2].split_whitespace().collect();
                                match bridge_for_repl.remove_participants(jid, &phones).await {
                                    Ok(results) => {
                                        for (p, status) in &results {
                                            println!("  {} → {}", p, status.as_deref().unwrap_or("ok"));
                                        }
                                    }
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-promote" => {
                            if parts.len() < 3 {
                                eprintln!("usage: group-promote <group-jid> <phone> [phone...]");
                            } else {
                                let jid = parts[1];
                                let phones: Vec<&str> = parts[2].split_whitespace().collect();
                                match bridge_for_repl.promote_participants(jid, &phones).await {
                                    Ok(()) => println!("promoted"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-demote" => {
                            if parts.len() < 3 {
                                eprintln!("usage: group-demote <group-jid> <phone> [phone...]");
                            } else {
                                let jid = parts[1];
                                let phones: Vec<&str> = parts[2].split_whitespace().collect();
                                match bridge_for_repl.demote_participants(jid, &phones).await {
                                    Ok(()) => println!("demoted"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-invite" => {
                            if parts.len() < 2 {
                                eprintln!("usage: group-invite <group-jid>");
                            } else {
                                match bridge_for_repl.get_group_invite_link(parts[1]).await {
                                    Ok(link) => println!("{link}"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-create" => {
                            if parts.len() < 3 {
                                eprintln!("usage: group-create <name> <phone> [phone...]");
                            } else {
                                let name = parts[1];
                                let phones: Vec<&str> = parts[2].split_whitespace().collect();
                                match bridge_for_repl.create_group(name, &phones).await {
                                    Ok(gid) => println!("created group: {gid}"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-leave" => {
                            if parts.len() < 2 {
                                eprintln!("usage: group-leave <group-jid>");
                            } else {
                                match bridge_for_repl.leave_group(parts[1]).await {
                                    Ok(()) => println!("left group"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "group-rename" => {
                            if parts.len() < 3 {
                                eprintln!("usage: group-rename <group-jid> <new-name>");
                            } else {
                                let jid = parts[1];
                                let name = parts[2..].join(" ");
                                match bridge_for_repl.set_group_subject(jid, &name).await {
                                    Ok(()) => println!("renamed"),
                                    Err(e) => eprintln!("error: {e}"),
                                }
                            }
                        }
                        "quit" | "q" | "exit" => {
                            cancel_for_repl.cancel();
                            break;
                        }
                        _ => {
                            println!("unknown command: {}", parts[0]);
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    error!(error = %e, "stdin read error");
                    break;
                }
            }
        }
    });

    // Wait for Ctrl-C, SIGTERM, or quit command
    let cancel_for_signals = cancel.clone();
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
            tokio::select! {
                _ = ctrl_c => info!("SIGINT received"),
                _ = sigterm.recv() => info!("SIGTERM received"),
            }
        }
        #[cfg(not(unix))]
        {
            ctrl_c.await.expect("failed to listen for ctrl-c");
            info!("SIGINT received");
        }
        cancel_for_signals.cancel();
    });

    cancel.cancelled().await;
    info!("shutting down...");
    bridge.stop();
    if !bridge.wait_stopped(Duration::from_secs(5)).await {
        warn!("bridge did not fully stop within 5 seconds");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// CLI client mode
// ---------------------------------------------------------------------------

async fn cli_main(args: &[String]) -> Result<()> {
    let port = get_port();
    let cmd = args[0].as_str();

    match cmd {
        "help" | "--help" | "-h" => {
            print_cli_help();
            Ok(())
        }
        "status" => {
            let (status, body) = api::cli_get(port, "/api/status").await?;
            print_json_result(status, &body)?;
            Ok(())
        }
        "qr" => {
            // whatsrust qr [--png /path/to/file.png]
            if args.len() >= 3 && args[1] == "--png" {
                let (status, body) = api::cli_get(port, "/api/qr?format=png").await?;
                if status == 200 {
                    std::fs::write(&args[2], &body)?;
                    println!("{}", json!({"ok": true, "path": args[2]}));
                } else {
                    print_json_result(status, &body)?;
                    std::process::exit(1);
                }
            } else {
                let (status, body) = api::cli_get(port, "/api/qr?format=terminal").await?;
                let text = String::from_utf8_lossy(&body);
                print!("{text}");
                if status >= 400 {
                    std::process::exit(1);
                }
            }
            Ok(())
        }
        "groups" => {
            let (status, body) = api::cli_get(port, "/api/groups").await?;
            print_json_result(status, &body)?;
            Ok(())
        }
        "group-info" => {
            require_args(args, 2, "group-info <jid>")?;
            let (status, body) = api::cli_get(port, &format!("/api/group-info?jid={}", args[1])).await?;
            print_json_result(status, &body)?;
            Ok(())
        }
        "send" => {
            require_args(args, 3, "send <jid> <text>")?;
            let text = args[2..].join(" ");
            let body = json!({"jid": args[1], "text": text}).to_string();
            let (status, resp) = api::cli_post(port, "/api/send", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "reply" => {
            require_args(args, 5, "reply <jid> <msg_id> <sender_jid> <text>")?;
            let text = args[4..].join(" ");
            let body = json!({"jid": args[1], "id": args[2], "sender": args[3], "text": text}).to_string();
            let (status, resp) = api::cli_post(port, "/api/reply", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "edit" => {
            require_args(args, 4, "edit <jid> <msg_id> <new text>")?;
            let text = args[3..].join(" ");
            let body = json!({"jid": args[1], "id": args[2], "text": text}).to_string();
            let (status, resp) = api::cli_post(port, "/api/edit", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "react" => {
            require_args(args, 4, "react <jid> <msg_id> <emoji> [from_me] [sender_jid]")?;
            let (from_me, sender_jid) = parse_cli_react_args(args)?;
            let body = json!({
                "jid": args[1],
                "id": args[2],
                "emoji": args[3],
                "from_me": from_me,
                "sender_jid": sender_jid,
            })
            .to_string();
            let (status, resp) = api::cli_post(port, "/api/react", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "unreact" => {
            require_args(args, 3, "unreact <jid> <msg_id> [from_me] [sender_jid]")?;
            let (from_me, sender_jid) = parse_cli_react_args_without_emoji(args)?;
            let body = json!({
                "jid": args[1],
                "id": args[2],
                "from_me": from_me,
                "sender_jid": sender_jid,
            })
            .to_string();
            let (status, resp) = api::cli_post(port, "/api/unreact", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "revoke" => {
            require_args(args, 3, "revoke <jid> <msg_id>")?;
            let body = json!({"jid": args[1], "id": args[2]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/revoke", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "image" => {
            require_args(args, 3, "image <jid> <path> [caption]")?;
            let caption = if args.len() > 3 { Some(args[3..].join(" ")) } else { None };
            let body = json!({"jid": args[1], "path": args[2], "caption": caption}).to_string();
            let (status, resp) = api::cli_post(port, "/api/image", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "video" => {
            require_args(args, 3, "video <jid> <path> [caption]")?;
            let caption = if args.len() > 3 { Some(args[3..].join(" ")) } else { None };
            let body = json!({"jid": args[1], "path": args[2], "caption": caption}).to_string();
            let (status, resp) = api::cli_post(port, "/api/video", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "audio" => {
            require_args(args, 3, "audio <jid> <path>")?;
            let body = json!({"jid": args[1], "path": args[2]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/audio", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "doc" => {
            require_args(args, 3, "doc <jid> <path>")?;
            let body = json!({"jid": args[1], "path": args[2]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/doc", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "sticker" => {
            require_args(args, 3, "sticker <jid> <path>")?;
            let body = json!({"jid": args[1], "path": args[2]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/sticker", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "location" => {
            require_args(args, 4, "location <jid> <lat> <lon>")?;
            let body = json!({"jid": args[1], "lat": args[2].parse::<f64>()?, "lon": args[3].parse::<f64>()?}).to_string();
            let (status, resp) = api::cli_post(port, "/api/location", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "contact" => {
            require_args(args, 4, "contact <jid> <name> <phone>")?;
            let body = json!({"jid": args[1], "name": args[2], "phone": args[3]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/contact", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "forward" => {
            require_args(args, 3, "forward <jid> <msg_id>")?;
            let body = json!({"jid": args[1], "msg_id": args[2]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/forward", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "poll" => {
            // whatsrust poll <jid> <count> <question> -- <opt1> <opt2> ...
            require_args(args, 4, "poll <jid> <count> <question> -- <opt1> <opt2> ...")?;
            let count: u32 = args[2].parse().map_err(|_| anyhow::anyhow!("invalid selectable count"))?;
            // Find -- separator
            let sep = args.iter().position(|a| a == "--");
            let (question, options) = match sep {
                Some(idx) => {
                    let q = args[3..idx].join(" ");
                    let opts: Vec<String> = args[idx + 1..].iter().map(|s| s.to_string()).collect();
                    (q, opts)
                }
                None => {
                    // No separator: question is args[3], remaining are options
                    let q = args[3].clone();
                    let opts: Vec<String> = args[4..].iter().map(|s| s.to_string()).collect();
                    (q, opts)
                }
            };
            let body = json!({"jid": args[1], "question": question, "options": options, "selectable_count": count}).to_string();
            let (status, resp) = api::cli_post(port, "/api/poll", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "vo-image" | "view-once-image" => {
            require_args(args, 3, "vo-image <jid> <path> [caption]")?;
            let caption = if args.len() > 3 { Some(args[3..].join(" ")) } else { None };
            let body = json!({"jid": args[1], "path": args[2], "caption": caption}).to_string();
            let (status, resp) = api::cli_post(port, "/api/view-once-image", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "vo-video" | "view-once-video" => {
            require_args(args, 3, "vo-video <jid> <path> [caption]")?;
            let caption = if args.len() > 3 { Some(args[3..].join(" ")) } else { None };
            let body = json!({"jid": args[1], "path": args[2], "caption": caption}).to_string();
            let (status, resp) = api::cli_post(port, "/api/view-once-video", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "typing" => {
            require_args(args, 2, "typing <jid>")?;
            let body = json!({"jid": args[1]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/typing", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "stop-typing" => {
            require_args(args, 2, "stop-typing <jid>")?;
            let body = json!({"jid": args[1]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/stop-typing", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "recording" => {
            require_args(args, 2, "recording <jid>")?;
            let body = json!({"jid": args[1]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/recording", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "stop-recording" => {
            require_args(args, 2, "stop-recording <jid>")?;
            let body = json!({"jid": args[1]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/stop-recording", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "subscribe" | "subscribe-presence" => {
            require_args(args, 2, "subscribe <jid>")?;
            let body = json!({"jid": args[1]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/subscribe-presence", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-create" => {
            require_args(args, 3, "group-create <name> <participant1> [participant2] ...")?;
            let body = json!({"name": args[1], "participants": &args[2..]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-create", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-subject" | "group-rename" => {
            require_args(args, 3, "group-subject <jid> <subject>")?;
            let body = json!({"jid": args[1], "subject": args[2..].join(" ")}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-subject", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-desc" | "group-description" => {
            require_args(args, 2, "group-desc <jid> [description]")?;
            let desc = if args.len() > 2 { Some(args[2..].join(" ")) } else { None };
            let body = json!({"jid": args[1], "description": desc}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-description", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-leave" => {
            require_args(args, 2, "group-leave <jid>")?;
            let body = json!({"jid": args[1]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-leave", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-invite" | "group-invite-link" => {
            require_args(args, 2, "group-invite <jid>")?;
            let (status, resp) = api::cli_get(port, &format!("/api/group-invite-link?jid={}", args[1])).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-add" => {
            require_args(args, 3, "group-add <jid> <participant1> [participant2] ...")?;
            let body = json!({"jid": args[1], "participants": &args[2..]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-add", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-remove" => {
            require_args(args, 3, "group-remove <jid> <participant1> [participant2] ...")?;
            let body = json!({"jid": args[1], "participants": &args[2..]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-remove", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-promote" => {
            require_args(args, 3, "group-promote <jid> <participant1> [participant2] ...")?;
            let body = json!({"jid": args[1], "participants": &args[2..]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-promote", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "group-demote" => {
            require_args(args, 3, "group-demote <jid> <participant1> [participant2] ...")?;
            let body = json!({"jid": args[1], "participants": &args[2..]}).to_string();
            let (status, resp) = api::cli_post(port, "/api/group-demote", &body).await?;
            print_json_result(status, &resp)?;
            Ok(())
        }
        "events" => {
            api::cli_stream_sse(port).await?;
            Ok(())
        }
        "mcp" => {
            // MCP server mode — JSON-RPC over stdin/stdout, proxies to HTTP daemon
            mcp::run_mcp_server(port);
            Ok(())
        }
        "history" => {
            require_args(args, 2, "history <jid> [limit]")?;
            let limit = args.get(2).and_then(|v| v.parse::<i64>().ok()).unwrap_or(20);
            let (status, body) = api::cli_get(port, &format!("/api/history?jid={}&limit={}", args[1], limit)).await?;
            print_json_result(status, &body)?;
            Ok(())
        }
        "search" => {
            require_args(args, 2, "search <query> [jid]")?;
            let query = &args[1];
            let jid_param = args.get(2).map(|j| format!("&jid={j}")).unwrap_or_default();
            let (status, body) = api::cli_get(port, &format!("/api/search?q={query}{jid_param}")).await?;
            print_json_result(status, &body)?;
            Ok(())
        }
        _ => {
            eprintln!("unknown command: {cmd}");
            print_cli_help();
            std::process::exit(1);
        }
    }
}

fn require_args(args: &[String], min: usize, usage: &str) -> Result<()> {
    if args.len() < min {
        anyhow::bail!("usage: whatsrust {usage}");
    }
    Ok(())
}

/// Parse optional [from_me|sender_jid] [sender_jid] into (from_me, sender_jid).
/// If the first optional arg is a sender JID (not a boolean), infers from_me=false.
/// Used by both REPL and CLI reaction commands.
fn parse_react_target(first_opt: Option<&str>, second_opt: Option<&str>) -> (bool, Option<String>) {
    match first_opt {
        None => (true, None),
        Some(v) => match parse_boolish(v) {
            Some(from_me) => (from_me, second_opt.map(|s| s.to_string())),
            None => (false, Some(v.to_string())), // sender JID implies from_me=false
        },
    }
}

fn parse_boolish(value: &str) -> Option<bool> {
    match value {
        "true" | "1" | "yes" => Some(true),
        "false" | "0" | "no" => Some(false),
        _ => None,
    }
}

fn parse_cli_react_args(args: &[String]) -> Result<(bool, Option<String>)> {
    if args.len() > 6 {
        anyhow::bail!("usage: whatsrust react <jid> <msg_id> <emoji> [from_me|sender_jid] [sender_jid]");
    }
    Ok(parse_react_target(
        args.get(4).map(|s| s.as_str()),
        args.get(5).map(|s| s.as_str()),
    ))
}

fn parse_cli_react_args_without_emoji(args: &[String]) -> Result<(bool, Option<String>)> {
    if args.len() > 5 {
        anyhow::bail!("usage: whatsrust unreact <jid> <msg_id> [from_me|sender_jid] [sender_jid]");
    }
    Ok(parse_react_target(
        args.get(3).map(|s| s.as_str()),
        args.get(4).map(|s| s.as_str()),
    ))
}

/// Print JSON response and return error if `ok` field is false.
fn print_json_result(status: u16, body: &[u8]) -> Result<()> {
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
        // Exit with error if server returned non-200 or ok=false
        if status >= 400 || v.get("ok") == Some(&serde_json::Value::Bool(false)) {
            std::process::exit(1);
        }
    } else {
        println!("{}", String::from_utf8_lossy(body));
        if status >= 400 {
            std::process::exit(1);
        }
    }
    Ok(())
}

fn print_cli_help() {
    println!("whatsrust v{}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("DAEMON MODE:");
    println!("  whatsrust                              Start bridge with REPL + API server");
    println!();
    println!("CLI MODE (requires running daemon):");
    println!("  whatsrust status                       Bridge state and metrics");
    println!("  whatsrust qr                           Show QR code in terminal");
    println!("  whatsrust qr --png <path>              Save QR as PNG file");
    println!("  whatsrust groups                       List joined groups");
    println!("  whatsrust group-info <jid>             Group details + members");
    println!("  whatsrust send <jid> <text>            Send text message");
    println!("  whatsrust reply <jid> <id> <sender> <text>  Reply to a message");
    println!("  whatsrust edit <jid> <id> <text>       Edit a sent message");
    println!("  whatsrust react <jid> <id> <emoji> [from_me] [sender_jid]  React to a message");
    println!("  whatsrust unreact <jid> <id> [from_me] [sender_jid]  Remove a reaction");
    println!("  whatsrust revoke <jid> <id>            Delete message for everyone");
    println!("  whatsrust image <jid> <path> [caption] Send image");
    println!("  whatsrust video <jid> <path> [caption] Send video");
    println!("  whatsrust audio <jid> <path>           Send voice note");
    println!("  whatsrust doc <jid> <path>             Send document");
    println!("  whatsrust sticker <jid> <path>         Send sticker");
    println!("  whatsrust location <jid> <lat> <lon>   Send location pin");
    println!("  whatsrust contact <jid> <name> <phone> Send contact card");
    println!("  whatsrust forward <jid> <msg_id>       Forward a cached message");
    println!("  whatsrust poll <jid> <N> <Q> -- <opts> Create poll (N=selectable)");
    println!("  whatsrust vo-image <jid> <path> [cap]  Send view-once image");
    println!("  whatsrust vo-video <jid> <path> [cap]  Send view-once video");
    println!("  whatsrust typing <jid>                 Send typing indicator");
    println!("  whatsrust stop-typing <jid>            Clear typing indicator");
    println!("  whatsrust recording <jid>              Send recording indicator");
    println!("  whatsrust stop-recording <jid>         Clear recording indicator");
    println!("  whatsrust subscribe <jid>              Subscribe to presence updates");
    println!("  whatsrust group-create <name> <jid>... Create group with participants");
    println!("  whatsrust group-subject <jid> <subj>   Set group subject/name");
    println!("  whatsrust group-desc <jid> [desc]      Set/clear group description");
    println!("  whatsrust group-leave <jid>            Leave a group");
    println!("  whatsrust group-invite <jid>           Get group invite link");
    println!("  whatsrust group-add <jid> <jid>...     Add participants to group");
    println!("  whatsrust group-remove <jid> <jid>...  Remove participants from group");
    println!("  whatsrust group-promote <jid> <jid>... Promote participants to admin");
    println!("  whatsrust group-demote <jid> <jid>...  Demote admins to regular");
    println!("  whatsrust events                       Stream SSE events (inbound + status)");
    println!("  whatsrust mcp                          MCP server (JSON-RPC over stdio)");
    println!("  whatsrust history <jid> [limit]        Recent messages for a chat");
    println!("  whatsrust search <query> [jid]         Search message history");
    println!();
    println!("ENVIRONMENT:");
    println!("  WHATSRUST_PORT   API port (default: 7270, fallback: HEALTH_PORT)");
    println!("  WHATSRUST_BIND   API bind address (default: 127.0.0.1)");
    println!("  WHATSRUST_ALLOW_REMOTE=1  Permit non-loopback API binds");
    println!("  WHATSRUST_API_TOKEN  Optional API bearer token; required for remote binds");
    println!("  WHATSRUST_SEND_BURST     Max burst size before rate limit (default: 5)");
    println!();
    println!("JID FORMAT:");
    println!("  Phone number: 15551234567 or 15551234567@s.whatsapp.net");
    println!("  Group: 120363012345678901@g.us");
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn test_parse_cli_react_args_defaults() {
        let args = vec![
            "react".to_string(),
            "chat".to_string(),
            "msg".to_string(),
            "👍".to_string(),
        ];
        let (from_me, sender_jid) = parse_cli_react_args(&args).unwrap();
        assert!(from_me);
        assert!(sender_jid.is_none());
    }

    #[test]
    fn test_parse_cli_react_args_explicit_sender_implies_not_from_me() {
        let args = vec![
            "react".to_string(),
            "chat".to_string(),
            "msg".to_string(),
            "👍".to_string(),
            "alice@s.whatsapp.net".to_string(),
        ];
        let (from_me, sender_jid) = parse_cli_react_args(&args).unwrap();
        assert!(!from_me);
        assert_eq!(sender_jid.as_deref(), Some("alice@s.whatsapp.net"));
    }

    #[test]
    fn test_parse_cli_react_args_bool_and_sender() {
        let args = vec![
            "react".to_string(),
            "chat".to_string(),
            "msg".to_string(),
            "👍".to_string(),
            "false".to_string(),
            "alice@s.whatsapp.net".to_string(),
        ];
        let (from_me, sender_jid) = parse_cli_react_args(&args).unwrap();
        assert!(!from_me);
        assert_eq!(sender_jid.as_deref(), Some("alice@s.whatsapp.net"));
    }

    #[test]
    fn test_parse_cli_react_args_without_emoji_sender_implies_not_from_me() {
        let args = vec![
            "unreact".to_string(),
            "chat".to_string(),
            "msg".to_string(),
            "alice@s.whatsapp.net".to_string(),
        ];
        let (from_me, sender_jid) = parse_cli_react_args_without_emoji(&args).unwrap();
        assert!(!from_me);
        assert_eq!(sender_jid.as_deref(), Some("alice@s.whatsapp.net"));
    }
}
