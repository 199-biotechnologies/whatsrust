//! Lean rusqlite storage backend for whatsapp-rust.
//!
//! Implements all four `Backend` traits (SignalStore, AppSyncStore, ProtocolStore, DeviceStore)
//! using raw rusqlite — no Diesel, no ORM, no migration framework.
//!
//! Design advantages over upstream (Diesel, 2370 lines) and ZeroClaw (rusqlite, 1347 lines):
//!   - Single-device only (no device_id column) — halves query complexity
//!   - One Mutex<Connection> + spawn_blocking — no semaphore needed
//!   - WAL mode + NORMAL sync — fast writes, no corruption risk
//!   - CREATE TABLE IF NOT EXISTS — no migration framework
//!   - serde_json only for opaque types (HashState) — everything else is columns

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use parking_lot::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use prost::Message as ProstMessage;
use rusqlite::{params, Connection, OptionalExtension};
use wacore::appstate::hash::HashState;
use wacore::appstate::processor::AppStateMutationMAC;
use wacore::libsignal::protocol::{KeyPair, PrivateKey, PublicKey};
use wacore::store::device::DEVICE_PROPS;
use wacore::store::error::{db_err, Result, StoreError};
use wacore::store::traits::*;
use wacore::store::Device;
use wacore_binary::jid::Jid;
use waproto::whatsapp as wa;

// ---------------------------------------------------------------------------
// Schema — 15 tables, no device_id column (single-device design)
// ---------------------------------------------------------------------------

const SCHEMA: &str = "
-- Device identity & crypto keys
CREATE TABLE IF NOT EXISTS device (
    id INTEGER PRIMARY KEY,
    pn TEXT NOT NULL DEFAULT '',
    lid TEXT NOT NULL DEFAULT '',
    registration_id INTEGER NOT NULL DEFAULT 0,
    noise_key BLOB NOT NULL DEFAULT x'',
    identity_key BLOB NOT NULL DEFAULT x'',
    signed_pre_key BLOB NOT NULL DEFAULT x'',
    signed_pre_key_id INTEGER NOT NULL DEFAULT 0,
    signed_pre_key_signature BLOB NOT NULL DEFAULT x'',
    adv_secret_key BLOB NOT NULL DEFAULT x'',
    account BLOB,
    push_name TEXT NOT NULL DEFAULT '',
    app_version_primary INTEGER NOT NULL DEFAULT 0,
    app_version_secondary INTEGER NOT NULL DEFAULT 0,
    app_version_tertiary INTEGER NOT NULL DEFAULT 0,
    app_version_last_fetched_ms INTEGER NOT NULL DEFAULT 0,
    edge_routing_info BLOB,
    props_hash TEXT,
    next_pre_key_id INTEGER NOT NULL DEFAULT 0,
    nct_salt BLOB
);

-- Signal Protocol: identity keys
CREATE TABLE IF NOT EXISTS identities (
    address TEXT PRIMARY KEY,
    key BLOB NOT NULL
);

-- Signal Protocol: sessions
CREATE TABLE IF NOT EXISTS sessions (
    address TEXT PRIMARY KEY,
    record BLOB NOT NULL
);

-- Signal Protocol: pre-keys
CREATE TABLE IF NOT EXISTS prekeys (
    id INTEGER PRIMARY KEY,
    key BLOB NOT NULL,
    uploaded INTEGER NOT NULL DEFAULT 0
);

-- Signal Protocol: signed pre-keys
CREATE TABLE IF NOT EXISTS signed_prekeys (
    id INTEGER PRIMARY KEY,
    record BLOB NOT NULL
);

-- Signal Protocol: sender keys (group messaging)
CREATE TABLE IF NOT EXISTS sender_keys (
    address TEXT PRIMARY KEY,
    record BLOB NOT NULL
);

-- App state sync keys
CREATE TABLE IF NOT EXISTS app_state_keys (
    key_id BLOB PRIMARY KEY,
    key_data BLOB NOT NULL,
    fingerprint BLOB NOT NULL DEFAULT x'',
    timestamp INTEGER NOT NULL DEFAULT 0
);

-- App state versions (HashState serialized as JSON)
CREATE TABLE IF NOT EXISTS app_state_versions (
    name TEXT PRIMARY KEY,
    state_data BLOB NOT NULL
);

-- App state mutation MACs
CREATE TABLE IF NOT EXISTS app_state_mutation_macs (
    name TEXT NOT NULL,
    index_mac BLOB NOT NULL,
    version INTEGER NOT NULL,
    value_mac BLOB NOT NULL,
    PRIMARY KEY (name, index_mac)
);

-- Per-device sender key tracking (replaces skdm_recipients)
CREATE TABLE IF NOT EXISTS sender_key_devices (
    group_jid TEXT NOT NULL,
    device_jid TEXT NOT NULL,
    needs_sender_key INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (group_jid, device_jid)
);

-- Sent message store for retry handling
CREATE TABLE IF NOT EXISTS sent_messages (
    chat_jid TEXT NOT NULL,
    message_id TEXT NOT NULL,
    message_bytes BLOB NOT NULL,
    timestamp INTEGER NOT NULL,
    PRIMARY KEY (chat_jid, message_id)
);

-- LID (Linked Identity) to phone number mapping
CREATE TABLE IF NOT EXISTS lid_pn_mapping (
    lid TEXT PRIMARY KEY,
    phone_number TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    learning_source TEXT NOT NULL
);

-- Base keys for replay/collision detection
CREATE TABLE IF NOT EXISTS base_keys (
    address TEXT NOT NULL,
    message_id TEXT NOT NULL,
    base_key BLOB NOT NULL,
    PRIMARY KEY (address, message_id)
);

-- Device registry (multi-device awareness per contact)
CREATE TABLE IF NOT EXISTS device_registry (
    user_id TEXT PRIMARY KEY,
    devices_json TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    phash TEXT
);

-- Trusted contact privacy tokens
CREATE TABLE IF NOT EXISTS tc_tokens (
    jid TEXT PRIMARY KEY,
    token BLOB NOT NULL,
    token_timestamp INTEGER NOT NULL,
    sender_timestamp INTEGER
);

-- Persistent outbound job queue (crash-safe, typed operations)
CREATE TABLE IF NOT EXISTS outbound_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    jid TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '',
    op_kind TEXT NOT NULL DEFAULT 'text',
    payload_json TEXT NOT NULL DEFAULT '{}',
    payload_blob BLOB,
    wa_message_id TEXT,
    delivery_status TEXT,
    last_error TEXT,
    status TEXT NOT NULL DEFAULT 'queued',
    retries INTEGER NOT NULL DEFAULT 0,
    retry_after INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Poll encryption keys (for decrypting incoming votes)
CREATE TABLE IF NOT EXISTS poll_keys (
    chat_jid TEXT NOT NULL,
    poll_id TEXT NOT NULL,
    enc_key BLOB NOT NULL,
    options_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (chat_jid, poll_id)
);

-- Inbound message history (searchable, prunable)
CREATE TABLE IF NOT EXISTS inbound_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_jid TEXT NOT NULL,
    sender_jid TEXT NOT NULL,
    message_id TEXT NOT NULL UNIQUE,
    content_kind TEXT NOT NULL,
    body_text TEXT,
    timestamp INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);

