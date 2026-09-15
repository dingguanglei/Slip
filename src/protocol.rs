//! Slip wire protocol v1: build and parse chat mail.
//!
//! Spec: docs/PROTOCOL.md. A Slip message is ordinary RFC 5322 mail with
//! `Subject: [slip/chat]`, `X-Slip-*` headers, a human-readable text part,
//! a JSON envelope part, and the media items as attachment parts. In
//! `box-v1` mode the envelope and every attachment are sealed.

use crate::core::{flatten_parts, safe_filename, subject_matches};
use crate::crypto::Identity;
use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use mailparse::{MailHeaderMap, ParsedMail, dateparse, parse_mail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::Path;

pub const SLIP_VERSION: u32 = 1;
pub const HDR_VERSION: &str = "X-Slip-Version";
pub const HDR_ID: &str = "X-Slip-Id";
pub const HDR_KEY: &str = "X-Slip-Key";
pub const HDR_ENC: &str = "X-Slip-Enc";

pub const ENC_NONE: &str = "none";
pub const ENC_BOX_V1: &str = "box-v1";

pub const MIME_ENVELOPE: &str = "application/x-slip+json";
pub const MIME_ENVELOPE_ENCRYPTED: &str = "application/x-slip-encrypted";

/// Hard cap on total attachment bytes (before base64). Most providers reject
/// mail well below this after encoding overhead.
pub const MAX_MEDIA_BYTES: u64 = 30 * 1024 * 1024;
/// Soft cap: senders should warn above this.
pub const WARN_MEDIA_BYTES: u64 = 18 * 1024 * 1024;

/// Media classification carried in the manifest.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Image,
    Audio,
    #[default]
    File,
}

impl MediaKind {
    pub fn from_mime(mime: &str) -> Self {
        let mime = mime.to_ascii_lowercase();
        if mime.starts_with("image/") {
            Self::Image
        } else if mime.starts_with("audio/") {
            Self::Audio
        } else {
            Self::File
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Audio => "audio",
            Self::File => "file",
        }
    }
}

impl fmt::Display for MediaKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// One manifest entry inside the envelope.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MediaEntry {
    /// 1-based position among the mail's attachment parts.
    pub idx: usize,
    pub kind: MediaKind,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub sha256: String,
}

/// The canonical JSON envelope.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SlipEnvelope {
    pub v: u32,
    pub id: String,
    pub ts: i64,
    pub from: String,
    pub to: Vec<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub media: Vec<MediaEntry>,
}

/// A media item staged for sending: manifest data plus the raw bytes.
#[derive(Clone, Debug)]
pub struct MediaSource {
    pub name: String,
    pub mime: String,
    pub kind: MediaKind,
    pub size: u64,
    pub sha256: String,
    pub bytes: Vec<u8>,
}

impl MediaSource {
    pub fn from_path(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("read attachment {}", path.display()))?;
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "attachment".to_string());
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .essence_str()
            .to_string();
        Ok(Self::from_bytes(name, mime, bytes))
    }

    pub fn from_bytes(name: String, mime: String, bytes: Vec<u8>) -> Self {
        let sha256 = hex::encode(Sha256::digest(&bytes));
        Self {
            kind: MediaKind::from_mime(&mime),
            size: bytes.len() as u64,
            sha256,
            name,
            mime,
            bytes,
        }
    }

    fn entry(&self, idx: usize) -> MediaEntry {
        MediaEntry {
            idx,
            kind: self.kind,
            name: self.name.clone(),
            mime: self.mime.clone(),
            size: self.size,
            sha256: self.sha256.clone(),
        }
    }
}

/// Everything needed to build one outgoing chat mail.
#[derive(Clone, Debug)]
pub struct OutgoingSlip {
    pub id: String,
    pub ts: i64,
    pub from: String,
    pub to: String,
    pub text: String,
    pub media: Vec<MediaSource>,
}

impl OutgoingSlip {
    pub fn envelope(&self) -> SlipEnvelope {
        SlipEnvelope {
            v: SLIP_VERSION,
            id: self.id.clone(),
            ts: self.ts,
            from: self.from.clone(),
            to: vec![self.to.clone()],
            text: self.text.clone(),
            media: self
                .media
                .iter()
                .enumerate()
                .map(|(index, source)| source.entry(index + 1))
                .collect(),
        }
    }

    pub fn total_media_bytes(&self) -> u64 {
        self.media.iter().map(|source| source.size).sum()
    }
}

