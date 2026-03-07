//! whatsrust — Pure Rust WhatsApp bridge.
//!
//! Lean replacement for the Baileys (Node.js) sidecar.
//! Uses whatsapp-rust (wa-rs) for the WhatsApp Web protocol
//! and our own rusqlite backend for Signal Protocol storage.

mod bridge;
mod dedup;
mod instance_lock;
mod polls;
pub mod qr;
mod read_receipts;
mod storage;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use bridge::{BridgeConfig, WhatsAppBridge};

#[tokio::main]
async fn main() -> Result<()> {
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

    let health_port: u16 = std::env::var("HEALTH_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let backup_dir = std::env::var("BACKUP_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from("whatsapp.db.backups")));

    let config = BridgeConfig {
        db_path: PathBuf::from("whatsapp.db"),
        pair_phone: std::env::var("WHATSAPP_PAIR_PHONE").ok(),
        allowed_numbers: allowed,
        health_port,
        backup_dir,
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
                            match std::fs::read(path) {
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
                            match std::fs::read(path) {
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
                            match std::fs::read(path) {
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
                            match std::fs::read(path) {
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
                            // react <jid> <msg_id> <emoji> [from_me] [sender_jid]
                            let react_parts: Vec<&str> = line.splitn(6, ' ').collect();
                            if react_parts.len() < 4 {
                                println!("usage: react <jid> <msg_id> <emoji> [from_me=true] [sender_jid]");
                                continue;
                            }
                            let from_me = react_parts
                                .get(4)
                                .map(|v| *v != "false" && *v != "0")
                                .unwrap_or(true);
                            let sender_jid = react_parts.get(5).copied();
                            match bridge_for_repl
                                .send_reaction(
                                    react_parts[1],
                                    react_parts[2],
                                    sender_jid,
                                    react_parts[3],
                                    from_me,
                                )
                                .await
                            {
                                Ok(()) => println!(">> reacted {} (from_me={})", react_parts[3], from_me),
                                Err(e) => println!("!! react failed: {e}"),
                            }
                        }
                        "sticker" | "stk" => {
                            if parts.len() < 3 {
                                println!("usage: sticker <jid> <path>");
                                continue;
                            }
                            let path = std::path::Path::new(parts[2]);
                            match std::fs::read(path) {
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
                            match std::fs::read(path) {
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
                            match std::fs::read(path) {
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
                                let phones: Vec<&str> = parts[2..].to_vec();
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
                                let phones: Vec<&str> = parts[2..].to_vec();
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
                                let phones: Vec<&str> = parts[2..].to_vec();
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
                                let phones: Vec<&str> = parts[2..].to_vec();
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
                                let phones: Vec<&str> = parts[2..].to_vec();
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
