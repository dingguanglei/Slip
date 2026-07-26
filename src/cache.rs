use crate::core::{MailConfig, MessageSummary};
use crate::protocol::MediaKind;
use crate::providers::{Endpoint, Security, provider_by_id};
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
#[cfg(unix)]
use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt};

#[derive(Clone, Debug)]
pub struct MailCache {
    root: PathBuf,
    /// Serializes read-modify-write access to the JSON store so the TUI's
    /// worker and watcher threads never lose updates. Clones of one
    /// `MailCache` share this lock (it is an `Arc`).
    store_lock: Arc<Mutex<()>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CachedMessage {
    pub mailbox: String,
    pub uid: u32,
    pub fetched_at: u64,
    pub message: MessageSummary,
}

/// login.json, version 2: full endpoint configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CachedLogin {
    #[serde(default)]
    pub version: u32,
    pub provider: String,
    pub address: String,
    pub secret: String,
    pub imap: Endpoint,
    pub smtp: Endpoint,
    #[serde(default)]
    pub allow_invalid_certs: bool,
    #[serde(default)]
    pub needs_imap_id: bool,
    #[serde(default = "default_slip_folder")]
    pub slip_folder: String,
    #[serde(default = "default_subject")]
    pub subject: String,
    pub saved_at: u64,
}

fn default_slip_folder() -> String {
    crate::core::DEFAULT_SLIP_FOLDER.to_string()
}

fn default_subject() -> String {
    crate::core::CHAT_SUBJECT.to_string()
}

/// login.json as written by the QQ/Gmail-only versions of Slip.
#[derive(Clone, Debug, Deserialize)]
struct LegacyCachedLogin {
    provider: String,
    address: String,
    auth_code: String,
    host: Option<String>,
    port: Option<u16>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CachedSearchHit {
    pub mailbox: String,
    pub uid: u32,
    pub subject: String,
    pub from: String,
    pub date: String,
    pub score: usize,
    pub excerpt: String,
}

/// Delivery state of an outgoing message.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryStatus {
    Sending,
    #[default]
    Sent,
    Failed,
}