/// Build the raw RFC 5322 bytes for an outgoing message.
///
/// The identity's public key rides in `X-Slip-Key`; when `encrypt_to` holds
/// the peer's public key, the envelope and all media bytes are sealed
/// (`box-v1`).
pub fn build_mail(
    message: &OutgoingSlip,
    identity: &Identity,
    encrypt_to: Option<&str>,
    subject: &str,
) -> Result<Vec<u8>> {
    if encrypt_to.is_none() && (!message.text.is_empty() || !message.media.is_empty()) {
        return Err(anyhow!(
            "refusing plaintext chat content; exchange public keys first"
        ));
    }
    build_mail_unchecked(message, identity, encrypt_to, subject)
}

fn build_mail_unchecked(
    message: &OutgoingSlip,
    identity: &Identity,
    encrypt_to: Option<&str>,
    subject: &str,
) -> Result<Vec<u8>> {
    let total = message.total_media_bytes();
    if total > MAX_MEDIA_BYTES {
        return Err(anyhow!(
            "attachments total {} bytes; the limit is {} (mail providers reject anything near this after encoding)",
            total,
            MAX_MEDIA_BYTES
        ));
    }

    let envelope_json = serde_json::to_vec(&message.envelope())?;
    let boundary = format!("----slip-{}", message.id);
    let encrypted = encrypt_to.is_some();

    let sender_domain = message
        .from
        .rsplit('@')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("slip.invalid");

    let mut mail = Vec::with_capacity(total as usize * 3 / 2 + envelope_json.len() + 2048);
    let date = chrono::Local::now().to_rfc2822();
    push_header(&mut mail, "Date", &date);
    push_header(&mut mail, "From", &format!("<{}>", message.from));
    push_header(&mut mail, "To", &format!("<{}>", message.to));
    push_header(&mut mail, "Subject", subject);
    push_header(
        &mut mail,
        "Message-ID",
        &format!("<{}@{}>", message.id, sender_domain),
    );
    push_header(&mut mail, "MIME-Version", "1.0");
    push_header(&mut mail, HDR_VERSION, &SLIP_VERSION.to_string());
    push_header(&mut mail, HDR_ID, &message.id);
    push_header(&mut mail, HDR_KEY, &identity.public_key_b64());
    push_header(
        &mut mail,
        HDR_ENC,
        if encrypted { ENC_BOX_V1 } else { ENC_NONE },
    );
    push_header(
        &mut mail,
        "Content-Type",
        &format!("multipart/mixed; boundary=\"{boundary}\""),
    );
    mail.extend_from_slice(b"\r\n");

    // Part 1: human-readable fallback for non-Slip clients.
    let fallback = if encrypted {
        "Encrypted Slip message. Open it with Slip: https://github.com/dingguanglei/Slip"
            .to_string()
    } else if message.text.is_empty() {
        format!("(Slip message with {} attachment(s))", message.media.len())
    } else {
        message.text.clone()
    };
    push_part_header(&mut mail, &boundary, "text/plain; charset=utf-8", None);
    push_base64_body(&mut mail, fallback.as_bytes());

    // Part 2: the canonical envelope.
    if let Some(peer_key) = encrypt_to {
        let sealed = identity.seal(peer_key, &envelope_json)?;
        push_part_header(
            &mut mail,
            &boundary,
            &format!("{MIME_ENVELOPE_ENCRYPTED}; name=\"envelope.slip\""),
            Some("inline; filename=\"envelope.slip\""),
        );
        push_base64_body(&mut mail, &sealed);
    } else {
        push_part_header(
            &mut mail,
            &boundary,
            &format!("{MIME_ENVELOPE}; name=\"envelope.json\""),
            Some("inline; filename=\"envelope.json\""),
        );
        push_base64_body(&mut mail, &envelope_json);
    }

    // Remaining parts: media items, in manifest order.
    for (index, source) in message.media.iter().enumerate() {
        let idx = index + 1;
        if let Some(peer_key) = encrypt_to {
            let sealed = identity.seal(peer_key, &source.bytes)?;
            push_part_header(
                &mut mail,
                &boundary,
                &format!("application/octet-stream; name=\"{idx}.slip\""),
                Some(&format!("attachment; filename=\"{idx}.slip\"")),
            );
            push_base64_body(&mut mail, &sealed);
        } else {
            let filename = safe_filename(&source.name, idx);
            push_part_header(
                &mut mail,
                &boundary,
                &format!("{}; name=\"{filename}\"", source.mime),
                Some(&format!("attachment; filename=\"{filename}\"")),
            );
            push_base64_body(&mut mail, &source.bytes);
        }
    }

    mail.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Ok(mail)
}