-- Indexes for non-PK lookups
CREATE INDEX IF NOT EXISTS idx_lid_pn_phone ON lid_pn_mapping(phone_number);
CREATE INDEX IF NOT EXISTS idx_tc_tokens_ts ON tc_tokens(token_timestamp);
CREATE INDEX IF NOT EXISTS idx_outbound_status ON outbound_queue(status, retry_after, id);
CREATE INDEX IF NOT EXISTS idx_outbound_wa_id ON outbound_queue(wa_message_id);
CREATE INDEX IF NOT EXISTS idx_inbound_chat_ts ON inbound_messages(chat_jid, timestamp);
CREATE INDEX IF NOT EXISTS idx_inbound_msg_id ON inbound_messages(message_id);
";

const CURRENT_SCHEMA_VERSION: i64 = 7;

#[cfg(unix)]
fn secure_backup_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let meta = std::fs::metadata(path)
        .map_err(|e| StoreError::Database(format!("stat {}: {e}", path.display())))?;
    let mut perms = meta.permissions();
    let mode = if meta.is_dir() { 0o700 } else { 0o600 };
    perms.set_mode(mode);
    std::fs::set_permissions(path, perms)
        .map_err(|e| StoreError::Database(format!("chmod {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_backup_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

/// A row from the inbound message history table.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InboundRow {
    pub id: i64,
    pub chat_jid: String,
    pub sender_jid: String,
    pub message_id: String,
    pub content_kind: String,
    pub body_text: Option<String>,
    pub timestamp: i64,
}

/// Statistics from a prune operation.
#[derive(Debug, Clone)]
pub struct PruneStats {
    pub sent_deleted: u32,
    pub inbound_deleted: u32,
}

impl Store {
    /// Open (or create) the database at `path` and initialize the schema.
    pub fn new(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(db_err)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA cache_size = -2000;
             PRAGMA foreign_keys = ON;
             PRAGMA temp_store = MEMORY;
             PRAGMA fullfsync = ON;
             PRAGMA journal_size_limit = 67108864;
             PRAGMA mmap_size = 268435456;
             PRAGMA wal_autocheckpoint = 1000;
             PRAGMA auto_vacuum = INCREMENTAL;",
        )
        .map_err(db_err)?;
        conn.execute_batch(SCHEMA).map_err(db_err)?;
        let schema_version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(db_err)?;
        run_schema_migrations(&conn, schema_version)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run a blocking database operation on a dedicated thread.
    async fn run<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock();
            f(&guard)
        })
        .await
        .map_err(db_err)?
    }

    /// Create a hot backup of the database using SQLite's backup API.
    pub fn snapshot_db(&self, dest_path: &Path) -> Result<()> {
        let guard = self.conn.lock();
        let mut dest = Connection::open(dest_path).map_err(db_err)?;
        let backup = rusqlite::backup::Backup::new(&guard, &mut dest).map_err(db_err)?;
        backup
            .run_to_completion(100, std::time::Duration::from_millis(10), None)
            .map_err(db_err)?;
        Ok(())
    }

    /// Clear stored device credentials (used after LoggedOut to trigger re-pairing on reconnect).
    pub async fn clear_device(&self) -> Result<()> {
        self.run(|c| {
            c.execute("DELETE FROM device WHERE id = 1", [])
                .map(|_| ())
                .map_err(db_err)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Outbound queue — persistent message queue (crash-safe)
    // -----------------------------------------------------------------------

    /// Mark an outbound message as successfully sent.
    /// Atomically mark a job as sent AND record its WA message ID in one write.
    /// Prevents the race where a receipt arrives between separate sent/wa_id updates.
    pub async fn mark_outbound_sent_with_id(&self, id: i64, wa_message_id: Option<&str>) -> Result<()> {
        let ts = now_secs();
        let wa = wa_message_id.map(|s| s.to_owned());
        self.run(move |c| {
            c.execute(
                "UPDATE outbound_queue SET status = 'sent', wa_message_id = COALESCE(?1, wa_message_id), updated_at = ?2 WHERE id = ?3",
                params![wa, ts, id],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    /// Mark an outbound message as failed (increment retries).
    /// If retries >= max_retries, status becomes 'failed'; otherwise back to 'queued'
    /// with exponential backoff via `retry_after` (1s, 2s, 4s, 8s, ...).
    /// This prevents head-of-line blocking: newer messages flow while failed ones wait.
    pub async fn mark_outbound_failed(&self, id: i64, max_retries: i32) -> Result<()> {
        let ts = now_secs();
        self.run(move |c| {
            // Read current retry count to compute backoff
            let retries: i32 = c
                .query_row(
                    "SELECT retries FROM outbound_queue WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db_err)?
                .unwrap_or(0);
            let backoff_secs = 1i64 << retries.min(6); // 1, 2, 4, 8, 16, 32, 64 max
            c.execute(
                "UPDATE outbound_queue SET
                    retries = retries + 1,
                    status = CASE WHEN retries + 1 >= ?1 THEN 'failed' ELSE 'queued' END,
                    retry_after = CASE WHEN retries + 1 >= ?1 THEN 0 ELSE ?4 END,
                    updated_at = ?2
                 WHERE id = ?3",
                params![max_retries, ts, id, ts + backoff_secs],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    /// Requeue messages stuck in 'inflight' for longer than the given threshold.
    /// This handles process crashes where inflight messages were never completed.
    pub async fn requeue_stale_inflight(&self, older_than_secs: i64) -> Result<u32> {
        let cutoff = now_secs() - older_than_secs;
        self.run(move |c| {
            let count = c
                .execute(
                    "UPDATE outbound_queue SET status = 'queued', updated_at = ?1
                     WHERE status = 'inflight' AND updated_at < ?2",
                    params![now_secs(), cutoff],
                )
                .map_err(db_err)?;
            u32::try_from(count).map_err(|_| {
                StoreError::Database(format!("requeue count {count} out of u32 range"))
            })
        })
        .await
    }

    /// Requeue a specific outbound message without incrementing retries.
    /// Used when the message can't be sent due to connection issues (not send failures).
    pub async fn requeue_outbound(&self, id: i64) -> Result<()> {
        self.run(move |c| {
            c.execute(
                "UPDATE outbound_queue SET status = 'queued', updated_at = ?1 WHERE id = ?2",
                params![now_secs(), id],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    /// Get the number of messages in queued or inflight status.
    pub async fn outbound_queue_depth(&self) -> Result<i64> {
        self.run(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM outbound_queue WHERE status IN ('queued', 'inflight')",
                [],
                |row| row.get(0),
            )
            .map_err(db_err)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Typed outbound job queue (v5+)
    // -----------------------------------------------------------------------

    /// Enqueue a typed outbound job. Returns the row ID (job_id).
    pub async fn enqueue_job(
        &self,
        jid: &str,
        op_kind: &str,
        payload_json: &str,
        payload_blob: Option<Vec<u8>>,
    ) -> Result<i64> {
        let j = jid.to_owned();
        let ok = op_kind.to_owned();
        let pj = payload_json.to_owned();
        let ts = now_secs();
        self.run(move |c| {
            c.execute(
                "INSERT INTO outbound_queue (jid, payload, op_kind, payload_json, payload_blob, status, retries, created_at, updated_at)
                 VALUES (?1, '', ?2, ?3, ?4, 'queued', 0, ?5, ?5)",
                params![j, ok, pj, payload_blob, ts],
            )
            .map_err(db_err)?;
            Ok(c.last_insert_rowid())
        })
        .await
    }

    /// Enqueue a typed outbound job scheduled for a future time. Returns the row ID.
    /// The job will not be claimed until `execute_at` (unix epoch seconds).
    pub async fn enqueue_job_at(
        &self,
        jid: &str,
        op_kind: &str,
        payload_json: &str,
        payload_blob: Option<Vec<u8>>,
        execute_at: i64,
    ) -> Result<i64> {
        let j = jid.to_owned();
        let ok = op_kind.to_owned();
        let pj = payload_json.to_owned();
        let ts = now_secs();
        self.run(move |c| {
            c.execute(
                "INSERT INTO outbound_queue (jid, payload, op_kind, payload_json, payload_blob, status, retries, retry_after, created_at, updated_at)
                 VALUES (?1, '', ?2, ?3, ?4, 'queued', 0, ?5, ?6, ?6)",
                params![j, ok, pj, payload_blob, execute_at, ts],
            )
            .map_err(db_err)?;
            Ok(c.last_insert_rowid())
        })
        .await
    }

    /// Atomically claim the next queued job for processing. Returns the full job row.
    pub async fn claim_next_job(&self) -> Result<Option<crate::outbound::OutboundJobRow>> {
        let ts = now_secs();
        self.run(move |c| {
            let tx = c.unchecked_transaction().map_err(db_err)?;
            let row = tx
                .query_row(
                    "SELECT id, jid, op_kind, payload_json, payload_blob, retries
                     FROM outbound_queue
                     WHERE status = 'queued' AND retry_after <= ?1
                     ORDER BY id LIMIT 1",
                    params![ts],
                    |row| {
                        Ok(crate::outbound::OutboundJobRow {
                            id: row.get(0)?,
                            jid: row.get(1)?,
                            op_kind: row.get(2)?,
                            payload_json: row.get(3)?,
                            payload_blob: row.get(4)?,
                            retries: row.get(5)?,
                        })
                    },
                )
                .optional()
                .map_err(db_err)?;
            if let Some(ref r) = row {
                tx.execute(
                    "UPDATE outbound_queue SET status = 'inflight', updated_at = ?1 WHERE id = ?2",
                    params![ts, r.id],
                )
                .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)?;
            Ok(row)
        })
        .await
    }


    /// Update delivery status for a job identified by its WhatsApp message ID.
    pub async fn update_delivery_status(&self, wa_message_id: &str, status: &str) -> Result<()> {
        let wa = wa_message_id.to_owned();
        let st = status.to_owned();
        let ts = now_secs();
        self.run(move |c| {
            c.execute(
                "UPDATE outbound_queue SET delivery_status = ?1, updated_at = ?2 WHERE wa_message_id = ?3",
                params![st, ts, wa],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Inbound message history
    // -----------------------------------------------------------------------

    /// Insert an inbound message into the history table. Duplicates (by message_id) are ignored.
    pub async fn insert_inbound(
        &self,
        chat_jid: &str,
        sender_jid: &str,
        message_id: &str,
        content_kind: &str,
        body_text: Option<&str>,
        timestamp: i64,
    ) -> Result<()> {
        let cj = chat_jid.to_owned();
        let sj = sender_jid.to_owned();
        let mid = message_id.to_owned();
        let ck = content_kind.to_owned();
        let bt = body_text.map(|s| s.to_owned());
        let ts = now_secs();
        self.run(move |c| {
            c.execute(
                "INSERT OR IGNORE INTO inbound_messages (chat_jid, sender_jid, message_id, content_kind, body_text, timestamp, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![cj, sj, mid, ck, bt, timestamp, ts],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    /// Search inbound message history. Returns recent messages matching filters.
    pub async fn search_inbound(
        &self,
        chat_jid: Option<&str>,
        query: Option<&str>,
        limit: i64,
        before_ts: Option<i64>,
    ) -> Result<Vec<InboundRow>> {
        let cj = chat_jid.map(|s| s.to_owned());
        let q = query.map(|s| format!("%{}%", s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
        let before = before_ts.unwrap_or(i64::MAX);
        self.run(move |c| {
            let mut sql = String::from(
                "SELECT id, chat_jid, sender_jid, message_id, content_kind, body_text, timestamp
                 FROM inbound_messages WHERE timestamp < ?1"
            );
            let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(before)];
            if let Some(ref jid) = cj {
                sql.push_str(&format!(" AND chat_jid = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(jid.clone()));
            }
            if let Some(ref search) = q {
                sql.push_str(&format!(" AND body_text LIKE ?{} ESCAPE '\\'", params_vec.len() + 1));
                params_vec.push(Box::new(search.clone()));
            }
            sql.push_str(&format!(" ORDER BY timestamp DESC LIMIT ?{}", params_vec.len() + 1));
            params_vec.push(Box::new(limit));

            let params_refs: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter().map(|p| p.as_ref()).collect();
            let mut stmt = c.prepare(&sql).map_err(db_err)?;
            let rows = stmt.query_map(params_refs.as_slice(), |row| {
                Ok(InboundRow {
                    id: row.get(0)?,
                    chat_jid: row.get(1)?,
                    sender_jid: row.get(2)?,
                    message_id: row.get(3)?,
                    content_kind: row.get(4)?,
                    body_text: row.get(5)?,
                    timestamp: row.get(6)?,
                })
            }).map_err(db_err)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(db_err)
        })
        .await
    }

    /// Delete all inbound messages for a chat (e.g. when the chat is deleted).
    pub async fn delete_inbound_chat(&self, chat_jid: &str) -> Result<u32> {
        let jid = chat_jid.to_owned();
        self.run(move |c| {
            let n = c
                .execute(
                    "DELETE FROM inbound_messages WHERE chat_jid = ?1",
                    params![jid],
                )
                .map_err(db_err)? as u32;
            Ok(n)
        })
        .await
    }

    /// Delete a single inbound message by its message ID.
    pub async fn delete_inbound_message(&self, message_id: &str) -> Result<u32> {
        let mid = message_id.to_owned();
        self.run(move |c| {
            let n = c
                .execute(
                    "DELETE FROM inbound_messages WHERE message_id = ?1",
                    params![mid],
                )
                .map_err(db_err)? as u32;
            Ok(n)
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Database pruning — prevent unbounded growth
    // -----------------------------------------------------------------------

    /// Prune old data from the database. Returns counts of deleted rows.
    pub async fn prune_old_data(&self, sent_retention_secs: i64, inbound_retention_secs: i64) -> Result<PruneStats> {
        let sent_cutoff = now_secs() - sent_retention_secs;
        let inbound_cutoff = now_secs() - inbound_retention_secs;
        self.run(move |c| {
            let tx = c.unchecked_transaction().map_err(db_err)?;

            // 1. Delete completed outbound messages older than retention period
            let sent_deleted = tx
                .execute(
                    "DELETE FROM outbound_queue WHERE status IN ('sent', 'failed') AND updated_at < ?1",
                    params![sent_cutoff],
                )
                .map_err(db_err)? as u32;

            // 2. Delete old inbound history
            let inbound_deleted = tx
                .execute(
                    "DELETE FROM inbound_messages WHERE created_at < ?1",
                    params![inbound_cutoff],
                )
                .map_err(db_err)? as u32;

            tx.commit().map_err(db_err)?;

            // 3. Reclaim disk space progressively (no-op if auto_vacuum != INCREMENTAL)
            let _ = c.execute_batch("PRAGMA incremental_vacuum(500);");

            Ok(PruneStats { sent_deleted, inbound_deleted })
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Backup — timestamped snapshots with rotation
    // -----------------------------------------------------------------------

    /// Create a timestamped backup in `backup_dir`, keeping at most `max_backups`.
    /// Returns the path to the new backup file.
    pub fn perform_backup(&self, backup_dir: &Path, max_backups: usize) -> Result<PathBuf> {
        // Ensure backup directory exists
        std::fs::create_dir_all(backup_dir)
            .map_err(|e| StoreError::Database(format!("create backup dir: {e}")))?;
        secure_backup_permissions(backup_dir)?;

        // Generate timestamped filename
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let filename = format!("whatsapp_backup_{ts}.db");
        let dest_path = backup_dir.join(&filename);

        // Perform the hot backup
        self.snapshot_db(&dest_path)?;
        secure_backup_permissions(&dest_path)?;

        // Rotate: list backups, sort by name, delete oldest if over limit
        if let Ok(entries) = std::fs::read_dir(backup_dir) {
            let mut backups: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("whatsapp_backup_") && n.ends_with(".db"))
                        .unwrap_or(false)
                })
                .collect();
            backups.sort();
            while backups.len() > max_backups {
                if let Some(oldest) = backups.first() {
                    let _ = std::fs::remove_file(oldest);
                }
                backups.remove(0);
            }
        }

        Ok(dest_path)
    }

    // -----------------------------------------------------------------------
    // Poll key storage
    // -----------------------------------------------------------------------

    /// Store a poll's encryption key and option names for later vote decryption.
    pub async fn store_poll_key(
        &self,
        chat_jid: &str,
        poll_id: &str,
        enc_key: &[u8],
        options: &[String],
    ) -> Result<()> {
        let chat_jid = chat_jid.to_string();
        let poll_id = poll_id.to_string();
        let enc_key = enc_key.to_vec();
        let options_json = serde_json::to_string(options).map_err(|e| {
            StoreError::Serialization(format!("poll options: {e}"))
        })?;
        self.run(move |c| {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            c.execute(
                "INSERT OR REPLACE INTO poll_keys (chat_jid, poll_id, enc_key, options_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![chat_jid, poll_id, enc_key, options_json, now],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    /// Retrieve a poll's encryption key and option names.
    pub async fn get_poll_key(
        &self,
        chat_jid: &str,
        poll_id: &str,
    ) -> Result<Option<(Vec<u8>, Vec<String>)>> {
        let chat_jid = chat_jid.to_string();
        let poll_id = poll_id.to_string();
        self.run(move |c| {
            let result: Option<(Vec<u8>, String)> = c
                .query_row(
                    "SELECT enc_key, options_json FROM poll_keys WHERE chat_jid = ?1 AND poll_id = ?2",
                    params![chat_jid, poll_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(db_err)?;
            match result {
                Some((enc_key, options_json)) => {
                    let options: Vec<String> = serde_json::from_str(&options_json)
                        .map_err(|e| StoreError::Serialization(format!("poll options: {e}")))?;
                    Ok(Some((enc_key, options)))
                }
                None => Ok(None),
            }
        })
        .await
    }
}

fn run_schema_migrations(conn: &Connection, from_version: i64) -> Result<()> {
    if from_version > CURRENT_SCHEMA_VERSION {
        return Err(StoreError::Database(format!(
            "database schema version {from_version} is newer than supported {CURRENT_SCHEMA_VERSION}"
        )));
    }

    if from_version >= CURRENT_SCHEMA_VERSION {
        return Ok(());
    }

    // Run all migrations inside a single transaction — partial migrations
    // leave the DB in a consistent pre-migration state on failure.
    conn.execute_batch("BEGIN IMMEDIATE;").map_err(db_err)?;

    let result = (|| -> Result<()> {
        if from_version < 1 {
            // v0→v1: initial schema (tables created by SCHEMA execute_batch above).
        }

        if from_version < 2 {
            // v1→v2: outbound_queue table (idempotent — CREATE IF NOT EXISTS in SCHEMA).
        }

        if from_version < 3 {
            // v2→v3: add retry_after column for per-message exponential backoff.
            // Check if column already exists (fresh DBs have it in the initial schema).
            let has_col: bool = conn
                .prepare("SELECT COUNT(*) FROM pragma_table_info('outbound_queue') WHERE name='retry_after'")
                .and_then(|mut s| s.query_row([], |r| r.get::<_, i64>(0)))
                .unwrap_or(0)
                > 0;
            if !has_col {
                conn.execute_batch(
                    "ALTER TABLE outbound_queue ADD COLUMN retry_after INTEGER NOT NULL DEFAULT 0;"
                ).map_err(db_err)?;
            }
        }

        if from_version < 4 {
            // v3→v4: poll_keys table for decrypting incoming poll votes.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS poll_keys (
                    chat_jid TEXT NOT NULL,
                    poll_id TEXT NOT NULL,
                    enc_key BLOB NOT NULL,
                    options_json TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    PRIMARY KEY (chat_jid, poll_id)
                );"
            ).map_err(db_err)?;
        }

        if from_version < 5 {
            // v4→v5: typed outbound job queue — add columns for op_kind, structured
            // payload, WA message ID tracking, and delivery status.
            let add_col = |col: &str, def: &str| -> std::result::Result<(), StoreError> {
                // Check if column exists first (fresh DBs already have it).
                let exists: bool = conn
                    .prepare(&format!(
                        "SELECT COUNT(*) FROM pragma_table_info('outbound_queue') WHERE name='{col}'"
                    ))
                    .and_then(|mut s| s.query_row([], |r| r.get::<_, i64>(0)))
                    .unwrap_or(0)
                    > 0;
                if !exists {
                    conn.execute_batch(&format!(
                        "ALTER TABLE outbound_queue ADD COLUMN {col} {def};"
                    ))
                    .map_err(db_err)?;
                }
                Ok(())
            };
            add_col("op_kind", "TEXT NOT NULL DEFAULT 'text'")?;
            add_col("payload_json", "TEXT NOT NULL DEFAULT '{}'")?;
            add_col("payload_blob", "BLOB")?;
            add_col("wa_message_id", "TEXT")?;
            add_col("delivery_status", "TEXT")?;
            add_col("last_error", "TEXT")?;

            // Migrate existing text rows: copy payload into payload_json
            conn.execute_batch(
                "UPDATE outbound_queue SET payload_json = json_object('text', payload)
                 WHERE op_kind = 'text' AND payload_json = '{}'
                 AND payload != '';"
            ).map_err(db_err)?;

            // Add better index for the typed queue
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_outbound_retry ON outbound_queue(status, retry_after, id);
                 CREATE INDEX IF NOT EXISTS idx_outbound_wa_id ON outbound_queue(wa_message_id);"
            ).map_err(db_err)?;
        }

        if from_version < 6 {
            // v5→v6: inbound message history for search and context
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS inbound_messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    chat_jid TEXT NOT NULL,
                    sender_jid TEXT NOT NULL,
                    message_id TEXT NOT NULL UNIQUE,
                    content_kind TEXT NOT NULL,
                    body_text TEXT,
                    timestamp INTEGER NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_inbound_chat_ts ON inbound_messages(chat_jid, timestamp);
                CREATE INDEX IF NOT EXISTS idx_inbound_msg_id ON inbound_messages(message_id);"
            ).map_err(db_err)?;
        }

        if from_version < 7 {
            // v6→v7: wa-rs v0.5.0 — new ProtocolStore tables + device columns.
            // Replace skdm_recipients with sender_key_devices.
            conn.execute_batch(
                "DROP TABLE IF EXISTS skdm_recipients;
                 DROP TABLE IF EXISTS sender_key_status;

                 CREATE TABLE IF NOT EXISTS sender_key_devices (
                     group_jid TEXT NOT NULL,
                     device_jid TEXT NOT NULL,
                     needs_sender_key INTEGER NOT NULL DEFAULT 1,
                     PRIMARY KEY (group_jid, device_jid)
                 );

                 CREATE TABLE IF NOT EXISTS sent_messages (
                     chat_jid TEXT NOT NULL,
                     message_id TEXT NOT NULL,
                     message_bytes BLOB NOT NULL,
                     timestamp INTEGER NOT NULL,
                     PRIMARY KEY (chat_jid, message_id)
                 );"
            ).map_err(db_err)?;

            // Add new device columns (idempotent check).
            let add_dev_col = |col: &str, def: &str| -> std::result::Result<(), StoreError> {
                let exists: bool = conn
                    .prepare(&format!(
                        "SELECT COUNT(*) FROM pragma_table_info('device') WHERE name='{col}'"
                    ))
                    .and_then(|mut s| s.query_row([], |r| r.get::<_, i64>(0)))
                    .unwrap_or(0)
                    > 0;
                if !exists {
                    conn.execute_batch(&format!(
                        "ALTER TABLE device ADD COLUMN {col} {def};"
                    ))
                    .map_err(db_err)?;
                }
                Ok(())
            };
            add_dev_col("next_pre_key_id", "INTEGER NOT NULL DEFAULT 0")?;
            add_dev_col("nct_salt", "BLOB")?;
        }

        conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)
            .map_err(db_err)?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT;").map_err(db_err)?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn serialize_keypair(kp: &KeyPair) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(kp.private_key.serialize());
    buf.extend_from_slice(kp.public_key.public_key_bytes());
    buf
}

fn deserialize_keypair(bytes: &[u8]) -> Result<KeyPair> {
    if bytes.len() != 64 {
        return Err(StoreError::Serialization(format!(
            "keypair: expected 64 bytes, got {}",
            bytes.len()
        )));
    }
    let private = PrivateKey::deserialize(&bytes[..32])
        .map_err(|e| StoreError::Serialization(e.to_string()))?;
    let public = PublicKey::from_djb_public_key_bytes(&bytes[32..])
        .map_err(|e| StoreError::Serialization(e.to_string()))?;
    Ok(KeyPair::new(public, private))
}

pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Save a Device to the database (shared by DeviceStore::save and ::create).
fn save_device_to_db(conn: &Connection, device: &Device) -> Result<()> {
    let pn = device
        .pn
        .as_ref()
        .map(|j| j.to_string())
        .unwrap_or_default();
    let lid = device
        .lid
        .as_ref()
        .map(|j| j.to_string())
        .unwrap_or_default();
    let noise = serialize_keypair(&device.noise_key);
    let identity = serialize_keypair(&device.identity_key);
    let spk = serialize_keypair(&device.signed_pre_key);
    let account = device.account.as_ref().map(|a| a.encode_to_vec());

    conn.execute(
        "INSERT INTO device (id, pn, lid, registration_id, noise_key, identity_key,
         signed_pre_key, signed_pre_key_id, signed_pre_key_signature, adv_secret_key,
         account, push_name, app_version_primary, app_version_secondary,
         app_version_tertiary, app_version_last_fetched_ms, edge_routing_info, props_hash,
         next_pre_key_id, nct_salt)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
         ON CONFLICT(id) DO UPDATE SET
            pn=excluded.pn, lid=excluded.lid, registration_id=excluded.registration_id,
            noise_key=excluded.noise_key, identity_key=excluded.identity_key,
            signed_pre_key=excluded.signed_pre_key, signed_pre_key_id=excluded.signed_pre_key_id,
            signed_pre_key_signature=excluded.signed_pre_key_signature,
            adv_secret_key=excluded.adv_secret_key, account=excluded.account,
            push_name=excluded.push_name,
            app_version_primary=excluded.app_version_primary,
            app_version_secondary=excluded.app_version_secondary,
            app_version_tertiary=excluded.app_version_tertiary,
            app_version_last_fetched_ms=excluded.app_version_last_fetched_ms,
            edge_routing_info=excluded.edge_routing_info,
            props_hash=excluded.props_hash,
            next_pre_key_id=excluded.next_pre_key_id,
            nct_salt=excluded.nct_salt",
        params![
            pn,
            lid,
            device.registration_id as i64,
            noise,
            identity,
            spk,
            device.signed_pre_key_id as i64,
            device.signed_pre_key_signature,
            device.adv_secret_key,
            account,
            device.push_name,
            device.app_version_primary as i64,
            device.app_version_secondary as i64,
            device.app_version_tertiary as i64,
            device.app_version_last_fetched_ms,
            device.edge_routing_info,
            device.props_hash,
            device.next_pre_key_id as i64,
            device.nct_salt,
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

/// Load a Device from the database.
fn load_device_from_db(conn: &Connection) -> Result<Option<Device>> {
    let row = conn
        .query_row(
            "SELECT pn, lid, registration_id, noise_key, identity_key, signed_pre_key,
             signed_pre_key_id, signed_pre_key_signature, adv_secret_key, account,
             push_name, app_version_primary, app_version_secondary, app_version_tertiary,
             app_version_last_fetched_ms, edge_routing_info, props_hash,
             next_pre_key_id, nct_salt
             FROM device WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Vec<u8>>(7)?,
                    row.get::<_, Vec<u8>>(8)?,
                    row.get::<_, Option<Vec<u8>>>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, i64>(13)?,
                    row.get::<_, i64>(14)?,
                    row.get::<_, Option<Vec<u8>>>(15)?,
                    row.get::<_, Option<String>>(16)?,
                    row.get::<_, i64>(17)?,
                    row.get::<_, Option<Vec<u8>>>(18)?,
                ))
            },
        )
        .optional()
        .map_err(db_err)?;

    let Some((
        pn_s,
        lid_s,
        reg_id,
        noise_b,
        ident_b,
        spk_b,
        spk_id,
        spk_sig,
        adv,
        account_b,
        push_name,
        v1,
        v2,
        v3,
        v_ts,
        eri,
        ph,
        npk_id_raw,
        nct_raw,
    )) = row
    else {
        return Ok(None);
    };

    let to_u32 = |v: i64, field: &str| -> Result<u32> {
        u32::try_from(v)
            .map_err(|_| StoreError::Serialization(format!("{field}: value {v} out of u32 range")))
    };
    let to_fixed = |v: Vec<u8>, field: &str, expected: usize| -> Result<Vec<u8>> {
        if v.len() == expected {
            Ok(v)
        } else {
            Err(StoreError::Serialization(format!(
                "{field}: expected {expected} bytes, got {}",
                v.len()
            )))
        }
    };
    let npk_id = to_u32(npk_id_raw, "next_pre_key_id")?;
    let nct = nct_raw;

    Ok(Some(Device {
        pn: if pn_s.is_empty() {
            None
        } else {
            Jid::from_str(&pn_s).ok()
        },
        lid: if lid_s.is_empty() {
            None
        } else {
            Jid::from_str(&lid_s).ok()
        },
        registration_id: to_u32(reg_id, "registration_id")?,
        noise_key: deserialize_keypair(&noise_b)?,
        identity_key: deserialize_keypair(&ident_b)?,
        signed_pre_key: deserialize_keypair(&spk_b)?,
        signed_pre_key_id: to_u32(spk_id, "signed_pre_key_id")?,
        signed_pre_key_signature: {
            let bytes = to_fixed(spk_sig, "signed_pre_key_signature", 64)?;
            let mut fixed = [0u8; 64];
            fixed.copy_from_slice(&bytes);
            fixed
        },
        adv_secret_key: {
            let bytes = to_fixed(adv, "adv_secret_key", 32)?;
            let mut fixed = [0u8; 32];
            fixed.copy_from_slice(&bytes);
            fixed
        },
        account: account_b
            .map(|b| wa::AdvSignedDeviceIdentity::decode(b.as_slice()))
            .transpose()
            .map_err(|e| StoreError::Serialization(e.to_string()))?,
        push_name,
        app_version_primary: to_u32(v1, "app_version_primary")?,
        app_version_secondary: to_u32(v2, "app_version_secondary")?,
        app_version_tertiary: to_u32(v3, "app_version_tertiary")?,
        app_version_last_fetched_ms: v_ts,
        device_props: DEVICE_PROPS.clone(),
        edge_routing_info: eri,
        props_hash: ph,
        next_pre_key_id: npk_id,
        nct_salt: nct,
        nct_salt_sync_seen: false,
    }))
}

// ===========================================================================
// SignalStore — identity keys, sessions, pre-keys, sender keys
// ===========================================================================

#[async_trait]
impl SignalStore for Store {
    async fn put_identity(&self, address: &str, key: [u8; 32]) -> Result<()> {
        let addr = address.to_owned();
        self.run(move |c| {
            c.execute(
                "INSERT INTO identities (address, key) VALUES (?1, ?2)
                 ON CONFLICT(address) DO UPDATE SET key = excluded.key",
                params![addr, key.as_slice()],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn load_identity(&self, address: &str) -> Result<Option<Vec<u8>>> {
        let addr = address.to_owned();
        self.run(move |c| {
            c.query_row(
                "SELECT key FROM identities WHERE address = ?1",
                params![addr],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn delete_identity(&self, address: &str) -> Result<()> {
        let addr = address.to_owned();
        self.run(move |c| {
            c.execute("DELETE FROM identities WHERE address = ?1", params![addr])
                .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_session(&self, address: &str) -> Result<Option<Vec<u8>>> {
        let addr = address.to_owned();
        self.run(move |c| {
            c.query_row(
                "SELECT record FROM sessions WHERE address = ?1",
                params![addr],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn put_session(&self, address: &str, session: &[u8]) -> Result<()> {
        let addr = address.to_owned();
        let data = session.to_vec();
        self.run(move |c| {
            c.execute(
                "INSERT INTO sessions (address, record) VALUES (?1, ?2)
                 ON CONFLICT(address) DO UPDATE SET record = excluded.record",
                params![addr, data],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn delete_session(&self, address: &str) -> Result<()> {
        let addr = address.to_owned();
        self.run(move |c| {
            c.execute("DELETE FROM sessions WHERE address = ?1", params![addr])
                .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn store_prekey(&self, id: u32, record: &[u8], uploaded: bool) -> Result<()> {
        let data = record.to_vec();
        self.run(move |c| {
            c.execute(
                "INSERT INTO prekeys (id, key, uploaded) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET key = excluded.key, uploaded = excluded.uploaded",
                params![id, data, uploaded],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn load_prekey(&self, id: u32) -> Result<Option<Vec<u8>>> {
        self.run(move |c| {
            c.query_row(
                "SELECT key FROM prekeys WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn remove_prekey(&self, id: u32) -> Result<()> {
        self.run(move |c| {
            c.execute("DELETE FROM prekeys WHERE id = ?1", params![id])
                .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn store_signed_prekey(&self, id: u32, record: &[u8]) -> Result<()> {
        let data = record.to_vec();
        self.run(move |c| {
            c.execute(
                "INSERT INTO signed_prekeys (id, record) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET record = excluded.record",
                params![id, data],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn load_signed_prekey(&self, id: u32) -> Result<Option<Vec<u8>>> {
        self.run(move |c| {
            c.query_row(
                "SELECT record FROM signed_prekeys WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn load_all_signed_prekeys(&self) -> Result<Vec<(u32, Vec<u8>)>> {
        self.run(|c| {
            let mut stmt = c
                .prepare("SELECT id, record FROM signed_prekeys")
                .map_err(db_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, u32>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .map_err(db_err)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(db_err)?);
            }
            Ok(out)
        })
        .await
    }

    async fn remove_signed_prekey(&self, id: u32) -> Result<()> {
        self.run(move |c| {
            c.execute("DELETE FROM signed_prekeys WHERE id = ?1", params![id])
                .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_max_prekey_id(&self) -> Result<u32> {
        self.run(|c| {
            let id: Option<u32> = c
                .query_row("SELECT MAX(id) FROM prekeys", [], |r| r.get(0))
                .optional()
                .map_err(db_err)?
                .flatten();
            Ok(id.unwrap_or(0))
        })
        .await
    }

    async fn put_sender_key(&self, address: &str, record: &[u8]) -> Result<()> {
        let addr = address.to_owned();
        let data = record.to_vec();
        self.run(move |c| {
            c.execute(
                "INSERT INTO sender_keys (address, record) VALUES (?1, ?2)
                 ON CONFLICT(address) DO UPDATE SET record = excluded.record",
                params![addr, data],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_sender_key(&self, address: &str) -> Result<Option<Vec<u8>>> {
        let addr = address.to_owned();
        self.run(move |c| {
            c.query_row(
                "SELECT record FROM sender_keys WHERE address = ?1",
                params![addr],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn delete_sender_key(&self, address: &str) -> Result<()> {
        let addr = address.to_owned();
        self.run(move |c| {
            c.execute("DELETE FROM sender_keys WHERE address = ?1", params![addr])
                .map_err(db_err)?;
            Ok(())
        })
        .await
    }
}

// ===========================================================================
// AppSyncStore — app state keys, versions, mutation MACs
// ===========================================================================

#[async_trait]
impl AppSyncStore for Store {
    async fn get_sync_key(&self, key_id: &[u8]) -> Result<Option<AppStateSyncKey>> {
        let kid = key_id.to_vec();
        self.run(move |c| {
            c.query_row(
                "SELECT key_data, fingerprint, timestamp FROM app_state_keys WHERE key_id = ?1",
                params![kid],
                |row| {
                    Ok(AppStateSyncKey {
                        key_data: row.get(0)?,
                        fingerprint: row.get(1)?,
                        timestamp: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn set_sync_key(&self, key_id: &[u8], key: AppStateSyncKey) -> Result<()> {
        let kid = key_id.to_vec();
        self.run(move |c| {
            c.execute(
                "INSERT INTO app_state_keys (key_id, key_data, fingerprint, timestamp)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(key_id) DO UPDATE SET
                    key_data=excluded.key_data, fingerprint=excluded.fingerprint,
                    timestamp=excluded.timestamp",
                params![kid, key.key_data, key.fingerprint, key.timestamp],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_version(&self, name: &str) -> Result<HashState> {
        let n = name.to_owned();
        self.run(move |c| {
            let opt: Option<Vec<u8>> = c
                .query_row(
                    "SELECT state_data FROM app_state_versions WHERE name = ?1",
                    params![n],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db_err)?;
            match opt {
                Some(data) => serde_json::from_slice(&data)
                    .map_err(|e| StoreError::Serialization(e.to_string())),
                None => Ok(HashState::default()),
            }
        })
        .await
    }

    async fn set_version(&self, name: &str, state: HashState) -> Result<()> {
        let n = name.to_owned();
        let data =
            serde_json::to_vec(&state).map_err(|e| StoreError::Serialization(e.to_string()))?;
        self.run(move |c| {
            c.execute(
                "INSERT INTO app_state_versions (name, state_data) VALUES (?1, ?2)
                 ON CONFLICT(name) DO UPDATE SET state_data = excluded.state_data",
                params![n, data],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn put_mutation_macs(
        &self,
        name: &str,
        version: u64,
        mutations: &[AppStateMutationMAC],
    ) -> Result<()> {
        let n = name.to_owned();
        let macs: Vec<(Vec<u8>, Vec<u8>)> = mutations
            .iter()
            .map(|m| (m.index_mac.clone(), m.value_mac.clone()))
            .collect();
        self.run(move |c| {
            let tx = c.unchecked_transaction().map_err(db_err)?;
            for (index_mac, value_mac) in &macs {
                tx.execute(
                    "INSERT INTO app_state_mutation_macs (name, index_mac, version, value_mac)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(name, index_mac) DO UPDATE SET
                        version=excluded.version, value_mac=excluded.value_mac",
                    params![n, index_mac, version as i64, value_mac],
                )
                .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)
        })
        .await
    }

    async fn get_mutation_mac(&self, name: &str, index_mac: &[u8]) -> Result<Option<Vec<u8>>> {
        let n = name.to_owned();
        let im = index_mac.to_vec();
        self.run(move |c| {
            c.query_row(
                "SELECT value_mac FROM app_state_mutation_macs
                 WHERE name = ?1 AND index_mac = ?2",
                params![n, im],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn delete_mutation_macs(&self, name: &str, index_macs: &[Vec<u8>]) -> Result<()> {
        let n = name.to_owned();
        let macs = index_macs.to_vec();
        self.run(move |c| {
            let tx = c.unchecked_transaction().map_err(db_err)?;
            for mac in &macs {
                tx.execute(
                    "DELETE FROM app_state_mutation_macs WHERE name = ?1 AND index_mac = ?2",
                    params![n, mac],
                )
                .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)
        })
        .await
    }

    async fn get_latest_sync_key_id(&self) -> Result<Option<Vec<u8>>> {
        self.run(|c| {
            c.query_row(
                "SELECT key_id FROM app_state_keys ORDER BY rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }
}

// ===========================================================================
// ProtocolStore — SKDM, LID mapping, base keys, device registry, tc tokens
// ===========================================================================

#[async_trait]
impl ProtocolStore for Store {
    async fn get_sender_key_devices(&self, group_jid: &str) -> Result<Vec<(String, bool)>> {
        let gj = group_jid.to_owned();
        self.run(move |c| {
            let mut stmt = c
                .prepare(
                    "SELECT device_jid, needs_sender_key FROM sender_key_devices WHERE group_jid = ?1",
                )
                .map_err(db_err)?;
            let rows = stmt
                .query_map(params![gj], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))
                })
                .map_err(db_err)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(db_err)
        })
        .await
    }

    async fn set_sender_key_status(&self, group_jid: &str, entries: &[(&str, bool)]) -> Result<()> {
        let gj = group_jid.to_owned();
        let owned: Vec<(String, bool)> = entries.iter().map(|(s, b)| (s.to_string(), *b)).collect();
        self.run(move |c| {
            let tx = c.unchecked_transaction().map_err(db_err)?;
            for (device_jid, needs) in &owned {
                tx.execute(
                    "INSERT INTO sender_key_devices (group_jid, device_jid, needs_sender_key) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(group_jid, device_jid) DO UPDATE SET needs_sender_key = ?3",
                    params![gj, device_jid, needs],
                )
                .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)
        })
        .await
    }

    async fn clear_sender_key_devices(&self, group_jid: &str) -> Result<()> {
        let gj = group_jid.to_owned();
        self.run(move |c| {
            c.execute(
                "DELETE FROM sender_key_devices WHERE group_jid = ?1",
                params![gj],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_lid_mapping(&self, lid: &str) -> Result<Option<LidPnMappingEntry>> {
        let l = lid.to_owned();
        self.run(move |c| {
            c.query_row(
                "SELECT lid, phone_number, created_at, updated_at, learning_source
                 FROM lid_pn_mapping WHERE lid = ?1",
                params![l],
                |row| {
                    Ok(LidPnMappingEntry {
                        lid: row.get(0)?,
                        phone_number: row.get(1)?,
                        created_at: row.get(2)?,
                        updated_at: row.get(3)?,
                        learning_source: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn get_pn_mapping(&self, phone: &str) -> Result<Option<LidPnMappingEntry>> {
        let p = phone.to_owned();
        self.run(move |c| {
            c.query_row(
                "SELECT lid, phone_number, created_at, updated_at, learning_source
                 FROM lid_pn_mapping WHERE phone_number = ?1
                 ORDER BY updated_at DESC LIMIT 1",
                params![p],
                |row| {
                    Ok(LidPnMappingEntry {
                        lid: row.get(0)?,
                        phone_number: row.get(1)?,
                        created_at: row.get(2)?,
                        updated_at: row.get(3)?,
                        learning_source: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn put_lid_mapping(&self, entry: &LidPnMappingEntry) -> Result<()> {
        let e = entry.clone();
        self.run(move |c| {
            c.execute(
                "INSERT INTO lid_pn_mapping (lid, phone_number, created_at, updated_at, learning_source)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(lid) DO UPDATE SET
                    phone_number=excluded.phone_number, updated_at=excluded.updated_at,
                    learning_source=excluded.learning_source",
                params![e.lid, e.phone_number, e.created_at, e.updated_at, e.learning_source],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_all_lid_mappings(&self) -> Result<Vec<LidPnMappingEntry>> {
        self.run(|c| {
            let mut stmt = c
                .prepare(
                    "SELECT lid, phone_number, created_at, updated_at, learning_source
                     FROM lid_pn_mapping",
                )
                .map_err(db_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(LidPnMappingEntry {
                        lid: row.get(0)?,
                        phone_number: row.get(1)?,
                        created_at: row.get(2)?,
                        updated_at: row.get(3)?,
                        learning_source: row.get(4)?,
                    })
                })
                .map_err(db_err)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(db_err)
        })
        .await
    }

    async fn save_base_key(&self, address: &str, message_id: &str, base_key: &[u8]) -> Result<()> {
        let addr = address.to_owned();
        let mid = message_id.to_owned();
        let bk = base_key.to_vec();
        self.run(move |c| {
            c.execute(
                "INSERT INTO base_keys (address, message_id, base_key) VALUES (?1, ?2, ?3)
                 ON CONFLICT(address, message_id) DO UPDATE SET base_key = excluded.base_key",
                params![addr, mid, bk],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn has_same_base_key(
        &self,
        address: &str,
        message_id: &str,
        current_base_key: &[u8],
    ) -> Result<bool> {
        let addr = address.to_owned();
        let mid = message_id.to_owned();
        let cbk = current_base_key.to_vec();
        self.run(move |c| {
            let stored: Option<Vec<u8>> = c
                .query_row(
                    "SELECT base_key FROM base_keys WHERE address = ?1 AND message_id = ?2",
                    params![addr, mid],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db_err)?;
            Ok(stored.map_or(false, |s| s == cbk))
        })
        .await
    }

    async fn delete_base_key(&self, address: &str, message_id: &str) -> Result<()> {
        let addr = address.to_owned();
        let mid = message_id.to_owned();
        self.run(move |c| {
            c.execute(
                "DELETE FROM base_keys WHERE address = ?1 AND message_id = ?2",
                params![addr, mid],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn update_device_list(&self, record: DeviceListRecord) -> Result<()> {
        let devices_json = serde_json::to_string(&record.devices)
            .map_err(|e| StoreError::Serialization(e.to_string()))?;
        self.run(move |c| {
            c.execute(
                "INSERT INTO device_registry (user_id, devices_json, timestamp, phash)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(user_id) DO UPDATE SET
                    devices_json=excluded.devices_json, timestamp=excluded.timestamp,
                    phash=excluded.phash",
                params![record.user, devices_json, record.timestamp, record.phash],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_devices(&self, user: &str) -> Result<Option<DeviceListRecord>> {
        let u = user.to_owned();
        self.run(move |c| {
            c.query_row(
                "SELECT user_id, devices_json, timestamp, phash FROM device_registry
                 WHERE user_id = ?1",
                params![u],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(db_err)?
            .map(|(user, dj, ts, ph)| {
                let devices = serde_json::from_str(&dj)
                    .map_err(|e| StoreError::Serialization(e.to_string()))?;
                Ok(DeviceListRecord {
                    user,
                    devices,
                    timestamp: ts,
                    phash: ph,
                })
            })
            .transpose()
        })
        .await
    }

    async fn store_sent_message(
        &self,
        chat_jid: &str,
        message_id: &str,
        payload: &[u8],
    ) -> Result<()> {
        let cj = chat_jid.to_owned();
        let mid = message_id.to_owned();
        let bytes = payload.to_vec();
        let ts = now_secs();
        self.run(move |c| {
            c.execute(
                "INSERT OR REPLACE INTO sent_messages (chat_jid, message_id, message_bytes, timestamp) VALUES (?1, ?2, ?3, ?4)",
                params![cj, mid, bytes, ts],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn take_sent_message(&self, chat_jid: &str, message_id: &str) -> Result<Option<Vec<u8>>> {
        let cj = chat_jid.to_owned();
        let mid = message_id.to_owned();
        self.run(move |c| {
            let result: Option<Vec<u8>> = c
                .query_row(
                    "SELECT message_bytes FROM sent_messages WHERE chat_jid = ?1 AND message_id = ?2",
                    params![cj, mid],
                    |r| r.get(0),
                )
                .optional()
                .map_err(db_err)?;
            if result.is_some() {
                c.execute(
                    "DELETE FROM sent_messages WHERE chat_jid = ?1 AND message_id = ?2",
                    params![cj, mid],
                )
                .map_err(db_err)?;
            }
            Ok(result)
        })
        .await
    }

    async fn delete_expired_sent_messages(&self, cutoff_timestamp: i64) -> Result<u32> {
        self.run(move |c| {
            let count = c
                .execute(
                    "DELETE FROM sent_messages WHERE timestamp < ?1",
                    params![cutoff_timestamp],
                )
                .map_err(db_err)?;
            u32::try_from(count)
                .map_err(|_| StoreError::Database(format!("delete count {count} out of u32 range")))
        })
        .await
    }

    async fn get_tc_token(&self, jid: &str) -> Result<Option<TcTokenEntry>> {
        let j = jid.to_owned();
        self.run(move |c| {
            c.query_row(
                "SELECT token, token_timestamp, sender_timestamp FROM tc_tokens WHERE jid = ?1",
                params![j],
                |row| {
                    Ok(TcTokenEntry {
                        token: row.get(0)?,
                        token_timestamp: row.get(1)?,
                        sender_timestamp: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(db_err)
        })
        .await
    }

    async fn put_tc_token(&self, jid: &str, entry: &TcTokenEntry) -> Result<()> {
        let j = jid.to_owned();
        let e = entry.clone();
        self.run(move |c| {
            c.execute(
                "INSERT INTO tc_tokens (jid, token, token_timestamp, sender_timestamp)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(jid) DO UPDATE SET
                    token=excluded.token, token_timestamp=excluded.token_timestamp,
                    sender_timestamp=excluded.sender_timestamp",
                params![j, e.token, e.token_timestamp, e.sender_timestamp],
            )
            .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn delete_tc_token(&self, jid: &str) -> Result<()> {
        let j = jid.to_owned();
        self.run(move |c| {
            c.execute("DELETE FROM tc_tokens WHERE jid = ?1", params![j])
                .map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn get_all_tc_token_jids(&self) -> Result<Vec<String>> {
        self.run(|c| {
            let mut stmt = c.prepare("SELECT jid FROM tc_tokens").map_err(db_err)?;
            let rows = stmt.query_map([], |row| row.get(0)).map_err(db_err)?;
            rows.collect::<std::result::Result<_, _>>().map_err(db_err)
        })
        .await
    }

    async fn delete_expired_tc_tokens(&self, cutoff_timestamp: i64) -> Result<u32> {
        self.run(move |c| {
            let count = c
                .execute(
                    "DELETE FROM tc_tokens WHERE token_timestamp < ?1",
                    params![cutoff_timestamp],
                )
                .map_err(db_err)?;
            u32::try_from(count)
                .map_err(|_| StoreError::Database(format!("delete count {count} out of u32 range")))
        })
        .await
    }
}

// ===========================================================================
// DeviceStore — device persistence
// ===========================================================================

#[async_trait]
impl DeviceStore for Store {
    async fn save(&self, device: &Device) -> Result<()> {
        let d = device.clone();
        self.run(move |c| save_device_to_db(c, &d)).await
    }

    async fn load(&self) -> Result<Option<Device>> {
        self.run(load_device_from_db).await
    }

    async fn exists(&self) -> Result<bool> {
        self.run(|c| {
            let count: i64 = c
                .query_row("SELECT COUNT(*) FROM device WHERE id = 1", [], |row| {
                    row.get(0)
                })
                .map_err(db_err)?;
            Ok(count > 0)
        })
        .await
    }

    async fn create(&self) -> Result<i32> {
        self.run(|c| {
            // Guard: never overwrite an existing device (would rotate identity keys)
            let count: i64 = c
                .query_row("SELECT COUNT(*) FROM device WHERE id = 1", [], |row| {
                    row.get(0)
                })
                .map_err(db_err)?;
            if count > 0 {
                return Ok(1); // Already exists — no-op
            }
            let device = Device::new();
            save_device_to_db(c, &device)?;
            Ok(1)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_dir(name: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("whatsrust-{name}-{ts}"))
    }

    #[test]
    fn test_perform_backup_creates_backup_file() {
        let root = unique_test_dir("backup");
        let db_path = root.join("whatsapp.db");
        let backup_dir = root.join("backups");
        std::fs::create_dir_all(&root).unwrap();

        let store = Store::new(&db_path).unwrap();
        let backup_path = store.perform_backup(&backup_dir, 3).unwrap();

        assert!(backup_path.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn test_perform_backup_hardens_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_test_dir("backup-perms");
        let db_path = root.join("whatsapp.db");
        let backup_dir = root.join("backups");
        std::fs::create_dir_all(&root).unwrap();

        let store = Store::new(&db_path).unwrap();
        let backup_path = store.perform_backup(&backup_dir, 3).unwrap();

        let dir_mode = std::fs::metadata(&backup_dir).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(&backup_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);

        let _ = std::fs::remove_dir_all(&root);
    }
}