/// One media item stored with a chat line.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StoredMedia {
    pub kind: MediaKind,
    pub name: String,
    pub mime: String,
    pub size: u64,
    /// Local copy under `~/.slip/attachments`.
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoredChatLine {
    pub sender: String,
    pub date: String,
    #[serde(default)]
    pub timestamp: i64,
    pub body: String,
    #[serde(default)]
    pub attachments: Vec<String>,
    /// Slip message id (32 hex). Empty for legacy v0 lines.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub status: DeliveryStatus,
    #[serde(default)]
    pub encrypted: bool,
    #[serde(default)]
    pub media: Vec<StoredMedia>,
    /// Integrity/trust notes: "key-changed", "hash-mismatch:<name>",
    /// "unreadable:<reason>", "gave-up", …
    #[serde(default)]
    pub flags: Vec<String>,
    /// Automatic-resend bookkeeping for a failed outgoing message.
    #[serde(default)]
    pub retry_count: u32,
    /// Earliest epoch-second at which this failed message may be retried.
    #[serde(default)]
    pub next_retry_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoredSession {
    pub contact: String,
    pub updated_at: u64,
    pub messages: Vec<StoredChatLine>,
}

impl Default for MailCache {
    fn default() -> Self {
        let root = env::var_os("SLIP_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("SLIP_MAIL_CACHE_DIR").map(PathBuf::from))
            .or_else(|| env::var_os("HOME").map(|path| PathBuf::from(path).join(".slip")))
            .or_else(|| env::var_os("XDG_CACHE_HOME").map(|path| PathBuf::from(path).join("slip")))
            .unwrap_or_else(|| PathBuf::from(".slip-cache"));
        Self {
            root,
            store_lock: Arc::new(Mutex::new(())),
        }
    }
}

impl MailCache {
    /// Construct a cache rooted at an explicit directory (tests).
    #[cfg(test)]
    fn at(root: PathBuf) -> Self {
        Self {
            root,
            store_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Acquire the store lock. Every read-modify-write of a JSON file in the
    /// store must run while holding this, so concurrent threads never lose
    /// updates. Poisoning is ignored (a panicked writer leaves the store in a
    /// consistent-enough state; the next atomic write repairs it).
    pub fn lock(&self) -> MutexGuard<'_, ()> {
        self.store_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // ------------------------------------------------------------------
    // SQLite store (slip.db): conversations + small key/value blobs
    // ------------------------------------------------------------------

    fn db_path(&self) -> PathBuf {
        self.root.join("slip.db")
    }

    /// Open the store, apply pragmas + schema, run the one-time JSON import,
    /// then hand the connection to `body`. A fresh connection per call keeps
    /// `MailCache` cheaply cloneable and thread-safe (SQLite's own file lock
    /// plus `busy_timeout` handle cross-process contention); the in-process
    /// `store_lock` still serializes multi-step read-modify-write sequences.
    fn with_conn<T>(&self, body: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        fs::create_dir_all(&self.root)
            .with_context(|| format!("create store dir {}", self.root.display()))?;
        let conn = Connection::open(self.db_path())
            .with_context(|| format!("open store {}", self.db_path().display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                 seq       INTEGER PRIMARY KEY AUTOINCREMENT,
                 contact   TEXT NOT NULL,
                 msg_id    TEXT NOT NULL DEFAULT '',
                 timestamp INTEGER NOT NULL DEFAULT 0,
                 blob      TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_messages_contact
                 ON messages(contact, timestamp, seq);
             CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_ident
                 ON messages(contact, msg_id) WHERE msg_id <> '';
             CREATE TABLE IF NOT EXISTS kv (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
        self.migrate_legacy_json(&conn)?;
        body(&conn)
    }

    /// Read a JSON blob stored under `key` in the kv table.
    fn get_kv<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        self.with_conn(|conn| {
            let raw: Option<String> = conn
                .query_row("SELECT value FROM kv WHERE key = ?1", params![key], |row| {
                    row.get(0)
                })
                .optional()?;
            match raw {
                Some(text) => Ok(Some(
                    serde_json::from_str(&text).with_context(|| format!("parse stored '{key}'"))?,
                )),
                None => Ok(None),
            }
        })
    }

    /// Write `value` as a JSON blob under `key` in the kv table.
    fn put_kv<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO kv(key, value) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, serde_json::to_string(value)?],
            )?;
            Ok(())
        })
    }

    /// One-time import of the pre-SQLite JSON files into `conn`. Guarded by a
    /// kv marker so it runs at most once; imported files are renamed to
    /// `*.migrated` (kept, not deleted, so the move is reversible).
    fn migrate_legacy_json(&self, conn: &Connection) -> Result<()> {
        let already: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM kv WHERE key = 'meta:migrated' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if already.is_some() {
            return Ok(());
        }

        // Conversations: sessions/<contact>.json -> messages rows.
        let sessions_dir = self.sessions_dir();
        if sessions_dir.is_dir() {
            for entry in fs::read_dir(&sessions_dir)? {
                let path = entry?.path();
                if path.extension().and_then(|value| value.to_str()) != Some("json") {
                    continue;
                }
                let bytes = fs::read(&path)
                    .with_context(|| format!("read legacy session {}", path.display()))?;
                let session: StoredSession = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parse legacy session {}", path.display()))?;
                let mut insert = conn.prepare(
                    "INSERT INTO messages(contact, msg_id, timestamp, blob) \
                     VALUES (?1, ?2, ?3, ?4)",
                )?;
                for line in &session.messages {
                    insert.execute(params![
                        session.contact,
                        line.id,
                        line.timestamp,
                        serde_json::to_string(line)?
                    ])?;
                }
            }
        }

        // Small maps: state / peers / contacts / aliases -> kv blobs.
        migrate_json_file(conn, "state", &self.state_path())?;
        migrate_json_file(conn, "peers", &self.peers_path())?;
        migrate_json_file(conn, "contacts", &self.contacts_path())?;
        migrate_json_file(conn, "aliases", &self.aliases_path())?;

        conn.execute(
            "INSERT INTO kv(key, value) VALUES ('meta:migrated', ?1)",
            params![now_epoch_seconds().to_string()],
        )?;

        // Rename imported files aside so they are not re-read (marker already
        // prevents that) and the old layout is preserved for rollback.
        rename_aside(&sessions_dir);
        rename_aside(&self.state_path());
        rename_aside(&self.peers_path());
        rename_aside(&self.contacts_path());
        rename_aside(&self.aliases_path());
        Ok(())
    }

    pub fn attachment_dir(&self, mailbox: &str, uid: u32) -> PathBuf {
        self.root
            .join("attachments")
            .join(format!("{}__{}", safe_component(mailbox), uid))
    }

    pub fn save_outgoing_attachments(
        &self,
        contact: &str,
        timestamp: i64,
        attachments: &[PathBuf],
    ) -> Result<Vec<String>> {
        if attachments.is_empty() {
            return Ok(Vec::new());
        }

        let dir = self.root.join("attachments").join(format!(
            "sent__{}__{}",
            safe_component(contact),
            timestamp
        ));
        fs::create_dir_all(&dir)?;

        let mut saved = Vec::new();
        for (index, path) in attachments.iter().enumerate() {
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .map(safe_component)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| format!("attachment-{}", index + 1));
            let target = dir.join(format!("{:02}-{}", index + 1, name));
            fs::copy(path, &target)
                .with_context(|| format!("copy attachment {}", path.display()))?;
            saved.push(target.display().to_string());
        }
        Ok(saved)
    }

    pub fn save_message(&self, mailbox: &str, message: &MessageSummary) -> Result<()> {
        fs::create_dir_all(self.messages_dir())?;
        let cached = CachedMessage {
            mailbox: mailbox.to_string(),
            uid: message.uid,
            fetched_at: now_epoch_seconds(),
            message: message.clone(),
        };
        let path = self.message_path(mailbox, message.uid);
        fs::write(path, serde_json::to_vec_pretty(&cached)?)?;
        Ok(())
    }

    pub fn load_message(&self, mailbox: &str, uid: u32) -> Result<Option<MessageSummary>> {
        let path = self.message_path(mailbox, uid);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path).with_context(|| format!("read cache {}", path.display()))?;
        let cached: CachedMessage = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse cache {}", path.display()))?;
        Ok(Some(cached.message))
    }

    pub fn save_login(&self, config: &MailConfig) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        let cached = CachedLogin {
            version: 2,
            provider: config.provider_id.clone(),
            address: config.address.clone(),
            secret: config.secret().to_string(),
            imap: config.imap.clone(),
            smtp: config.smtp.clone(),
            allow_invalid_certs: config.allow_invalid_certs,
            needs_imap_id: config.needs_imap_id,
            slip_folder: config.slip_folder.clone(),
            subject: config.subject.clone(),
            saved_at: now_epoch_seconds(),
        };
        let bytes = serde_json::to_vec_pretty(&cached)?;
        write_secret_file(self.login_path(), &bytes)
    }

    pub fn load_login(&self) -> Result<Option<MailConfig>> {
        let path = self.login_path();
        if !path.exists() {
            return Ok(None);
        }
        let bytes =
            fs::read(&path).with_context(|| format!("read login cache {}", path.display()))?;

        if let Ok(cached) = serde_json::from_slice::<CachedLogin>(&bytes)
            && cached.version >= 2
        {
            let mut config =
                MailConfig::custom(cached.address, cached.secret, cached.imap, cached.smtp);
            config.provider_id = cached.provider;
            config.allow_invalid_certs = cached.allow_invalid_certs;
            config.needs_imap_id = cached.needs_imap_id;
            config.slip_folder = cached.slip_folder;
            config.subject = cached.subject;
            return Ok(Some(config));
        }

        // Legacy QQ/Gmail-era login.json: rebuild endpoints from the registry.
        let legacy: LegacyCachedLogin = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse login cache {}", path.display()))?;
        let provider = provider_by_id(&legacy.provider)
            .with_context(|| format!("unknown provider in login cache: {}", legacy.provider))?;
        let mut config = MailConfig::from_provider(provider, legacy.address, legacy.auth_code);
        if let Some(host) = legacy.host {
            config.imap.host = host;
        }
        if let Some(port) = legacy.port {
            config.imap.port = port;
            config.imap.security = if port == 143 {
                Security::Plain
            } else {
                Security::Ssl
            };
        }
        Ok(Some(config))
    }

    pub fn clear_login(&self) -> Result<()> {
        let path = self.login_path();
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("remove login cache {}", path.display()))?;
        }
        Ok(())
    }

    pub fn save_contacts(&self, contacts: &[String]) -> Result<()> {
        let mut contacts = contacts.to_vec();
        contacts.sort();
        contacts.dedup();
        self.put_kv("contacts", &contacts)
    }

    pub fn load_contacts(&self) -> Result<Vec<String>> {
        let mut contacts: Vec<String> = self.get_kv("contacts")?.unwrap_or_default();
        contacts.sort();
        contacts.dedup();
        Ok(contacts)
    }

    /// Local per-contact display names (email -> alias), kept apart from the
    /// plain contact address list.
    pub fn load_aliases(&self) -> Result<HashMap<String, String>> {
        Ok(self.get_kv("aliases")?.unwrap_or_default())
    }

    pub fn save_aliases(&self, aliases: &HashMap<String, String>) -> Result<()> {
        self.put_kv("aliases", aliases)
    }

    /// Replace a conversation wholesale. Prefer `append_line`/`update_line`
    /// for single-message changes; this exists for bulk rewrites (imports,
    /// pruning) and runs in one transaction.
    pub fn save_session(&self, contact: &str, messages: &[StoredChatLine]) -> Result<()> {
        self.with_conn(|conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute("DELETE FROM messages WHERE contact = ?1", params![contact])?;
            {
                let mut insert = tx.prepare(
                    "INSERT INTO messages(contact, msg_id, timestamp, blob) \
                     VALUES (?1, ?2, ?3, ?4)",
                )?;
                for line in messages {
                    insert.execute(params![
                        contact,
                        line.id,
                        line.timestamp,
                        serde_json::to_string(line)?
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub fn load_session(&self, contact: &str) -> Result<Vec<StoredChatLine>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT blob FROM messages WHERE contact = ?1 ORDER BY timestamp, seq")?;
            let rows = stmt.query_map(params![contact], |row| row.get::<_, String>(0))?;
            let mut lines = Vec::new();
            for blob in rows {
                lines.push(serde_json::from_str(&blob?)?);
            }
            Ok(lines)
        })
    }

    /// Does this conversation already hold a line with `id`? Indexed lookup,
    /// used for dedup without loading the whole conversation.
    pub fn session_has_id(&self, contact: &str, id: &str) -> Result<bool> {
        if id.is_empty() {
            return Ok(false);
        }
        self.with_conn(|conn| {
            let found: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM messages WHERE contact = ?1 AND msg_id = ?2 LIMIT 1",
                    params![contact, id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(found.is_some())
        })
    }

    /// Append one line. Single INSERT — O(1) regardless of history length.
    pub fn append_line(&self, contact: &str, line: &StoredChatLine) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO messages(contact, msg_id, timestamp, blob) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    contact,
                    line.id,
                    line.timestamp,
                    serde_json::to_string(line)?
                ],
            )?;
            Ok(())
        })
    }

    /// Overwrite the line with `line.id` in place. Returns false when no such
    /// line exists (caller should append instead). No-op for empty ids.
    pub fn update_line(&self, contact: &str, line: &StoredChatLine) -> Result<bool> {
        if line.id.is_empty() {
            return Ok(false);
        }
        self.with_conn(|conn| {
            let changed = conn.execute(
                "UPDATE messages SET timestamp = ?3, blob = ?4 \
                 WHERE contact = ?1 AND msg_id = ?2",
                params![
                    contact,
                    line.id,
                    line.timestamp,
                    serde_json::to_string(line)?
                ],
            )?;
            Ok(changed > 0)
        })
    }

    pub fn search(
        &self,
        query: &str,
        mailbox_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CachedSearchHit>> {
        let terms = query
            .split_whitespace()
            .map(|term| term.to_ascii_lowercase())
            .filter(|term| !term.is_empty())
            .collect::<Vec<_>>();
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        let mut hits = Vec::new();
        let dir = self.messages_dir();
        if !dir.exists() {
            return Ok(hits);
        }

        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let bytes =
                fs::read(&path).with_context(|| format!("read cache {}", path.display()))?;
            let cached: CachedMessage = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse cache {}", path.display()))?;
            if let Some(filter) = mailbox_filter
                && cached.mailbox != filter
            {
                continue;
            }
            let haystack = searchable_text(&cached.message).to_ascii_lowercase();
            let score = terms
                .iter()
                .filter(|term| haystack.contains(term.as_str()))
                .count();
            if score == 0 {
                continue;
            }
            hits.push(CachedSearchHit {
                mailbox: cached.mailbox,
                uid: cached.uid,
                subject: cached.message.subject.clone(),
                from: cached.message.from.clone(),
                date: cached.message.date.clone(),
                score,
                excerpt: excerpt_for(&cached.message, &terms),
            });
        }

        hits.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| right.uid.cmp(&left.uid))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    // ------------------------------------------------------------------
    // Sync state (cursors, read marks)
    // ------------------------------------------------------------------

    pub fn load_state(&self) -> Result<SyncState> {
        Ok(self.get_kv("state")?.unwrap_or_default())
    }

    pub fn save_state(&self, state: &SyncState) -> Result<()> {
        self.put_kv("state", state)
    }

    // ------------------------------------------------------------------
    // Peer keys (TOFU)
    // ------------------------------------------------------------------

    pub fn load_peers(&self) -> Result<PeerStore> {
        Ok(self.get_kv("peers")?.unwrap_or_default())
    }

    pub fn save_peers(&self, peers: &PeerStore) -> Result<()> {
        self.put_kv("peers", peers)
    }

    /// Directory for the media of one received message.
    pub fn incoming_media_dir(&self, contact: &str, message_id: &str) -> PathBuf {
        let id = if message_id.is_empty() {
            "legacy".to_string()
        } else {
            safe_component(message_id)
        };
        self.root
            .join("attachments")
            .join(format!("recv__{}__{}", safe_component(contact), id))
    }

    fn state_path(&self) -> PathBuf {
        self.root.join("state.json")
    }

    fn peers_path(&self) -> PathBuf {
        self.root.join("peers.json")
    }

    fn messages_dir(&self) -> PathBuf {
        self.root.join("messages")
    }

    fn login_path(&self) -> PathBuf {
        self.root.join("login.json")
    }

    fn contacts_path(&self) -> PathBuf {
        self.root.join("contacts.json")
    }

    fn aliases_path(&self) -> PathBuf {
        self.root.join("aliases.json")
    }

    fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    fn message_path(&self, mailbox: &str, uid: u32) -> PathBuf {
        self.messages_dir()
            .join(format!("{}__{}.json", safe_component(mailbox), uid))
    }
}