fn push_header(mail: &mut Vec<u8>, name: &str, value: &str) {
    mail.extend_from_slice(name.as_bytes());
    mail.extend_from_slice(b": ");
    mail.extend_from_slice(value.as_bytes());
    mail.extend_from_slice(b"\r\n");
}

fn push_part_header(
    mail: &mut Vec<u8>,
    boundary: &str,
    content_type: &str,
    disposition: Option<&str>,
) {
    mail.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    push_header(mail, "Content-Type", content_type);
    push_header(mail, "Content-Transfer-Encoding", "base64");
    if let Some(disposition) = disposition {
        push_header(mail, "Content-Disposition", disposition);
    }
    mail.extend_from_slice(b"\r\n");
}

fn push_base64_body(mail: &mut Vec<u8>, bytes: &[u8]) {
    let encoded = BASE64.encode(bytes);
    for chunk in encoded.as_bytes().chunks(76) {
        mail.extend_from_slice(chunk);
        mail.extend_from_slice(b"\r\n");
    }
    mail.extend_from_slice(b"\r\n");
}

/// One received media item: manifest entry plus decoded (decrypted) bytes.
#[derive(Clone, Debug)]
pub struct IncomingMedia {
    pub entry: MediaEntry,
    pub bytes: Vec<u8>,
    pub hash_ok: bool,
}

/// A fully parsed inbound chat message.
#[derive(Clone, Debug)]
pub struct IncomingSlip {
    pub id: String,
    pub ts: i64,
    pub from: String,
    pub to: Vec<String>,
    pub text: String,
    pub media: Vec<IncomingMedia>,
    /// Sender public key from `X-Slip-Key` (normalized base64), if any.
    pub sender_key: Option<String>,
    pub encrypted: bool,
    /// v0 mail without a Slip envelope.
    pub legacy: bool,
    /// Non-fatal integrity notes ("hash-mismatch:photo.png", …).
    pub problems: Vec<String>,
}

/// Outcome of parsing one raw mail.
#[derive(Debug)]
pub enum ParseOutcome {
    /// Not Slip chat traffic at all; leave it alone.
    NotSlip,
    /// Parsed fine.
    Message(Box<IncomingSlip>),
    /// Recognizably Slip, but the payload cannot be recovered. The mail
    /// must not be burned.
    Unreadable {
        id: String,
        from: String,
        ts: i64,
        encrypted: bool,
        reason: String,
    },
}

/// Refuse to parse mail with absurd multipart nesting. `mailparse` recurses
/// once per level with no depth limit, so a few hundred KB of deeply nested
/// `multipart/*` parts could overflow the stack. Real mail nests only a few
/// levels; this cap is far above anything legitimate.
const MAX_MULTIPART_HEADERS: usize = 100;

fn is_mime_bomb(raw: &[u8]) -> bool {
    let needle = b"multipart/";
    let mut count = 0usize;
    // Case-insensitive substring count; each nesting level needs its own
    // `Content-Type: multipart/...` header, so this bounds the nesting depth.
    for window in raw.windows(needle.len()) {
        if window.eq_ignore_ascii_case(needle) {
            count += 1;
            if count > MAX_MULTIPART_HEADERS {
                return true;
            }
        }
    }
    false
}

/// Canonical digest of an outbound MIME payload. Added transport headers and
/// base64 wrapping do not change it; changing any payload part does. Used only
/// to recognize exact locally recorded sent copies, never as authentication of
/// unknown mail.
pub fn outbound_wire_digest(raw: &[u8], address: &str, key: &str, subject: &str) -> Option<String> {
    if is_mime_bomb(raw) {
        return None;
    }
    let parsed = parse_mail(raw).ok()?;
    let headers = &parsed.headers;
    if !subject_matches(&headers.get_first_value("Subject")?, subject)
        || headers.get_first_value(HDR_VERSION)?.trim() != "1"
        || first_address(&headers.get_first_value("From")?) != address
        || headers.get_first_value(HDR_KEY)?.trim() != key
    {
        return None;
    }
    let mut hash = Sha256::new();
    for part in flatten_parts(&parsed) {
        let mime = part.ctype.mimetype.to_ascii_lowercase();
        let body = part.get_body_raw().ok()?;
        hash.update((mime.len() as u64).to_be_bytes());
        hash.update(mime.as_bytes());
        hash.update((body.len() as u64).to_be_bytes());
        hash.update(body);
    }
    Some(hex::encode(hash.finalize()))
}

