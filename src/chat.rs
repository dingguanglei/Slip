//! Chat orchestration shared by the CLI and the TUI: sending (with
//! opportunistic encryption), cursor-based incremental sync, TOFU peer
//! tracking, dedup by message id, and the IDLE-driven watch loop.

use crate::{
    cache::{
        DeliveryStatus, MailCache, PeerRecord, PeerStore, StoredChatLine, StoredMedia, SyncCursor,
        cursor_key,
    },
    core::{MailCore, MailboxSession, imap_search_string},
    crypto::{Identity, fingerprint_of_b64},
    protocol::{self, IncomingSlip, MediaSource, OutgoingSlip, ParseOutcome, WARN_MEDIA_BYTES},
};
use anyhow::{Context, Result, anyhow};
use chrono::{Local, TimeZone};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const DEFAULT_SYNC_LIMIT: usize = 200;
const IDLE_CYCLE: Duration = Duration::from_secs(60);
/// How many times a failed outgoing message is auto-resent before giving up.
pub const MAX_SEND_RETRIES: u32 = 6;

/// Exponential backoff (seconds) before the next resend attempt: 15s, 30s,
/// 60s, … capped at 30 minutes.
fn retry_backoff_secs(retry_count: u32) -> i64 {
    let base = 15i64;
    let cap = 30 * 60;
    base.saturating_mul(1i64 << retry_count.min(12)).min(cap)
}