/// Persistent sync/read state (state.json).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SyncState {
    /// Cursor per `account|mailbox`.
    #[serde(default)]
    pub cursors: HashMap<String, SyncCursor>,
    /// Timestamp of the newest message the user has seen, per contact.
    /// Used to derive unread counts across restarts.
    #[serde(default)]
    pub last_read: HashMap<String, i64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct SyncCursor {
    pub uid_validity: u32,
    pub last_uid: u32,
}

pub fn cursor_key(account: &str, mailbox: &str) -> String {
    format!("{}|{}", account.to_ascii_lowercase(), mailbox)
}

/// peers.json: TOFU key records keyed by contact address.
pub type PeerStore = HashMap<String, PeerRecord>;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PeerRecord {
    /// Trusted public key (base64), as first seen or re-trusted.
    pub key: String,
    /// hex(SHA-256(key))[..16]
    pub fingerprint: String,
    pub first_seen: i64,
    pub last_seen: i64,
    /// Encrypt outgoing messages to this peer (user-controllable).
    #[serde(default = "default_true")]
    pub encrypt: bool,
    /// A different key arrived after `key` was recorded; encryption is
    /// suspended until the user runs /trust.
    #[serde(default)]
    pub pending_key: Option<String>,
}

fn default_true() -> bool {
    true
}