/// Parse raw RFC 5322 bytes into a chat message.
///
/// `identity` is needed to open `box-v1` payloads (sealed with our public
/// key and the sender key from `X-Slip-Key`). `subject_marker` is the Subject
/// that identifies chat mail (both peers must agree on it).
pub fn parse_mail_bytes(raw: &[u8], identity: &Identity, subject_marker: &str) -> ParseOutcome {
    if is_mime_bomb(raw) {
        return ParseOutcome::Unreadable {
            id: String::new(),
            from: String::new(),
            ts: 0,
            encrypted: false,
            reason: "message has excessive MIME nesting; refusing to parse".to_string(),
        };
    }
    match parse_inner(raw, identity, subject_marker) {
        Ok(outcome) => outcome,
        Err(err) => ParseOutcome::Unreadable {
            id: String::new(),
            from: String::new(),
            ts: 0,
            encrypted: false,
            reason: format!("{err:#}"),
        },
    }
}

fn parse_inner(raw: &[u8], identity: &Identity, subject_marker: &str) -> Result<ParseOutcome> {
    let parsed = parse_mail(raw)?;
    let headers = &parsed.headers;
    let subject = headers.get_first_value("Subject").unwrap_or_default();
    if !subject_matches(&subject, subject_marker) {
        return Ok(ParseOutcome::NotSlip);
    }

    let header_from = first_address(&headers.get_first_value("From").unwrap_or_default());
    let header_ts = headers
        .get_first_value("Date")
        .and_then(|value| dateparse(&value).ok())
        .unwrap_or(0);
    let sender_key = headers
        .get_first_value(HDR_KEY)
        .and_then(|value| crate::crypto::normalize_public_key_b64(&value).ok());

    let version = headers
        .get_first_value(HDR_VERSION)
        .and_then(|value| value.trim().parse::<u32>().ok());

    let Some(version) = version else {
        return Ok(parse_legacy(&parsed, header_from, header_ts, sender_key));
    };

    let header_id = headers
        .get_first_value(HDR_ID)
        .unwrap_or_default()
        .trim()
        .to_string();
    let enc = headers
        .get_first_value(HDR_ENC)
        .unwrap_or_else(|| ENC_NONE.to_string())
        .trim()
        .to_ascii_lowercase();
    let encrypted = enc == ENC_BOX_V1;

    if version > SLIP_VERSION {
        return Ok(ParseOutcome::Unreadable {
            id: header_id,
            from: header_from,
            ts: header_ts,
            encrypted,
            reason: format!("message uses Slip protocol v{version}; upgrade Slip to read it"),
        });
    }

    // Locate the envelope part and the attachment parts.
    let mut envelope_raw: Option<Vec<u8>> = None;
    let mut envelope_sealed = false;
    let mut attachment_parts: Vec<&ParsedMail<'_>> = Vec::new();
    for part in flatten_parts(&parsed) {
        let mime = part.ctype.mimetype.to_ascii_lowercase();
        if mime == MIME_ENVELOPE || mime == MIME_ENVELOPE_ENCRYPTED {
            if envelope_raw.is_none() {
                envelope_raw = Some(part.get_body_raw()?);
                envelope_sealed = mime == MIME_ENVELOPE_ENCRYPTED;
            }
            continue;
        }
        let disposition = part.get_content_disposition();
        if disposition.disposition == mailparse::DispositionType::Attachment {
            attachment_parts.push(part);
        }
    }

    let Some(envelope_raw) = envelope_raw else {
        return Ok(ParseOutcome::Unreadable {
            id: header_id,
            from: header_from,
            ts: header_ts,
            encrypted,
            reason: "v1 message has no Slip envelope part".to_string(),
        });
    };

    let mut problems = Vec::new();
    if encrypted != envelope_sealed {
        problems.push("encryption-header-mismatch".into());
    }

    let envelope_json = if envelope_sealed {
        let Some(sender_key) = sender_key.as_deref() else {
            return Ok(ParseOutcome::Unreadable {
                id: header_id,
                from: header_from,
                ts: header_ts,
                encrypted: true,
                reason: "encrypted message carries no sender key".to_string(),
            });
        };
        match identity.open(sender_key, &envelope_raw) {
            Ok(plain) => plain,
            Err(err) => {
                return Ok(ParseOutcome::Unreadable {
                    id: header_id,
                    from: header_from,
                    ts: header_ts,
                    encrypted: true,
                    reason: format!("cannot decrypt envelope: {err:#}"),
                });
            }
        }
    } else {
        envelope_raw
    };

    let envelope: SlipEnvelope = match serde_json::from_slice(&envelope_json) {
        Ok(envelope) => envelope,
        Err(err) => {
            return Ok(ParseOutcome::Unreadable {
                id: header_id,
                from: header_from,
                ts: header_ts,
                encrypted,
                reason: format!("invalid Slip envelope JSON: {err}"),
            });
        }
    };

    if !header_id.is_empty() && envelope.id != header_id {
        problems.push("id-mismatch-between-header-and-envelope".to_string());
    }
    if !header_from.is_empty() && !envelope.from.eq_ignore_ascii_case(&header_from) {
        problems.push(format!(
            "from-mismatch: header {header_from}, envelope {}",
            envelope.from
        ));
    }

    let mut media = Vec::new();
    for entry in &envelope.media {
        let Some(part) = entry
            .idx
            .checked_sub(1)
            .and_then(|index| attachment_parts.get(index))
        else {
            problems.push(format!("missing-attachment-part:{}", entry.name));
            continue;
        };
        let raw_bytes = part.get_body_raw()?;
        let bytes = if envelope_sealed {
            match identity.open(sender_key.as_deref().unwrap_or_default(), &raw_bytes) {
                Ok(plain) => plain,
                Err(_) => {
                    problems.push(format!("undecryptable-attachment:{}", entry.name));
                    continue;
                }
            }
        } else {
            raw_bytes
        };
        let hash_ok = hex::encode(Sha256::digest(&bytes)) == entry.sha256;
        if !hash_ok {
            problems.push(format!("hash-mismatch:{}", entry.name));
        }
        media.push(IncomingMedia {
            entry: entry.clone(),
            bytes,
            hash_ok,
        });
    }

    Ok(ParseOutcome::Message(Box::new(IncomingSlip {
        id: envelope.id.clone(),
        ts: if envelope.ts > 0 {
            envelope.ts
        } else {
            header_ts
        },
        from: envelope.from.to_ascii_lowercase(),
        to: envelope
            .to
            .iter()
            .map(|address| address.to_ascii_lowercase())
            .collect(),
        text: envelope.text.clone(),
        media,
        sender_key,
        encrypted,
        legacy: false,
        problems,
    })))
}