#[derive(Clone, Debug)]
pub struct ChatClient {
    core: MailCore,
    cache: MailCache,
    identity: Identity,
    account_address: String,
    sync_lock: std::sync::Arc<std::sync::Mutex<()>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChatSendResult {
    pub id: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub attachments: Vec<String>,
    pub encrypted: bool,
    pub status: DeliveryStatus,
    pub cached_session: String,
    pub timestamp: i64,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Something worth notifying the user about, produced by a sync.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NewMessageNotice {
    pub contact: String,
    pub sender: String,
    pub preview: String,
    pub id: String,
    pub timestamp: i64,
    pub encrypted: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ChatSyncResult {
    pub mailbox: String,
    pub fetched: usize,
    pub saved: usize,
    pub burned: usize,
    /// Recognized messages retained because validation or cleanup failed.
    #[serde(default)]
    pub retained: usize,
    #[serde(default)]
    pub cleanup_pending: usize,
    /// Chat mail moved out of the inbox into the dedicated Slip folder.
    #[serde(default)]
    pub relocated: usize,
    pub contacts: Vec<String>,
    pub cache_dir: String,
    #[serde(default)]
    pub new_messages: Vec<NewMessageNotice>,
    /// Contacts whose identity key changed (encryption suspended).
    #[serde(default)]
    pub key_changes: Vec<String>,
    #[serde(default)]
    pub used_cursor: bool,
    #[serde(default)]
    pub requests_changed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChatSession {
    pub contact: String,
    pub messages: Vec<StoredChatLine>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChatSessionSummary {
    pub contact: String,
    pub messages: usize,
    pub updated_at: i64,
    pub preview: String,
    #[serde(default)]
    pub unread: usize,
    /// Local display name for this contact, if set.
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub request_status: Option<String>,
}

/// Key/encryption facts for /info.
#[derive(Clone, Debug, Serialize)]
pub struct ContactInfo {
    pub address: String,
    pub my_fingerprint: String,
    pub peer_fingerprint: Option<String>,
    pub pending_fingerprint: Option<String>,
    pub encrypt_enabled: bool,
    pub encryption_active: bool,
    pub request_status: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedComposer {
    pub body: String,
    pub attachments: Vec<PathBuf>,
}

impl ChatClient {
    pub fn new(core: MailCore, cache: MailCache) -> Result<Self> {
        let identity = Identity::load_or_create(cache.root())
            .context("load or create ~/.slip identity keypair")?;
        let account_address = core.config().address.to_ascii_lowercase();
        Ok(Self {
            core,
            cache,
            identity,
            account_address,
            sync_lock: std::sync::Arc::new(std::sync::Mutex::new(())),
        })
    }

    pub fn account_address(&self) -> &str {
        &self.account_address
    }

    pub fn cache(&self) -> &MailCache {
        &self.cache
    }

    pub fn identity_public_key(&self) -> String {
        self.identity.public_key_b64()
    }

    pub fn identity_fingerprint(&self) -> String {
        self.identity.fingerprint()
    }

    fn remember_wire(&self, raw: &[u8]) -> Result<()> {
        let digest = protocol::outbound_wire_digest(
            raw,
            &self.account_address,
            &self.identity_public_key(),
            self.core.config().subject_marker(),
        )
        .ok_or_else(|| anyhow!("cannot record outbound wire receipt"))?;
        self.cache.remember_outbound_wire(&digest)
    }

    fn is_saved_outbound(&self, raw: &[u8]) -> Result<bool> {
        match protocol::outbound_wire_digest(
            raw,
            &self.account_address,
            &self.identity_public_key(),
            self.core.config().subject_marker(),
        ) {
            Some(digest) => self.cache.knows_outbound_wire(&digest),
            None => Ok(false),
        }
    }

    fn cleanup_saved(
        &self,
        session: &mut MailboxSession,
        uid: u32,
        result: &mut ChatSyncResult,
    ) -> Result<()> {
        let cleanup = if self.core.config().provider_id == "gmail" {
            session.burn_gmail(uid)
        } else {
            session.delete(uid).map(|_| true)
        };
        match cleanup {
            Ok(true) => result.burned += 1,
            Ok(false) => result.relocated += 1,
            Err(_) => result.cleanup_pending += 1,
        }
        Ok(())
    }

    /// Persist an invitation/acceptance before attempting any network IO.
    /// The engine delivers it asynchronously and retries after disconnection.
    pub fn exchange_key(&self, address: &str) -> Result<()> {
        let to = normalize_email(address)?;
        if to == self.account_address {
            return Err(anyhow!("不能添加自己为好友"));
        }
        self.add_contact(&to)?;
        let _guard = self.cache.lock();
        let mut requests = self.cache.load_requests()?;
        let known_key = self.cache.load_peers()?.contains_key(&to);
        let request = requests
            .entry(to.clone())
            .or_insert_with(|| crate::cache::KeyRequest {
                received: known_key,
                ..Default::default()
            });
        request.accepted = true;
        if request.wire.is_empty() && !request.sent {
            let message = OutgoingSlip {
                id: crate::crypto::random_id(),
                ts: now_epoch_seconds_i64(),
                from: self.account_address.clone(),
                to: to.clone(),
                text: String::new(),
                media: Vec::new(),
            };
            request.wire = protocol::build_mail(
                &message,
                &self.identity,
                None,
                self.core.config().subject_marker(),
            )?;
            self.remember_wire(&request.wire)?;
        }
        request.next_attempt = 0;
        request.updated_at = now_epoch_seconds_i64();
        self.cache.save_requests(&requests)?;
        Ok(())
    }

    /// A recovered mailbox connection should not wait out an old offline backoff.
    pub fn wake_key_requests(&self) -> Result<()> {
        let _guard = self.cache.lock();
        let mut requests = self.cache.load_requests()?;
        let mut changed = false;
        for request in requests.values_mut() {
            if !request.wire.is_empty() && request.next_attempt != 0 {
                request.next_attempt = 0;
                changed = true;
            }
        }
        if changed {
            self.cache.save_requests(&requests)?;
        }
        Ok(())
    }

    pub fn retry_key_requests(&self) -> Result<Vec<String>> {
        self.retry_key_requests_using(|to, raw| self.core.send_raw_mail(to, raw))
    }

    /// Flush durable invitations through an explicit transport.
    pub fn retry_key_requests_using(
        &self,
        mut deliver: impl FnMut(&str, &[u8]) -> Result<()>,
    ) -> Result<Vec<String>> {
        let requests = self.cache.load_requests()?;
        let mut changed = Vec::new();
        for (to, snapshot) in requests {
            if snapshot.wire.is_empty() || snapshot.next_attempt > now_epoch_seconds_i64() {
                continue;
            }
            // Reserve before IO; a concurrent worker or restart cannot hot-loop.
            let wire = {
                let _guard = self.cache.lock();
                let mut current = self.cache.load_requests()?;
                let Some(request) = current.get_mut(&to) else {
                    continue;
                };
                if request.wire.is_empty() || request.next_attempt > now_epoch_seconds_i64() {
                    continue;
                }
                request.next_attempt = now_epoch_seconds_i64() + 120;
                let wire = request.wire.clone();
                self.cache.save_requests(&current)?;
                wire
            };
            let success = deliver(&to, &wire).is_ok();
            let _guard = self.cache.lock();
            let mut current = self.cache.load_requests()?;
            if let Some(request) = current.get_mut(&to) {
                if success {
                    request.wire.clear();
                    request.sent = true;
                } else {
                    request.next_attempt =
                        now_epoch_seconds_i64() + retry_backoff_secs(request.attempts);
                    request.attempts = request.attempts.saturating_add(1);
                }
                request.updated_at = now_epoch_seconds_i64();
            }
            self.cache.save_requests(&current)?;
            changed.push(to);
        }
        Ok(changed)
    }

    pub fn add_contact(&self, address: &str) -> Result<Vec<String>> {
        let address = normalize_email(address)?;
        self.with_contacts(|contacts| {
            ensure_contact(contacts, &address);
            contacts.clone()
        })
    }

    /// Set (or, with an empty name, clear) a local display name for a contact.
    /// Also ensures the contact exists. Returns the stored alias, if any.
    pub fn set_alias(&self, address: &str, name: &str) -> Result<Option<String>> {
        let address = normalize_email(address)?;
        self.with_contacts(|contacts| ensure_contact(contacts, &address))?;
        let name = name.trim().to_string();
        let _guard = self.cache.lock();
        let mut aliases = self.cache.load_aliases()?;
        let result = if name.is_empty() {
            aliases.remove(&address);
            None
        } else {
            aliases.insert(address.clone(), name.clone());
            Some(name)
        };
        self.cache.save_aliases(&aliases)?;
        Ok(result)
    }

    /// The local display name for a contact, if one is set.
    pub fn contact_alias(&self, address: &str) -> Result<Option<String>> {
        let address = normalize_email(address)?;
        let _guard = self.cache.lock();
        Ok(self.cache.load_aliases()?.get(&address).cloned())
    }

    // ------------------------------------------------------------------
    // Locked, atomic read-modify-write primitives
    //
    // Every mutation of a store file goes through one of these while holding
    // `MailCache::lock`, so the TUI's worker and watcher threads can never
    // lose an update by interleaving load/save. Each helper re-reads current
    // state inside the lock, so a concurrent /trust or /add is always seen.
    // ------------------------------------------------------------------

    fn with_contacts<T>(&self, f: impl FnOnce(&mut Vec<String>) -> T) -> Result<T> {
        let _guard = self.cache.lock();
        let mut contacts = self.cache.load_contacts()?;
        let out = f(&mut contacts);
        self.cache.save_contacts(&contacts)?;
        Ok(out)
    }

    fn with_peers<T>(&self, f: impl FnOnce(&mut PeerStore) -> T) -> Result<T> {
        let _guard = self.cache.lock();
        let mut peers = self.cache.load_peers()?;
        let out = f(&mut peers);
        self.cache.save_peers(&peers)?;
        Ok(out)
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut crate::cache::SyncState) -> T) -> Result<T> {
        let _guard = self.cache.lock();
        let mut state = self.cache.load_state()?;
        let out = f(&mut state);
        self.cache.save_state(&state)?;
        Ok(out)
    }

    // ------------------------------------------------------------------
    // Sending
    // ------------------------------------------------------------------

    pub fn send(&self, to: &str, input: &str) -> Result<ChatSendResult> {
        let parsed = parse_composer(input);
        self.send_parts(to, &parsed.body, &parsed.attachments)
    }

    pub fn send_parts(
        &self,
        to: &str,
        body: &str,
        attachments: &[PathBuf],
    ) -> Result<ChatSendResult> {
        self.send_parts_using(to, body, attachments, |to, raw| {
            self.core.send_raw_mail(to, raw)
        })
    }

    /// Same encryption and local persistence pipeline with an explicit
    /// transport callback.
    pub fn send_parts_using(
        &self,
        to: &str,
        body: &str,
        attachments: &[PathBuf],
        deliver: impl FnOnce(&str, &[u8]) -> Result<()>,
    ) -> Result<ChatSendResult> {
        let to = normalize_email(to)?;
        if !self.contact_info(&to)?.encryption_active {
            return Err(anyhow!("请先接受好友请求并完成公钥交换"));
        }
        let mut media = Vec::new();
        for path in attachments {
            if !path.is_file() {
                return Err(anyhow!("attachment is not a file: {}", path.display()));
            }
            media.push(MediaSource::from_path(path)?);
        }
        let body = body.trim();
        if body.is_empty() && media.is_empty() {
            return Err(anyhow!("message is empty"));
        }
        self.add_contact(&to)?;

        let mut warnings = Vec::new();
        let total: u64 = media.iter().map(|item| item.size).sum();
        if total > WARN_MEDIA_BYTES {
            warnings.push(format!(
                "attachments total {:.1} MB; many providers reject mail this large",
                total as f64 / (1024.0 * 1024.0)
            ));
        }

        let peers = self.cache.load_peers()?;
        let encrypt_key = peers
            .get(&to)
            .and_then(|record| record.encryption_key())
            .map(ToOwned::to_owned);
        if encrypt_key.is_none() {
            return Err(anyhow!("尚未建立加密连接，请先交换公钥；正文未发送"));
        }

        let id = crate::crypto::random_id();
        let timestamp = now_epoch_seconds_i64();
        let outgoing = OutgoingSlip {
            id: id.clone(),
            ts: timestamp,
            from: self.account_address.clone(),
            to: to.clone(),
            text: body.to_string(),
            media,
        };

        // Local copies of outgoing attachments before any network work.
        let cached_attachments =
            self.cache
                .save_outgoing_attachments(&to, timestamp, attachments)?;
        let stored_media = outgoing
            .media
            .iter()
            .zip(cached_attachments.iter())
            .map(|(source, path)| StoredMedia {
                kind: source.kind,
                name: source.name.clone(),
                mime: source.mime.clone(),
                size: source.size,
                path: path.clone(),
            })
            .collect::<Vec<_>>();

        let encrypted = encrypt_key.is_some();
        let mut line = StoredChatLine {
            sender: "me".to_string(),
            date: format_chat_timestamp(timestamp),
            timestamp,
            body: body.to_string(),
            attachments: cached_attachments.clone(),
            id: id.clone(),
            status: DeliveryStatus::Sending,
            encrypted,
            media: stored_media,
            flags: Vec::new(),
            retry_count: 0,
            next_retry_at: 0,
        };
        let raw = protocol::build_mail(
            &outgoing,
            &self.identity,
            encrypt_key.as_deref(),
            self.core.config().subject_marker(),
        )?;
        self.upsert_line(&to, line.clone())?;
        self.remember_wire(&raw)?;
        let send_result = deliver(&to, &raw);

        let status = match &send_result {
            Ok(()) => DeliveryStatus::Sent,
            Err(_) => DeliveryStatus::Failed,
        };
        line.status = status;
        match &send_result {
            Ok(()) => crate::logging::info(
                "send",
                "delivered",
                &[
                    ("to", &to),
                    ("id", &id),
                    ("bytes", &raw.len()),
                    ("encrypted", &encrypted),
                ],
            ),
            Err(err) => {
                line.flags.push(format!("send-error:{err:#}"));
                // Due on the next retry tick; backoff grows only after that.
                line.next_retry_at = now_epoch_seconds_i64();
                crate::logging::warn(
                    "send",
                    "failed, queued for retry",
                    &[("to", &to), ("id", &id), ("err", &format!("{err:#}"))],
                );
            }
        }
        self.upsert_line(&to, line)?;
        send_result?;

        Ok(ChatSendResult {
            id,
            to: to.clone(),
            subject: self.core.config().subject_marker().to_string(),
            body: body.to_string(),
            attachments: cached_attachments,
            encrypted,
            status,
            cached_session: self.cache.root().display().to_string(),
            timestamp,
            warnings,
        })
    }

    /// Re-send the most recent failed message in a conversation, keeping its
    /// id so receivers dedup if the first copy secretly made it out.
    pub fn retry_last_failed(&self, contact: &str) -> Result<ChatSendResult> {
        let contact = normalize_email(contact)?;
        let lines = self.cache.load_session(&contact)?;
        let failed = lines
            .iter()
            .rev()
            .find(|line| line.sender == "me" && line.status == DeliveryStatus::Failed)
            .cloned()
            .ok_or_else(|| anyhow!("no failed message to retry for {contact}"))?;

        let mut media = Vec::new();
        for item in &failed.media {
            media.push(MediaSource::from_path(Path::new(&item.path))?);
        }
        let peers = self.cache.load_peers()?;
        let encrypt_key = peers
            .get(&contact)
            .and_then(|record| record.encryption_key())
            .map(ToOwned::to_owned);
        if encrypt_key.is_none() {
            return Err(anyhow!("cannot retry without a trusted encryption key"));
        }
        let outgoing = OutgoingSlip {
            id: failed.id.clone(),
            ts: failed.timestamp,
            from: self.account_address.clone(),
            to: contact.clone(),
            text: failed.body.clone(),
            media,
        };
        let raw = protocol::build_mail(
            &outgoing,
            &self.identity,
            encrypt_key.as_deref(),
            self.core.config().subject_marker(),
        )?;
        self.remember_wire(&raw)?;
        let send_result = self.core.send_raw_mail(&contact, &raw);

        let mut line = failed.clone();
        line.encrypted = true;
        line.status = if send_result.is_ok() {
            DeliveryStatus::Sent
        } else {
            DeliveryStatus::Failed
        };
        line.flags.retain(|flag| !flag.starts_with("send-error:"));
        if let Err(err) = &send_result {
            line.flags.push(format!("send-error:{err:#}"));
        }
        self.upsert_line(&contact, line.clone())?;
        send_result?;

        Ok(ChatSendResult {
            id: line.id,
            to: contact,
            subject: self.core.config().subject_marker().to_string(),
            body: failed.body,
            attachments: failed.attachments,
            encrypted: encrypt_key.is_some(),
            status: DeliveryStatus::Sent,
            cached_session: self.cache.root().display().to_string(),
            timestamp: failed.timestamp,
            warnings: Vec::new(),
        })
    }

    /// Automatically resend failed outgoing messages whose backoff has
    /// elapsed, across every conversation. Returns the contacts whose
    /// sessions changed (so the frontend can reload them). Each attempt keeps
    /// the original message id, so a receiver deduplicates a copy that
    /// secretly did go out. After [`MAX_SEND_RETRIES`] attempts a message is
    /// marked "gave-up" and left failed for a manual /retry.
    pub fn retry_pending(&self) -> Result<Vec<String>> {
        let contacts = {
            let _guard = self.cache.lock();
            self.cache.load_contacts()?
        };
        let now = now_epoch_seconds_i64();
        let mut changed = Vec::new();

        for contact in contacts {
            let due: Vec<StoredChatLine> = self
                .load_session_lines(&contact)?
                .into_iter()
                .filter(|line| {
                    line.sender == "me"
                        && line.status == DeliveryStatus::Failed
                        && line.retry_count < MAX_SEND_RETRIES
                        && line.next_retry_at <= now
                })
                .collect();
            if due.is_empty() {
                continue;
            }

            let encrypt_key = {
                let peers = self.cache.load_peers()?;
                peers
                    .get(&contact)
                    .and_then(|record| record.encryption_key())
                    .map(ToOwned::to_owned)
            };

            if encrypt_key.is_none() {
                continue;
            }
            let mut touched = false;
            for mut line in due {
                let mut media = Vec::new();
                let mut media_ok = true;
                for item in &line.media {
                    match MediaSource::from_path(Path::new(&item.path)) {
                        Ok(source) => media.push(source),
                        Err(_) => {
                            media_ok = false;
                            break;
                        }
                    }
                }
                if !media_ok {
                    // A referenced attachment vanished; stop retrying it.
                    line.retry_count = MAX_SEND_RETRIES;
                    line.flags.push("gave-up:attachment-missing".to_string());
                    self.upsert_line(&contact, line)?;
                    touched = true;
                    continue;
                }

                let outgoing = OutgoingSlip {
                    id: line.id.clone(),
                    ts: line.timestamp,
                    from: self.account_address.clone(),
                    to: contact.clone(),
                    text: line.body.clone(),
                    media,
                };
                let sent = protocol::build_mail(
                    &outgoing,
                    &self.identity,
                    encrypt_key.as_deref(),
                    self.core.config().subject_marker(),
                )
                .and_then(|raw| {
                    self.remember_wire(&raw)?;
                    self.core.send_raw_mail(&contact, &raw)
                });

                match sent {
                    Ok(()) => {
                        line.status = DeliveryStatus::Sent;
                        line.encrypted = true;
                        line.flags.retain(|flag| !flag.starts_with("send-error:"));
                        line.next_retry_at = 0;
                    }
                    Err(err) => {
                        line.retry_count += 1;
                        line.flags.retain(|flag| !flag.starts_with("send-error:"));
                        line.flags.push(format!("send-error:{err:#}"));
                        if line.retry_count >= MAX_SEND_RETRIES {
                            line.flags.push("gave-up".to_string());
                        } else {
                            line.next_retry_at = now + retry_backoff_secs(line.retry_count);
                        }
                    }
                }
                self.upsert_line(&contact, line)?;
                touched = true;
            }
            if touched {
                changed.push(contact);
            }
        }
        Ok(changed)
    }

    // ------------------------------------------------------------------
    // Sync
    // ------------------------------------------------------------------

    pub fn sync(
        &self,
        mailbox: &str,
        limit: usize,
        burn_after_save: bool,
    ) -> Result<ChatSyncResult> {
        let mut session = self.core.connect()?;
        let result = self.sync_with(&mut session, mailbox, limit, burn_after_save);
        session.logout();
        result
    }

    /// Ingest one mail payload through the same validation/storage path.
    /// Does not remove any remote message. Intended for explicit transports.
    pub fn receive_wire(&self, raw: &[u8]) -> Result<ChatSyncResult> {
        let _guard = self.sync_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut result = ChatSyncResult::default();
        match protocol::parse_mail_bytes(raw, &self.identity, self.core.config().subject_marker()) {
            ParseOutcome::Message(incoming) => {
                if !self.process_incoming(*incoming, &mut result)? {
                    return Err(anyhow!("message not accepted"));
                }
            }
            _ => return Err(anyhow!("message not readable")),
        }
        Ok(result)
    }

    /// Incremental sync on an existing connection (used by the watch loop).
    ///
    /// Read candidates in place across configured folders, then restore the
    /// arrival mailbox so the watch loop receives its IDLE notifications.
    pub fn sync_with(
        &self,
        session: &mut MailboxSession,
        arrival_mailbox: &str,
        limit: usize,
        burn_after_save: bool,
    ) -> Result<ChatSyncResult> {
        let _sync_guard = self.sync_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut result = ChatSyncResult {
            mailbox: arrival_mailbox.to_string(),
            cache_dir: self.cache.root().display().to_string(),
            ..ChatSyncResult::default()
        };

        // Read in place: never move a message based only on SUBJECT search.
        self.sync_folder(
            session,
            arrival_mailbox,
            limit,
            burn_after_save,
            &mut result,
        )?;
        if let Some(folder) = self.core.config().slip_folder_for(arrival_mailbox)
            && session.select(folder).is_ok()
        {
            self.sync_folder(session, folder, limit, burn_after_save, &mut result)?;
        }
        for folder in self.core.config().spam_folders() {
            if session.select(folder).is_ok() {
                self.sync_folder(session, folder, limit, burn_after_save, &mut result)?;
            }
        }

        if burn_after_save {
            for folder in session.sent_folders()? {
                if !folder.eq_ignore_ascii_case(arrival_mailbox) {
                    self.sync_folder(session, &folder, limit, true, &mut result)?;
                }
            }
        }
        if burn_after_save && self.core.config().provider_id == "gmail" {
            let trash = session.gmail_trash_folder()?;
            self.sync_folder(session, &trash, limit, true, &mut result)?;
        }
        session.select(arrival_mailbox)?;
        result.contacts = {
            let _guard = self.cache.lock();
            self.cache.load_contacts()?
        };
        if result.saved > 0 || result.relocated > 0 || !result.key_changes.is_empty() {
            crate::logging::info(
                "sync",
                "processed mailbox",
                &[
                    ("mailbox", &arrival_mailbox),
                    ("fetched", &result.fetched),
                    ("saved", &result.saved),
                    ("relocated", &result.relocated),
                    ("burned", &result.burned),
                    ("key_changes", &result.key_changes.len()),
                ],
            );
        }
        Ok(result)
    }

    /// Cursor-based incremental processing of one folder.
    fn sync_folder(
        &self,
        session: &mut MailboxSession,
        mailbox: &str,
        limit: usize,
        burn_after_save: bool,
        result: &mut ChatSyncResult,
    ) -> Result<()> {
        let info = session.select(mailbox)?;
        let key = cursor_key(&self.account_address, mailbox);
        let saved_cursor = {
            let _guard = self.cache.lock();
            self.cache
                .load_state()?
                .cursors
                .get(&key)
                .copied()
                .filter(|cursor| cursor.uid_validity == info.uid_validity)
        };
        let cursor = if burn_after_save { None } else { saved_cursor };

        let (mut uids, used_cursor, truncated) = match cursor {
            Some(cursor) if cursor.uid_validity == info.uid_validity && cursor.last_uid > 0 => {
                let query = format!(
                    "UID {}:* SUBJECT {}",
                    cursor.last_uid + 1,
                    imap_search_string(self.core.config().subject_marker())
                );
                let uids = session
                    .uid_search(&query)?
                    .into_iter()
                    .filter(|uid| *uid > cursor.last_uid)
                    .collect::<Vec<_>>();
                (uids, true, false)
            }
            _ => {
                // First sync or a UIDVALIDITY reset: full subject scan. If it
                // exceeds `limit`, process the OLDEST `limit` now and advance
                // the cursor only to what we processed, so the newer backlog
                // is drained forward by later syncs (never permanently
                // skipped, unlike a jump straight to uid_next).
                let mut uids = session.uid_search(&format!(
                    "SUBJECT {}",
                    imap_search_string(self.core.config().subject_marker())
                ))?;
                let truncated = !burn_after_save && uids.len() > limit;
                if truncated {
                    uids.truncate(limit);
                }
                (uids, false, truncated)
            }
        };

        // Also inspect a bounded range of fresh headers without SEARCH.
        // This closes the delivery-to-search-index lag seen on real providers.
        let recent_start = saved_cursor
            .map(|c| c.last_uid.saturating_add(1))
            .unwrap_or_else(|| info.uid_next.saturating_sub(200))
            .max(1);
        let recent_end = info
            .uid_next
            .saturating_sub(1)
            .min(recent_start.saturating_add(199));
        if burn_after_save {
            uids.extend(session.chat_header_uids(
                recent_start,
                recent_end,
                self.core.config().subject_marker(),
            )?);
            uids.sort_unstable();
            uids.dedup();
        }
        result.used_cursor = used_cursor;
        let mut max_uid = cursor.map(|cursor| cursor.last_uid).unwrap_or(0);

        for uid in uids {
            let Some(raw) = session.fetch_raw(uid)? else {
                continue;
            };
            result.fetched += 1;
            max_uid = max_uid.max(uid);

            if self.is_saved_outbound(&raw)? {
                if burn_after_save {
                    self.cleanup_saved(session, uid, result)?;
                }
                continue;
            }
            match protocol::parse_mail_bytes(
                &raw,
                &self.identity,
                self.core.config().subject_marker(),
            ) {
                ParseOutcome::NotSlip => {}
                ParseOutcome::Unreadable { .. } => {
                    // Never reserve a message ID for an unauthenticated or
                    // unreadable placeholder: it could suppress a later valid
                    // copy, causing premature deletion without real content.
                    result.retained += 1;
                }
                ParseOutcome::Message(incoming) => {
                    let processed = self.process_incoming(*incoming, result)?;
                    if processed && burn_after_save {
                        self.cleanup_saved(session, uid, result)?;
                    } else if !processed {
                        result.retained += 1;
                    }
                }
            }
        }

        // When the full scan was truncated, only claim up to the newest UID we
        // actually processed. Otherwise everything through uid_next has been
        // examined, so advance past it.
        let last_uid = if burn_after_save {
            recent_end.max(saved_cursor.map(|c| c.last_uid).unwrap_or(0))
        } else if truncated {
            max_uid
        } else {
            max_uid.max(info.uid_next.saturating_sub(1))
        };
        self.with_state(|state| {
            state.cursors.insert(
                key,
                SyncCursor {
                    uid_validity: info.uid_validity,
                    last_uid,
                },
            );
        })?;
        Ok(())
    }

    /// Ingest one parsed message. Returns true when it is safe to burn the
    /// remote mail (fully persisted, or a known duplicate). Inbound mail that
    /// claims to be *from us* is dropped: our own sent messages are stored at
    /// send time, so a "from me" message arriving in the inbox is either a
    /// loopback we don't need or a spoof forging our history.
    fn process_incoming(
        &self,
        incoming: IncomingSlip,
        result: &mut ChatSyncResult,
    ) -> Result<bool> {
        if incoming.from == self.account_address
            || incoming.legacy
            || incoming.id.is_empty()
            || !incoming.problems.is_empty()
            || !incoming.to.iter().any(|to| to == &self.account_address)
            || (!incoming.encrypted && (!incoming.text.is_empty() || !incoming.media.is_empty()))
        {
            return Ok(false);
        }
        if normalize_email(&incoming.from).is_err() {
            return Ok(false);
        }
        let contact = incoming.from.clone();
        let handshake =
            !incoming.encrypted && incoming.text.is_empty() && incoming.media.is_empty();
        if incoming.sender_key.is_none() {
            return Ok(false);
        }
        let first_key = !self.cache.load_peers()?.contains_key(&contact);
        // Unsolicited invitations never cause automatic email replies.
        self.with_contacts(|contacts| ensure_contact(contacts, &contact))?;

        let mut flags = incoming.problems.clone();

        // Reject cross-conversation replay: the envelope must be addressed to
        // us (spec: receivers verify `to` contains their own address).
        if !incoming.legacy
            && !incoming.to.is_empty()
            && !incoming.to.iter().any(|to| to == &self.account_address)
        {
            flags.push("wrong-recipient".to_string());
        }

        // TOFU on the sender key. A changed key is flagged and recorded as
        // pending, but does NOT drop us to plaintext (see
        // PeerRecord::encryption_key); the user resolves it with /trust.
        if let Some(sender_key) = &incoming.sender_key {
            let key_changed = self.with_peers(|peers| {
                let now = now_epoch_seconds_i64();
                match peers.get_mut(&contact) {
                    None => {
                        peers.insert(
                            contact.clone(),
                            PeerRecord {
                                key: sender_key.clone(),
                                fingerprint: fingerprint_of_b64(sender_key).unwrap_or_default(),
                                first_seen: now,
                                last_seen: now,
                                encrypt: true,
                                pending_key: None,
                            },
                        );
                        false
                    }
                    Some(record) if record.key == *sender_key => {
                        record.last_seen = now;
                        false
                    }
                    Some(record) => {
                        let newly = record.pending_key.as_deref() != Some(sender_key.as_str());
                        record.pending_key = Some(sender_key.clone());
                        newly
                    }
                }
            })?;
            if key_changed {
                result.key_changes.push(contact.clone());
            }
            if self
                .cache
                .load_peers()?
                .get(&contact)
                .and_then(|record| record.pending_key.as_ref())
                .is_some()
            {
                flags.push("key-changed".to_string());
            }
        }

        if (handshake || first_key || self.cache.load_requests()?.contains_key(&contact))
            && !flags.iter().any(|flag| flag == "key-changed")
        {
            let _guard = self.cache.lock();
            let mut requests = self.cache.load_requests()?;
            let legacy_peer = !first_key
                && !requests.contains_key(&contact)
                && self
                    .load_session_lines(&contact)?
                    .iter()
                    .any(|line| line.sender == "me" && line.encrypted);
            let request = requests.entry(contact.clone()).or_default();
            if legacy_peer {
                request.accepted = true;
                request.sent = true;
            }
            if !request.received {
                request.received = true;
                request.updated_at = now_epoch_seconds_i64();
                result.requests_changed = true;
            }
            self.cache.save_requests(&requests)?;
        }
        if handshake {
            return Ok(true); // the peer key has been committed to SQLite
        }
        if self
            .cache
            .load_peers()?
            .get(&contact)
            .and_then(|p| p.pending_key.as_ref())
            .is_some()
        {
            return Ok(false);
        }

        // Duplicate remote copy of an already-saved message: safe to burn,
        // but do not rewrite its media (a replay with the same id must not
        // clobber the stored attachment bytes) or re-notify.
        let replace_placeholder = match self
            .load_session_lines(&contact)?
            .iter()
            .find(|line| line.id == incoming.id)
        {
            Some(line) if !line.flags.is_empty() => true,
            Some(_) => return Ok(true),
            None => false,
        };

        // Only burn once every media item was recovered; a missing or
        // undecryptable attachment means we keep the mail for a later retry.
        let complete = !incoming.problems.iter().any(|problem| {
            problem.starts_with("missing-attachment-part")
                || problem.starts_with("undecryptable-attachment")
        });

        // Persist media bytes.
        let media_dir = self.cache.incoming_media_dir(&contact, &incoming.id);
        let mut stored_media = Vec::new();
        let mut attachment_paths = Vec::new();
        if !incoming.media.is_empty() {
            std::fs::create_dir_all(&media_dir)
                .with_context(|| format!("create {}", media_dir.display()))?;
        }
        for item in &incoming.media {
            let file_name = format!(
                "{:02}-{}",
                item.entry.idx,
                crate::core::safe_filename(&item.entry.name, item.entry.idx)
            );
            let path = media_dir.join(file_name);
            use std::io::Write;
            let mut file = std::fs::File::create(&path)?;
            file.write_all(&item.bytes)?;
            file.sync_all()?;
            let path = path.display().to_string();
            attachment_paths.push(path.clone());
            stored_media.push(StoredMedia {
                kind: item.entry.kind,
                name: item.entry.name.clone(),
                mime: item.entry.mime.clone(),
                size: item.entry.size,
                path,
            });
        }

        if !incoming.media.is_empty() {
            crate::cache::sync_directory(&media_dir)?;
        }
        let timestamp = if incoming.ts > 0 {
            incoming.ts
        } else {
            now_epoch_seconds_i64()
        };
        let line = StoredChatLine {
            sender: contact.clone(),
            date: format_chat_timestamp(timestamp),
            timestamp,
            body: incoming.text.clone(),
            attachments: attachment_paths,
            id: incoming.id.clone(),
            status: DeliveryStatus::Sent,
            encrypted: incoming.encrypted,
            media: stored_media,
            flags,
            retry_count: 0,
            next_retry_at: 0,
        };

        let preview = preview_of(&line);
        let inserted = if replace_placeholder {
            self.upsert_line(&contact, line)?;
            true
        } else {
            self.insert_if_new(&contact, line)?
        };
        if inserted {
            result.saved += 1;
            result.new_messages.push(NewMessageNotice {
                contact: contact.clone(),
                sender: contact.clone(),
                preview,
                id: incoming.id.clone(),
                timestamp,
                encrypted: incoming.encrypted,
            });
        }
        Ok(complete)
    }

    // ------------------------------------------------------------------
    // Watch (push)
    // ------------------------------------------------------------------

    /// Blocking loop: keep a connection in IDLE and sync the moment the
    /// mailbox changes. Falls back to polling when IDLE is unsupported.
    /// `on_sync` fires only for syncs that changed something or failed.
    pub fn watch(
        &self,
        mailbox: &str,
        burn_after_save: bool,
        poll_interval: Duration,
        stop: &AtomicBool,
        mut on_sync: impl FnMut(&ChatSyncResult),
        mut on_status: impl FnMut(&str),
    ) {
        let min_backoff = Duration::from_secs(2);
        let max_backoff = Duration::from_secs(60);
        let mut backoff = min_backoff;
        // Sleep before every reconnect (not only after connect failures) so a
        // deterministic post-connect failure — a corrupt state.json, an
        // unselectable mailbox — cannot spin the loop into a tight reconnect
        // storm that gets the account rate-limited. Backoff resets only after
        // a successful IDLE/poll and sync cycle.
        while !stop.load(Ordering::Relaxed) {
            let mut session = match self.core.connect() {
                Ok(session) => session,
                Err(err) => {
                    on_status(&format!(
                        "connection failed ({err:#}); retrying in {}s",
                        backoff.as_secs()
                    ));
                    interruptible_sleep(backoff, stop);
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
            };

            // Catch up before idling.
            match self.sync_with(&mut session, mailbox, DEFAULT_SYNC_LIMIT, burn_after_save) {
                Ok(result) => {
                    if result.saved > 0
                        || !result.new_messages.is_empty()
                        || result.requests_changed
                        || !result.key_changes.is_empty()
                    {
                        on_sync(&result);
                    }
                }
                Err(err) => {
                    on_status(&format!(
                        "sync failed ({err:#}); retrying in {}s",
                        backoff.as_secs()
                    ));
                    session.logout();
                    interruptible_sleep(backoff, stop);
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }
            }

            let idle_supported = session.supports_idle();
            on_status(if idle_supported {
                "listening (IMAP IDLE)"
            } else {
                "listening (polling; server lacks IDLE)"
            });

            loop {
                if stop.load(Ordering::Relaxed) {
                    session.logout();
                    return;
                }
                if idle_supported {
                    if let Err(err) = session.idle_wait(IDLE_CYCLE) {
                        on_status(&format!("IDLE dropped ({err:#}); reconnecting"));
                        break;
                    }
                    // Sync on every wake, including timeouts: a notification
                    // that arrives during the IDLE-to-DONE handoff would
                    // otherwise be swallowed. The sync is cursor-based, so a
                    // no-op costs one cheap UID SEARCH.
                } else {
                    interruptible_sleep(poll_interval, stop);
                }

                if stop.load(Ordering::Relaxed) {
                    session.logout();
                    return;
                }
                match self.sync_with(&mut session, mailbox, DEFAULT_SYNC_LIMIT, burn_after_save) {
                    Ok(result) => {
                        backoff = min_backoff;
                        if result.saved > 0
                            || !result.new_messages.is_empty()
                            || result.requests_changed
                            || !result.key_changes.is_empty()
                        {
                            on_sync(&result);
                        }
                    }
                    Err(err) => {
                        on_status(&format!("sync failed ({err:#}); reconnecting"));
                        break;
                    }
                }
            }
            session.logout();
            interruptible_sleep(backoff, stop);
            backoff = (backoff * 2).min(max_backoff);
        }
    }

    // ------------------------------------------------------------------
    // Sessions and reads
    // ------------------------------------------------------------------

    pub fn session(&self, contact: &str) -> Result<ChatSession> {
        let contact = normalize_email(contact)?;
        Ok(ChatSession {
            messages: self.load_session_lines(&contact)?,
            contact,
        })
    }

    pub fn sessions(&self) -> Result<Vec<ChatSessionSummary>> {
        let (state, contacts, aliases) = {
            let _guard = self.cache.lock();
            (
                self.cache.load_state()?,
                self.cache.load_contacts()?,
                self.cache.load_aliases()?,
            )
        };
        let requests = self.cache.load_requests()?;
        let mut sessions = Vec::new();
        for contact in contacts {
            // Never list our own address as a conversation partner.
            if contact == self.account_address {
                continue;
            }
            let lines = self.load_session_lines(&contact)?;
            let request = requests.get(&contact);
            let updated_at = lines
                .iter()
                .map(stored_chat_timestamp)
                .max()
                .unwrap_or_default()
                .max(request.map(|r| r.updated_at).unwrap_or_default());
            let last_read = state.last_read.get(&contact).copied().unwrap_or(0);
            let incoming_count = lines.iter().filter(|line| line.sender != "me").count();
            let read_count = state.read_counts.get(&contact).copied().unwrap_or_else(|| {
                lines
                    .iter()
                    .filter(|line| line.sender != "me" && stored_chat_timestamp(line) <= last_read)
                    .count()
            });
            let unread = incoming_count.saturating_sub(read_count);
            let alias = aliases.get(&contact).cloned();
            sessions.push(ChatSessionSummary {
                request_status: request.map(|r| r.status().to_string()),
                contact,
                messages: lines.len(),
                updated_at,
                preview: lines
                    .last()
                    .map(preview_of)
                    .unwrap_or_else(|| "(empty session)".to_string()),
                unread,
                alias,
            });
        }
        sessions.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.contact.cmp(&right.contact))
        });
        Ok(sessions)
    }

    /// Mark a conversation as read up to its newest line.
    pub fn mark_read(&self, contact: &str) -> Result<()> {
        let contact = normalize_email(contact)?;
        let lines = self.load_session_lines(&contact)?;
        let newest = lines
            .iter()
            .map(stored_chat_timestamp)
            .max()
            .unwrap_or_else(now_epoch_seconds_i64);
        let count = lines.iter().filter(|line| line.sender != "me").count();
        self.with_state(|state| {
            state.read_counts.insert(contact.clone(), count);
            let entry = state.last_read.entry(contact).or_insert(0);
            *entry = (*entry).max(newest);
        })
    }

    // ------------------------------------------------------------------
    // Trust management
    // ------------------------------------------------------------------

    pub fn contact_info(&self, contact: &str) -> Result<ContactInfo> {
        let contact = normalize_email(contact)?;
        let peers = self.cache.load_peers()?;
        let record = peers.get(&contact);
        let requests = self.cache.load_requests()?;
        let request = requests.get(&contact);
        Ok(ContactInfo {
            request_status: request.map(|r| r.status().to_string()),
            address: contact,
            my_fingerprint: self.identity.fingerprint(),
            peer_fingerprint: record.map(|record| record.fingerprint.clone()),
            pending_fingerprint: record
                .and_then(|record| record.pending_key.as_deref())
                .and_then(|key| fingerprint_of_b64(key).ok()),
            encrypt_enabled: record.map(|record| record.encrypt).unwrap_or(true),
            encryption_active: request.is_none_or(|r| r.accepted && r.sent && r.received)
                && record
                    .map(|record| record.encryption_key().is_some())
                    .unwrap_or(false),
        })
    }

    /// Accept a changed peer key after out-of-band verification.
    pub fn trust_peer(&self, contact: &str) -> Result<ContactInfo> {
        let contact = normalize_email(contact)?;
        self.with_peers(|peers| {
            let record = peers
                .get_mut(&contact)
                .ok_or_else(|| anyhow!("no key on file for {contact}"))?;
            let pending = record
                .pending_key
                .take()
                .ok_or_else(|| anyhow!("no pending key change for {contact}"))?;
            record.key = pending;
            record.fingerprint = fingerprint_of_b64(&record.key).unwrap_or_default();
            record.last_seen = now_epoch_seconds_i64();
            Ok::<(), anyhow::Error>(())
        })??;
        self.contact_info(&contact)
    }

    pub fn set_encrypt(&self, contact: &str, enabled: bool) -> Result<ContactInfo> {
        let contact = normalize_email(contact)?;
        self.with_peers(|peers| {
            let record = peers.get_mut(&contact).ok_or_else(|| {
                anyhow!("no key on file for {contact}; encryption starts after their first message")
            })?;
            record.encrypt = enabled;
            Ok::<(), anyhow::Error>(())
        })??;
        self.contact_info(&contact)
    }

    pub fn peers(&self) -> Result<PeerStore> {
        let _guard = self.cache.lock();
        self.cache.load_peers()
    }

    // ------------------------------------------------------------------
    // Internals
    // ------------------------------------------------------------------

    fn load_session_lines(&self, contact: &str) -> Result<Vec<StoredChatLine>> {
        let mut lines = self.cache.load_session(contact)?;
        sort_chat_lines(&mut lines);
        Ok(lines)
    }

    /// Insert a line unless its id (or legacy identity triple) is present.
    /// The dedup check and the append happen atomically under the store lock.
    /// The common (non-empty id) path is a single indexed lookup + insert;
    /// only legacy id-less lines fall back to scanning the conversation.
    fn insert_if_new(&self, contact: &str, line: StoredChatLine) -> Result<bool> {
        let _guard = self.cache.lock();
        let duplicate = if line.id.is_empty() {
            self.load_session_lines(contact)?
                .iter()
                .any(|existing| same_legacy_line(existing, &line))
        } else {
            self.cache.session_has_id(contact, &line.id)?
        };
        if duplicate {
            return Ok(false);
        }
        self.cache.append_line(contact, &line)?;
        Ok(true)
    }

    /// Insert or replace (by id) a line — used for delivery status updates.
    /// The update-or-append happens atomically under the store lock.
    fn upsert_line(&self, contact: &str, line: StoredChatLine) -> Result<()> {
        let _guard = self.cache.lock();
        if !self.cache.update_line(contact, &line)? {
            self.cache.append_line(contact, &line)?;
        }
        Ok(())
    }
}

fn ensure_contact(contacts: &mut Vec<String>, contact: &str) {
    if !contacts.iter().any(|existing| existing == contact) {
        contacts.push(contact.to_string());
        contacts.sort();
        contacts.dedup();
    }
}

fn interruptible_sleep(total: Duration, stop: &AtomicBool) {
    let step = Duration::from_millis(250);
    let mut remaining = total;
    while remaining > Duration::ZERO && !stop.load(Ordering::Relaxed) {
        let chunk = remaining.min(step);
        std::thread::sleep(chunk);
        remaining = remaining.saturating_sub(chunk);
    }
}

/// Split the composer input into a body and attachments.
///
/// Tokenizing is quote- and escape-aware so paths with spaces work, which is
/// also what terminals paste when a file is dragged in (single-quoted, or
/// with backslash-escaped spaces). A token is an attachment when it starts
/// with `@`, or when it is a bare path that resolves to an existing file
/// (drag-drop without typing `@`). Everything else is body text.
pub fn parse_composer(input: &str) -> ParsedComposer {
    let mut body = Vec::new();
    let mut attachments = Vec::new();
    for token in tokenize_composer(input) {
        if let Some(path) = token.strip_prefix('@') {
            if !path.is_empty() {
                attachments.push(PathBuf::from(path));
                continue;
            }
            // A lone "@" is kept as literal text.
            body.push(token);
            continue;
        }
        if looks_like_droppable_path(&token) {
            attachments.push(PathBuf::from(&token));
            continue;
        }
        body.push(token);
    }
    ParsedComposer {
        body: body.join(" "),
        attachments,
    }
}

/// Tokenize composer input into whitespace-separated tokens, keeping paths
/// with spaces intact — but without breaking ordinary prose.
///
/// A quote is only treated as a path delimiter at a "quotable position": the
/// very start of a token, or immediately after a leading `@`. That way a
/// quote wrapping a dragged-in path (`@'/a b.png'`, `'/a b.png'`) spans the
/// spaces, while an apostrophe inside a word (`it's`, `don't`) stays literal.
/// An unterminated quote is treated as a literal character rather than
/// swallowing the rest of the line. A backslash escapes only a following
/// space or backslash, so Windows paths like `C:\Users\pic.png` keep their
/// separators.
fn tokenize_composer(input: &str) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut i = 0;

    while i < len {
        let ch = chars[i];
        if ch.is_whitespace() {
            if started {
                tokens.push(std::mem::take(&mut current));
                started = false;
            }
            i += 1;
            continue;
        }

        let quotable = !started || current == "@";
        if (ch == '\'' || ch == '"') && quotable {
            match (i + 1..len).find(|&j| chars[j] == ch) {
                Some(close) => {
                    started = true;
                    current.extend(&chars[i + 1..close]);
                    i = close + 1;
                }
                None => {
                    // Unterminated quote: keep it literal, do not swallow.
                    started = true;
                    current.push(ch);
                    i += 1;
                }
            }
            continue;
        }

        if ch == '\\' && i + 1 < len && (chars[i + 1] == ' ' || chars[i + 1] == '\\') {
            started = true;
            current.push(chars[i + 1]);
            i += 2;
            continue;
        }

        started = true;
        current.push(ch);
        i += 1;
    }
    if started {
        tokens.push(current);
    }
    tokens
}

/// A bare token (no leading `@`) that should still be attached: an absolute
/// path to an existing file, which is what a terminal pastes when a file is
/// dragged in. Requiring an absolute path avoids silently attaching a
/// relative word like `config/prod.env` that merely happens to exist under
/// the current directory.
fn looks_like_droppable_path(token: &str) -> bool {
    !token.contains("://") && {
        let path = Path::new(token);
        path.is_absolute() && path.is_file()
    }
}

pub fn normalize_email(value: &str) -> Result<String> {
    let address = value.trim().to_ascii_lowercase();
    if looks_like_email(&address) {
        Ok(address)
    } else {
        Err(anyhow!("invalid email address: {value}"))
    }
}

pub fn looks_like_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

/// One-line human preview of a chat line ("bob: 🖼 photo.png" style).
pub fn preview_of(line: &StoredChatLine) -> String {
    let mut body = line.body.trim().to_string();
    if body.is_empty() {
        body = line
            .media
            .first()
            .map(|media| format!("[{} {}]", media.kind.label(), media.name))
            .unwrap_or_else(|| {
                if line.attachments.is_empty() {
                    "(empty message)".to_string()
                } else {
                    "(attachment)".to_string()
                }
            });
    }
    let extra = match line.media.len() {
        0 | 1 => String::new(),
        n => format!(" [+{} media]", n - 1),
    };
    format!("{}: {}{}", line.sender, body, extra)
}

fn same_legacy_line(left: &StoredChatLine, right: &StoredChatLine) -> bool {
    left.sender == right.sender
        && left.body == right.body
        && if left.timestamp > 0 && right.timestamp > 0 {
            left.timestamp == right.timestamp
        } else {
            left.date == right.date
        }
}

pub fn sort_chat_lines(lines: &mut [StoredChatLine]) {
    // Stable sort preserves local insertion order for same-second messages.
    lines.sort_by_key(stored_chat_timestamp);
}

pub fn stored_chat_timestamp(line: &StoredChatLine) -> i64 {
    if line.timestamp > 0 {
        line.timestamp
    } else {
        parse_chat_timestamp(&line.date).unwrap_or_default()
    }
}

pub fn format_chat_timestamp(timestamp: i64) -> String {
    Local
        .timestamp_opt(timestamp, 0)
        .single()
        .map(|datetime| datetime.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| timestamp.to_string())
}

pub fn parse_chat_timestamp(value: &str) -> Option<i64> {
    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .ok()
        .and_then(|datetime| datetime.and_local_timezone(Local).single())
        .map(|datetime| datetime.timestamp())
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()
                .and_then(|date| date.and_hms_opt(0, 0, 0))
                .and_then(|datetime| datetime.and_local_timezone(Local).single())
                .map(|datetime| datetime.timestamp())
        })
}

pub fn now_epoch_seconds_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

/// Which contacts have unread messages, derived from state.last_read.
pub fn unread_map(summaries: &[ChatSessionSummary]) -> HashSet<String> {
    summaries
        .iter()
        .filter(|summary| summary.unread > 0)
        .map(|summary| summary.contact.clone())
        .collect()
}