impl PeerRecord {
    /// The key to encrypt to right now.
    ///
    /// A pending (unverified) key change does NOT drop us to plaintext: we
    /// keep encrypting to the last trusted `key`, so an attacker who merely
    /// spoofs an inbound email with a fresh key cannot force a downgrade.
    /// Only the user disabling encryption (`encrypt = false`) turns it off.
    pub fn encryption_key(&self) -> Option<&str> {
        self.encrypt.then_some(self.key.as_str())
    }
}

#[cfg(unix)]
fn write_secret_file(path: PathBuf, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("write login cache {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write login cache {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret_file(path: PathBuf, bytes: &[u8]) -> Result<()> {
    fs::write(&path, bytes).with_context(|| format!("write login cache {}", path.display()))
}

/// Import one legacy JSON file into the kv table under `key`, if it exists.
fn migrate_json_file(conn: &Connection, key: &str, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("read legacy {}", path.display()))?;
    // Validate it parses as JSON before storing, so a corrupt file surfaces
    // now rather than at the next read.
    let value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parse legacy {}", path.display()))?;
    conn.execute(
        "INSERT INTO kv(key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, serde_json::to_string(&value)?],
    )?;
    Ok(())
}

/// Rename an imported file/dir to `<name>.migrated`, replacing any prior one.
/// Best-effort: failure to move a stale file never fails the migration.
fn rename_aside(path: &Path) {
    if !path.exists() {
        return;
    }
    let aside = path.with_extension("migrated");
    if aside.exists() {
        let _ = if aside.is_dir() {
            fs::remove_dir_all(&aside)
        } else {
            fs::remove_file(&aside)
        };
    }
    let _ = fs::rename(path, &aside);
}