/// v0: `[slip/chat]` subject, no envelope. Plain text plus generic files.
fn parse_legacy(
    parsed: &ParsedMail<'_>,
    header_from: String,
    header_ts: i64,
    sender_key: Option<String>,
) -> ParseOutcome {
    let mut text = String::new();
    let mut media = Vec::new();
    let mut to = Vec::new();

    if let Some(value) = parsed.headers.get_first_value("To") {
        to = value
            .split(',')
            .map(first_address)
            .filter(|address| !address.is_empty())
            .collect();
    }

    for (index, part) in flatten_parts(parsed).into_iter().enumerate() {
        let disposition = part.get_content_disposition();
        let mime = part.ctype.mimetype.to_ascii_lowercase();
        let is_attachment = disposition.disposition == mailparse::DispositionType::Attachment;
        if !is_attachment && mime == "text/plain" && text.is_empty() {
            text = part.get_body().unwrap_or_default().trim().to_string();
            continue;
        }
        let filename = disposition
            .params
            .get("filename")
            .cloned()
            .or_else(|| part.ctype.params.get("name").cloned());
        if let Some(name) = filename {
            let bytes = part.get_body_raw().unwrap_or_default();
            let sha256 = hex::encode(Sha256::digest(&bytes));
            media.push(IncomingMedia {
                entry: MediaEntry {
                    idx: index + 1,
                    kind: MediaKind::from_mime(&mime),
                    name,
                    mime,
                    size: bytes.len() as u64,
                    sha256,
                },
                bytes,
                hash_ok: true,
            });
        }
    }

    ParseOutcome::Message(Box::new(IncomingSlip {
        id: String::new(),
        ts: header_ts,
        from: header_from,
        to,
        text,
        media,
        sender_key,
        encrypted: false,
        legacy: true,
        problems: Vec::new(),
    }))
}

/// Extract the bare lowercase email address from a header fragment like
/// `"Alice" <alice@example.com>`.
pub fn first_address(value: &str) -> String {
    let value = value.trim();
    if let Some(start) = value.find('<')
        && let Some(end) = value[start..].find('>')
    {
        return value[start + 1..start + end].trim().to_ascii_lowercase();
    }
    value
        .split_whitespace()
        .find(|token| token.contains('@'))
        .map(|token| {
            token
                .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && !"@.-_+".contains(ch))
                .to_ascii_lowercase()
        })
        .unwrap_or_default()
}