fn searchable_text(message: &MessageSummary) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        message.subject,
        message.from,
        message.to,
        message.cc,
        message.links.join("\n"),
        message.body.as_deref().unwrap_or_default()
    )
}

fn excerpt_for(message: &MessageSummary, terms: &[String]) -> String {
    let body = message.body.as_deref().unwrap_or_default();
    let lower = body.to_ascii_lowercase();
    let start = terms
        .iter()
        .filter_map(|term| lower.find(term))
        .min()
        .unwrap_or(0);
    let excerpt = body
        .chars()
        .skip(start.saturating_sub(80))
        .take(240)
        .collect::<String>();
    if excerpt.trim().is_empty() {
        message.subject.clone()
    } else {
        excerpt.replace('\n', " ")
    }
}

fn safe_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "mailbox".to_string()
    } else {
        sanitized
    }
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{DeliveryStatus, MailCache, StoredChatLine, StoredSession};
    use std::collections::HashMap;

    fn temp_cache(tag: &str) -> (std::path::PathBuf, MailCache) {
        let dir = std::env::temp_dir().join(format!("slip-cache-{}-{tag}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let cache = MailCache::at(dir.clone());
        (dir, cache)
    }

    fn line(id: &str, ts: i64, body: &str) -> StoredChatLine {
        StoredChatLine {
            sender: "me".to_string(),
            date: String::new(),
            timestamp: ts,
            body: body.to_string(),
            attachments: Vec::new(),
            id: id.to_string(),
            status: DeliveryStatus::Sending,
            encrypted: false,
            media: Vec::new(),
            flags: Vec::new(),
            retry_count: 0,
            next_retry_at: 0,
        }
    }

    #[test]
    fn aliases_round_trip_and_clear() {
        let (dir, cache) = temp_cache("aliases");

        assert!(cache.load_aliases().unwrap().is_empty());
        let mut aliases = HashMap::new();
        aliases.insert("a@b.com".to_string(), "Alice".to_string());
        cache.save_aliases(&aliases).unwrap();
        assert_eq!(
            cache
                .load_aliases()
                .unwrap()
                .get("a@b.com")
                .map(String::as_str),
            Some("Alice")
        );

        // Stored in the DB, not a stray JSON/temp file.
        assert!(dir.join("slip.db").exists());
        assert!(!dir.join("aliases.json").exists());

        aliases.remove("a@b.com");
        cache.save_aliases(&aliases).unwrap();
        assert!(cache.load_aliases().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn session_append_dedup_and_update() {
        let (dir, cache) = temp_cache("session");
        let contact = "friend@example.com";

        // Append two, dedup a repeat of the first by id.
        cache
            .append_line(contact, &line("id1", 100, "first"))
            .unwrap();
        cache
            .append_line(contact, &line("id2", 200, "second"))
            .unwrap();
        assert!(cache.session_has_id(contact, "id1").unwrap());
        assert!(!cache.session_has_id(contact, "missing").unwrap());
        assert!(!cache.session_has_id(contact, "").unwrap());

        // Update in place by id, and report a miss for an unknown id.
        assert!(
            cache
                .update_line(contact, &line("id1", 100, "first-edited"))
                .unwrap()
        );
        assert!(
            !cache
                .update_line(contact, &line("id9", 300, "nope"))
                .unwrap()
        );

        let loaded = cache.load_session(contact).unwrap();
        assert_eq!(loaded.len(), 2);
        // Ordered by timestamp.
        assert_eq!(loaded[0].id, "id1");
        assert_eq!(loaded[0].body, "first-edited");
        assert_eq!(loaded[1].id, "id2");

        // Wholesale replace collapses to the given set.
        cache
            .save_session(contact, &[line("id3", 400, "only")])
            .unwrap();
        let loaded = cache.load_session(contact).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "id3");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_json_is_migrated_once() {
        let (dir, cache) = temp_cache("migrate");

        // Lay down the pre-SQLite JSON layout.
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        let session = StoredSession {
            contact: "old@example.com".to_string(),
            updated_at: 1,
            messages: vec![line("m1", 10, "legacy hello")],
        };
        std::fs::write(
            dir.join("sessions/old_example.com.json"),
            serde_json::to_vec(&session).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("contacts.json"),
            serde_json::to_vec(&vec!["old@example.com"]).unwrap(),
        )
        .unwrap();
        let mut aliases = HashMap::new();
        aliases.insert("old@example.com".to_string(), "Legacy".to_string());
        std::fs::write(
            dir.join("aliases.json"),
            serde_json::to_vec(&aliases).unwrap(),
        )
        .unwrap();

        // First DB touch imports everything.
        assert_eq!(cache.load_contacts().unwrap(), vec!["old@example.com"]);
        assert_eq!(
            cache
                .load_aliases()
                .unwrap()
                .get("old@example.com")
                .map(String::as_str),
            Some("Legacy")
        );
        let msgs = cache.load_session("old@example.com").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].body, "legacy hello");

        // Imported files are moved aside, not left to be re-read.
        assert!(!dir.join("contacts.json").exists());
        assert!(dir.join("contacts.migrated").exists());
        assert!(dir.join("sessions.migrated").is_dir());

        // A second run does not double-import: writing a new contact then
        // reloading must not resurrect a re-import of the (now absent) files.
        cache
            .save_contacts(&["new@example.com".to_string()])
            .unwrap();
        assert_eq!(cache.load_contacts().unwrap(), vec!["new@example.com"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
