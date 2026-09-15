//! Network transport: IMAP (rustls/plain, persistent sessions, IDLE) and
//! SMTP (lettre). Also the legacy MIME summary parsing used by the debug CLI.

use crate::providers::{Endpoint, Provider, Security, provider_by_id, provider_for_address};
use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD};
use imap::{
    Client, Session,
    extensions::idle::{SetReadTimeout, WaitOutcome},
    types::{Fetch, NameAttribute},
};
use lettre::{
    Transport,
    address::Envelope,
    transport::smtp::{
        SmtpTransport,
        authentication::Credentials,
        client::{Tls, TlsParameters},
    },
};
use mailparse::{MailHeaderMap, ParsedMail, addrparse_header, dateparse, parse_mail};
use serde::{Deserialize, Serialize};
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The default Subject marker that identifies Slip chat mail. Both peers must
/// use the same marker; a privacy-conscious pair can set a less identifying
/// one via `SLIP_SUBJECT` (see `MailConfig::subject`).
pub const CHAT_SUBJECT: &str = "[slip/chat]";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// Whether `subject` matches the given chat marker exactly (after trimming).
pub fn subject_matches(subject: &str, marker: &str) -> bool {
    subject.trim() == marker.trim()
}

/// Default IMAP folder that holds Slip chat traffic, isolated from other mail.
pub const DEFAULT_SLIP_FOLDER: &str = "Slip";

/// Full account configuration: identity address, secret, and both endpoints.
#[derive(Clone, Debug)]
pub struct MailConfig {
    pub address: String,
    secret: String,
    /// Registry id (`qq`, `gmail`, …) or `custom`.
    pub provider_id: String,
    pub imap: Endpoint,
    pub smtp: Endpoint,
    /// Send the IMAP `ID` handshake after login (NetEase requirement).
    pub needs_imap_id: bool,
    /// Dedicated folder Slip chat mail is filed into. Incoming chat mail is
    /// moved here out of the inbox so Slip traffic never mixes with other
    /// mail. Empty disables the dedicated folder (chat stays in the inbox).
    pub slip_folder: String,
    /// Subject marker that tags chat mail. Both peers must use the same value;
    /// a custom one (via `SLIP_SUBJECT`) makes Slip traffic less obvious to
    /// the mail provider. Defaults to [`CHAT_SUBJECT`].
    pub subject: String,
}

impl MailConfig {
    pub fn from_provider(
        provider: &Provider,
        address: impl Into<String>,
        secret: impl Into<String>,
    ) -> Self {
        Self {
            address: address.into(),
            secret: secret.into(),
            provider_id: provider.id.to_string(),
            imap: provider.imap(),
            smtp: provider.smtp(),
            needs_imap_id: provider.needs_imap_id,
            slip_folder: DEFAULT_SLIP_FOLDER.to_string(),
            subject: CHAT_SUBJECT.to_string(),
        }
    }

    pub fn custom(
        address: impl Into<String>,
        secret: impl Into<String>,
        imap: Endpoint,
        smtp: Endpoint,
    ) -> Self {
        Self {
            address: address.into(),
            secret: secret.into(),
            provider_id: crate::providers::CUSTOM_PROVIDER_ID.to_string(),
            imap,
            smtp,
            needs_imap_id: false,
            slip_folder: DEFAULT_SLIP_FOLDER.to_string(),
            subject: CHAT_SUBJECT.to_string(),
        }
    }

    /// The configured Slip folder, or `None` when it is disabled or would
    /// collide with the arrival mailbox.
    pub fn slip_folder_for(&self, arrival_mailbox: &str) -> Option<&str> {
        let folder = self.slip_folder.trim();
        if folder.is_empty() || folder.eq_ignore_ascii_case(arrival_mailbox) {
            None
        } else {
            Some(folder)
        }
    }

    /// Candidate spam/junk folder names to sweep for misfiled chat mail.
    /// Providers sometimes route automated mail to spam; scanning these lets
    /// Slip recover a message the provider misclassified. Non-existent names
    /// are skipped, so a broad list is harmless.
    pub fn spam_folders(&self) -> Vec<&'static str> {
        match self.provider_id.as_str() {
            "gmail" => vec!["[Gmail]/Spam", "Spam"],
            "qq" | "163" | "126" => vec!["垃圾邮件", "Junk"],
            "outlook" => vec!["Junk", "Junk Email"],
            "icloud" => vec!["Junk"],
            "yahoo" => vec!["Bulk Mail", "Junk"],
            _ => vec!["Junk", "Spam", "Junk E-mail"],
        }
    }

    /// Resolve credentials and endpoints from the environment.
    ///
    /// Preferred variables:
    /// `SLIP_ADDRESS`, `SLIP_PASSWORD`, and (for non-preset domains or
    /// overrides) `SLIP_IMAP_HOST/PORT/SECURITY`, `SLIP_SMTP_HOST/PORT/SECURITY`,
    /// `SLIP_ALLOW_INVALID_CERTS`.
    ///
    /// Legacy variables (`QQ_MAIL_ADDRESS`/`QQ_MAIL_AUTH_CODE`/`QQ_EMAIL`/`QQ_PWD`
    /// and the `GMAIL_*` equivalents) keep working.
    pub fn from_env() -> Result<Self> {
        let address = env_first(&["SLIP_ADDRESS", "SLIP_EMAIL"]);
        let secret = env_first(&["SLIP_PASSWORD", "SLIP_AUTH_CODE"]);

        let (address, secret) = match (address, secret) {
            (Some(address), Some(secret)) => (address, secret),
            _ => legacy_env_credentials().context(
                "set SLIP_ADDRESS and SLIP_PASSWORD (or legacy QQ_MAIL_ADDRESS/QQ_MAIL_AUTH_CODE, GMAIL_ADDRESS/GMAIL_APP_PASSWORD)",
            )?,
        };

        let mut config = match provider_for_address(&address) {
            Some(provider) => Self::from_provider(provider, address, secret),
            None => {
                let imap_host = env_first(&["SLIP_IMAP_HOST"]);
                let smtp_host = env_first(&["SLIP_SMTP_HOST"]);
                match (imap_host, smtp_host) {
                    (Some(imap_host), Some(smtp_host)) => {
                        let imap_security =
                            env_security("SLIP_IMAP_SECURITY")?.unwrap_or(Security::Ssl);
                        let smtp_security =
                            env_security("SLIP_SMTP_SECURITY")?.unwrap_or(Security::Ssl);
                        let imap = Endpoint::new(
                            imap_host,
                            env_port("SLIP_IMAP_PORT")?
                                .unwrap_or_else(|| default_imap_port(imap_security)),
                            imap_security,
                        );
                        let smtp = Endpoint::new(
                            smtp_host,
                            env_port("SLIP_SMTP_PORT")?
                                .unwrap_or_else(|| default_smtp_port(smtp_security)),
                            smtp_security,
                        );
                        Self::custom(address, secret, imap, smtp)
                    }
                    _ => {
                        return Err(anyhow!(
                            "no preset for the {address} domain; set SLIP_IMAP_HOST and SLIP_SMTP_HOST (plus *_PORT/*_SECURITY as needed)"
                        ));
                    }
                }
            }
        };

        // Explicit endpoint variables override preset values too.
        if let Some(host) = env_first(&["SLIP_IMAP_HOST"]) {
            config.imap.host = host;
        }
        if let Some(port) = env_port("SLIP_IMAP_PORT")? {
            config.imap.port = port;
        }
        if let Some(security) = env_security("SLIP_IMAP_SECURITY")? {
            config.imap.security = security;
        }
        if let Some(host) = env_first(&["SLIP_SMTP_HOST"]) {
            config.smtp.host = host;
        }
        if let Some(port) = env_port("SLIP_SMTP_PORT")? {
            config.smtp.port = port;
        }
        if let Some(security) = env_security("SLIP_SMTP_SECURITY")? {
            config.smtp.security = security;
        }
        // Presence check (not env_first) so an explicitly-empty SLIP_FOLDER=""
        // is honored as "disable the dedicated folder", per the README.
        if let Ok(folder) = env::var("SLIP_FOLDER") {
            config.slip_folder = folder.trim().to_string();
        }
        if let Some(subject) = env_first(&["SLIP_SUBJECT"]) {
            config.subject = subject;
        }
        Ok(config)
    }

    /// The chat subject marker, never empty (falls back to [`CHAT_SUBJECT`]).
    pub fn subject_marker(&self) -> &str {
        let trimmed = self.subject.trim();
        if trimmed.is_empty() {
            CHAT_SUBJECT
        } else {
            trimmed
        }
    }

    pub fn secret(&self) -> &str {
        &self.secret
    }

    pub fn provider(&self) -> Option<&'static Provider> {
        provider_by_id(&self.provider_id)
    }

    pub fn credential_hint(&self) -> &'static str {
        self.provider()
            .map(|provider| provider.secret_hint)
            .unwrap_or("Check the IMAP host, port, security mode, and password.")
    }

    /// Actionable guidance for a failed IMAP login, tuned to the provider and
    /// to what the server said. Most Gmail/QQ first-run failures are "used the
    /// normal password" or "IMAP not enabled", so name those explicitly.
    fn login_help(&self, server_error: &str) -> String {
        let lower = server_error.to_ascii_lowercase();
        let looks_like_bad_auth = lower.contains("authenticationfailed")
            || lower.contains("invalid credentials")
            || lower.contains("not accepted")
            || lower.contains("authorized")
            || lower.contains("login")
            || lower.contains("password");
        match self.provider_id.as_str() {
            "gmail" => if looks_like_bad_auth {
                "Gmail rejected the login. Use a 16-character App Password \
                     (Google Account -> Security -> 2-Step Verification -> App passwords), \
                     not your normal password, and make sure IMAP is enabled in Gmail \
                     settings -> Forwarding and POP/IMAP."
            } else {
                "Check your Gmail App Password and that IMAP is enabled."
            }
            .to_string(),
            "qq" => if looks_like_bad_auth {
                "QQ 邮箱登录被拒。请用「授权码」而不是 QQ 密码：QQ 邮箱 -> 设置 -> 账户 \
                     -> 开启 IMAP/SMTP 服务，生成授权码后填入这里。"
            } else {
                "检查 QQ 邮箱授权码，并确认已开启 IMAP/SMTP 服务。"
            }
            .to_string(),
            _ => self.credential_hint().to_string(),
        }
    }
}

fn env_first(names: &[&str]) -> Option<String> {
    names
        .iter()
        .filter_map(|name| env::var(name).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

fn env_port(name: &str) -> Result<Option<u16>> {
    match env_first(&[name]) {
        None => Ok(None),
        Some(value) => value
            .parse::<u16>()
            .map(Some)
            .with_context(|| format!("{name} must be a port number, got {value}")),
    }
}

fn env_security(name: &str) -> Result<Option<Security>> {
    match env_first(&[name]) {
        None => Ok(None),
        Some(value) => value
            .parse::<Security>()
            .map(Some)
            .map_err(|err| anyhow!("{name}: {err}")),
    }
}

fn default_imap_port(security: Security) -> u16 {
    match security {
        Security::Ssl => 993,
        Security::StartTls | Security::Plain => 143,
    }
}

fn default_smtp_port(security: Security) -> u16 {
    match security {
        Security::Ssl => 465,
        Security::StartTls => 587,
        Security::Plain => 25,
    }
}

fn legacy_env_credentials() -> Result<(String, String)> {
    for (address_vars, secret_vars) in [
        (
            &["QQ_ACCOUNT", "QQ_MAIL_ADDRESS", "QQ_EMAIL"][..],
            &["QQ_PASSWORD", "QQ_MAIL_AUTH_CODE", "QQ_PWD"][..],
        ),
        (
            &["GMAIL_ACCOUNT", "GMAIL_ADDRESS", "GMAIL_EMAIL"][..],
            &["GMAIL_PASSWORD", "GMAIL_APP_PASSWORD", "GMAIL_PWD"][..],
        ),
    ] {
        if let (Some(address), Some(secret)) = (env_first(address_vars), env_first(secret_vars)) {
            return Ok((address, secret));
        }
    }
    Err(anyhow!("no mail credentials in the environment"))
}

/// A TCP stream that is either plaintext or TLS, with IDLE timeout support.
pub enum MailStream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl MailStream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Self::Plain(stream) => stream,
            Self::Tls(stream) => &stream.sock,
        }
    }
}

impl Read for MailStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buf),
            Self::Tls(stream) => stream.read(buf),
        }
    }
}

impl Write for MailStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buf),
            Self::Tls(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

impl SetReadTimeout for MailStream {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> imap::error::Result<()> {
        self.tcp()
            .set_read_timeout(timeout)
            .map_err(imap::error::Error::Io)
    }
}

pub type ImapSession = Session<MailStream>;

fn tls_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

fn tcp_connect(host: &str, port: u16) -> Result<TcpStream> {
    let mut last_err = None;
    let addrs = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolve {host}:{port}"))?;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(stream) => {
                stream.set_read_timeout(Some(IO_TIMEOUT))?;
                stream.set_write_timeout(Some(IO_TIMEOUT))?;
                return Ok(stream);
            }
            Err(err) => last_err = Some(err),
        }
    }
    Err(anyhow!(
        "connect to {host}:{port} failed: {}",
        last_err.map(|err| err.to_string()).unwrap_or_default()
    ))
}

fn tls_handshake(host: &str, tcp: TcpStream) -> Result<MailStream> {
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| anyhow!("invalid TLS server name: {host}"))?;
    let connection = rustls::ClientConnection::new(tls_config(), server_name)
        .with_context(|| format!("TLS setup for {host}"))?;
    Ok(MailStream::Tls(Box::new(rustls::StreamOwned::new(
        connection, tcp,
    ))))
}

/// Read one CRLF-terminated line from a plain TCP stream (pre-TLS phase of
/// STARTTLS, where no IMAP client object exists yet).
fn read_socket_line(reader: &mut BufReader<&TcpStream>) -> Result<String> {
    let mut line = String::new();
    reader.read_line(&mut line).context("read server line")?;
    if line.is_empty() {
        return Err(anyhow!("server closed the connection"));
    }
    Ok(line)
}

fn imap_starttls_upgrade(host: &str, tcp: TcpStream) -> Result<MailStream> {
    {
        let mut reader = BufReader::new(&tcp);
        let greeting = read_socket_line(&mut reader)?;
        if !greeting.starts_with("* OK") && !greeting.starts_with("* PREAUTH") {
            return Err(anyhow!("unexpected IMAP greeting: {}", greeting.trim()));
        }
        (&tcp).write_all(b"s0 STARTTLS\r\n")?;
        loop {
            let line = read_socket_line(&mut reader)?;
            if line.starts_with("s0 OK") {
                break;
            }
            if line.starts_with("s0 ") {
                return Err(anyhow!("server refused STARTTLS: {}", line.trim()));
            }
        }
    }
    tls_handshake(host, tcp)
}

fn open_imap_session(config: &MailConfig) -> Result<(ImapSession, Option<TcpStream>)> {
    let endpoint = &config.imap;
    let tcp = tcp_connect(&endpoint.host, endpoint.port)?;
    // A second handle to the same socket. `imap`'s IDLE support clears the
    // read timeout (sets it to None) when it finishes waiting and never
    // restores it; we re-arm through this clone so a later blocking read on a
    // half-open connection cannot hang the watcher forever.
    let timeout_handle = tcp.try_clone().ok();

    let (stream, greeting_pending) = match endpoint.security {
        Security::Plain => (MailStream::Plain(tcp), true),
        Security::Ssl => (tls_handshake(&endpoint.host, tcp)?, true),
        Security::StartTls => (imap_starttls_upgrade(&endpoint.host, tcp)?, false),
    };

    let mut client = Client::new(stream);
    if greeting_pending {
        client
            .read_greeting()
            .map_err(|err| anyhow!("IMAP greeting from {}: {err}", endpoint.host))?;
    }

    // Mask the account secret in any subsequent log line, as a backstop to
    // call sites only logging metadata.
    crate::logging::register_secret(config.secret.as_str());
    let mut session = client
        .login(config.address.as_str(), config.secret.as_str())
        .map_err(|(err, _)| {
            let server_error = err.to_string();
            crate::logging::warn(
                "imap",
                "login failed",
                &[
                    ("address", &config.address),
                    ("endpoint", &endpoint.label()),
                    ("err", &server_error),
                ],
            );
            anyhow!(
                "IMAP login failed for {} at {}: {server_error}\n{}",
                config.address,
                endpoint.label(),
                config.login_help(&server_error)
            )
        })?;
    crate::logging::info(
        "imap",
        "connected",
        &[
            ("address", &config.address),
            ("endpoint", &endpoint.label()),
        ],
    );

    if config.needs_imap_id {
        // NetEase rejects SELECT with "Unsafe Login" unless the client
        // identifies itself first. Failure here is non-fatal elsewhere.
        let _ = session.run_command_and_check_ok(
            "ID (\"name\" \"slip\" \"version\" \"0.2.0\" \"vendor\" \"slip\")",
        );
    }

    Ok((session, timeout_handle))
}

/// State of a selected mailbox, as reported by SELECT.
#[derive(Clone, Copy, Debug, Default)]
pub struct SelectInfo {
    pub uid_validity: u32,
    pub uid_next: u32,
    pub exists: u32,
}

/// A persistent IMAP connection with one selected mailbox.
pub struct MailboxSession {
    session: ImapSession,
    selected: Option<String>,
    /// Second handle to the socket, used to restore the read timeout the
    /// IDLE extension clears. `None` if the clone failed at connect time.
    timeout_handle: Option<TcpStream>,
}

impl MailboxSession {
    pub fn select(&mut self, mailbox: &str) -> Result<SelectInfo> {
        let encoded = encode_imap_utf7(mailbox);
        let info = self
            .session
            .select(&encoded)
            .with_context(|| format!("select mailbox {mailbox}"))?;
        self.selected = Some(mailbox.to_string());
        Ok(SelectInfo {
            uid_validity: info.uid_validity.unwrap_or_default(),
            uid_next: info.uid_next.unwrap_or_default(),
            exists: info.exists,
        })
    }

    pub fn uid_search(&mut self, query: &str) -> Result<Vec<u32>> {
        let uids = self
            .session
            .uid_search(query)
            .with_context(|| format!("UID SEARCH {query}"))?;
        let mut uids: Vec<u32> = uids.into_iter().collect();
        uids.sort_unstable();
        Ok(uids)
    }

    /// Inspect only protocol headers on recent UIDs, independent of the
    /// provider's potentially delayed SUBJECT search index. Never sets Seen.
    pub fn chat_header_uids(&mut self, start: u32, end: u32, marker: &str) -> Result<Vec<u32>> {
        if start > end || end == 0 {
            return Ok(Vec::new());
        }
        let fetches = self.session.uid_fetch(
            format!("{start}:{end}"),
            "(UID BODY.PEEK[HEADER.FIELDS (SUBJECT X-SLIP-VERSION)])",
        )?;
        Ok(fetches
            .iter()
            .filter_map(|fetch| {
                let raw = fetch.header()?;
                let parsed = parse_mail(raw).ok()?;
                let subject = parsed.headers.get_first_value("Subject")?;
                let version = parsed.headers.get_first_value("X-Slip-Version")?;
                (subject_matches(&subject, marker) && version.trim() == "1")
                    .then_some(fetch.uid?)
                    .filter(|uid| *uid >= start && *uid <= end)
            })
            .collect())
    }

    /// Fetch the raw RFC 5322 bytes of one message.
    pub fn fetch_raw(&mut self, uid: u32) -> Result<Option<Vec<u8>>> {
        let fetches = self
            .session
            .uid_fetch(uid.to_string(), "BODY.PEEK[]")
            .with_context(|| format!("UID FETCH {uid}"))?;
        Ok(fetches
            .iter()
            .find(|fetch| fetch.uid == Some(uid))
            .and_then(|fetch| fetch.body())
            .map(|body| body.to_vec()))
    }

    pub fn delete(&mut self, uid: u32) -> Result<()> {
        self.session
            .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
            .with_context(|| format!("mark UID {uid} deleted"))?;
        expunge_uid(&mut self.session, uid)
    }

    /// Gmail removes labels on an inbox EXPUNGE. Move only this validated
    /// chat to the special-use Trash first, then purge its Trash UID.
    /// The return value is true only for the final purge.
    pub fn burn_gmail(&mut self, uid: u32) -> Result<bool> {
        let trash = self.gmail_trash_folder()?;
        if self.selected.as_deref() == Some(trash.as_str()) {
            self.delete(uid)?;
            Ok(true)
        } else {
            self.session
                .uid_mv(uid.to_string(), encode_imap_utf7(&trash))
                .context("move saved Gmail chat to Trash; cleanup pending")?;
            Ok(false)
        }
    }

    /// Only provider sent-mail folders; no broad sweep of user folders.
    pub fn sent_folders(&mut self) -> Result<Vec<String>> {
        let folders = self.session.list(None, Some("*"))?;
        Ok(folders
            .iter()
            .filter_map(|name| {
                let decoded = decode_imap_utf7(name.name());
                let marked = name.attributes().iter().any(|attr| {
                    matches!(attr,
                NameAttribute::Custom(value) if value.eq_ignore_ascii_case("\\Sent"))
                });
                (marked
                    || matches!(
                        decoded.to_ascii_lowercase().as_str(),
                        "sent"
                            | "sent messages"
                            | "sent items"
                            | "已发送"
                            | "已发送邮件"
                            | "[gmail]/sent mail"
                    ))
                .then_some(decoded)
            })
            .collect())
    }

    pub fn gmail_trash_folder(&mut self) -> Result<String> {
        let folders = self.session.list(None, Some("*"))?;
        folders
            .iter()
            .find(|name| {
                name.attributes().iter().any(|attr| {
            matches!(attr, NameAttribute::Custom(value) if value.eq_ignore_ascii_case("\\Trash"))
        })
            })
            .map(|name| decode_imap_utf7(name.name()))
            .ok_or_else(|| anyhow!("Gmail Trash folder unavailable; remote cleanup pending"))
    }

    /// Create `folder` if it does not exist. Servers reject CREATE for an
    /// existing folder, which is expected and ignored.
    pub fn ensure_folder(&mut self, folder: &str) -> Result<()> {
        let _ = self.session.create(encode_imap_utf7(folder));
        Ok(())
    }

    /// Move one message from the selected mailbox to `folder`. Prefers `UID
    /// MOVE` (RFC 6851); if the server lacks it, falls back to COPY +
    /// \Deleted + [`expunge_uid`], which only ever removes the target message
    /// (or, on a server without UIDPLUS, leaves it flagged rather than
    /// nuking every \Deleted message). Either way, unrelated \Deleted mail is
    /// never touched.
    pub fn move_to(&mut self, uid: u32, folder: &str) -> Result<()> {
        let encoded = encode_imap_utf7(folder);
        if self.session.uid_mv(uid.to_string(), &encoded).is_ok() {
            return Ok(());
        }
        self.session
            .uid_copy(uid.to_string(), &encoded)
            .with_context(|| format!("copy UID {uid} to {folder}"))?;
        self.session
            .uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")
            .with_context(|| format!("mark UID {uid} deleted"))?;
        expunge_uid(&mut self.session, uid)
    }

    pub fn supports_idle(&mut self) -> bool {
        self.session
            .capabilities()
            .map(|caps| caps.has_str("IDLE"))
            .unwrap_or(false)
    }

    /// Block until the mailbox changes or `timeout` passes.
    pub fn idle_wait(&mut self, timeout: Duration) -> Result<WaitOutcome> {
        let outcome = self
            .session
            .idle()
            .context("enter IDLE")?
            .wait_with_timeout(timeout)
            .context("IDLE wait")?;
        // The IDLE handle cleared the socket read timeout on the way out;
        // restore it so the next blocking read cannot hang indefinitely.
        if let Some(handle) = &self.timeout_handle {
            let _ = handle.set_read_timeout(Some(IO_TIMEOUT));
        }
        Ok(outcome)
    }

    pub fn logout(mut self) {
        let _ = self.session.logout();
    }

    fn session_mut(&mut self) -> &mut ImapSession {
        &mut self.session
    }
}

/// Transport factory bound to one account configuration.
#[derive(Clone, Debug)]
pub struct MailCore {
    config: MailConfig,
    /// Timestamp of the last SMTP send, shared across clones so bursts (e.g.
    /// the resend queue draining several messages) are spaced out and do not
    /// trip a provider's rate limiter or spam heuristics.
    last_send: Arc<Mutex<Option<Instant>>>,
}

impl MailCore {
    pub fn new(config: MailConfig) -> Self {
        Self {
            config,
            last_send: Arc::new(Mutex::new(None)),
        }
    }

    pub fn config(&self) -> &MailConfig {
        &self.config
    }

    /// Minimum spacing between SMTP sends (default 800ms; `SLIP_SEND_MIN_MS`
    /// overrides, 0 disables).
    fn send_min_interval() -> Duration {
        let ms = env::var("SLIP_SEND_MIN_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(800);
        Duration::from_millis(ms)
    }

    /// Block until enough time has passed since the previous send.
    fn throttle_send(&self) {
        let interval = Self::send_min_interval();
        if interval.is_zero() {
            return;
        }
        let mut last = self.last_send.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < interval {
                std::thread::sleep(interval - elapsed);
            }
        }
        *last = Some(Instant::now());
    }

    /// Open a persistent IMAP session (no mailbox selected yet).
    pub fn connect(&self) -> Result<MailboxSession> {
        let (session, timeout_handle) = open_imap_session(&self.config)?;
        Ok(MailboxSession {
            session,
            selected: None,
            timeout_handle,
        })
    }

    /// Verify that the IMAP credentials work at all.
    pub fn check_login(&self) -> Result<()> {
        let session = self.connect()?;
        session.logout();
        Ok(())
    }

    /// Send prebuilt RFC 5322 bytes over SMTP.
    pub fn send_raw_mail(&self, to: &str, raw: &[u8]) -> Result<()> {
        self.throttle_send();
        let envelope = Envelope::new(
            Some(self.config.address.parse().context("parse from address")?),
            vec![to.parse().context("parse to address")?],
        )
        .context("build SMTP envelope")?;
        let transport = self.smtp_transport()?;
        transport
            .send_raw(&envelope, raw)
            .with_context(|| format!("SMTP send to {to} via {}", self.config.smtp.label()))?;
        Ok(())
    }

    fn smtp_transport(&self) -> Result<SmtpTransport> {
        let endpoint = &self.config.smtp;
        let mut builder =
            SmtpTransport::builder_dangerous(endpoint.host.as_str()).port(endpoint.port);
        if endpoint.security != Security::Plain {
            let params = TlsParameters::builder(endpoint.host.clone())
                .build()
                .context("build TLS parameters")?;
            builder = match endpoint.security {
                Security::Ssl => builder.tls(Tls::Wrapper(params)),
                Security::StartTls => builder.tls(Tls::Required(params)),
                Security::Plain => unreachable!(),
            };
        }
        Ok(builder
            .credentials(Credentials::new(
                self.config.address.clone(),
                self.config.secret.clone(),
            ))
            .timeout(Some(IO_TIMEOUT))
            .build())
    }

    // ------------------------------------------------------------------
    // One-shot operations kept for the scriptable/debug CLI.
    // ------------------------------------------------------------------

    pub fn list_mailboxes(&self) -> Result<Vec<MailboxInfo>> {
        let mut mailbox = self.connect()?;
        let names = mailbox
            .session_mut()
            .list(None, Some("*"))
            .context("LIST mailboxes")?;
        let result = names
            .iter()
            .map(|entry| MailboxInfo {
                name: decode_imap_utf7(entry.name()),
                raw_name: entry.name().to_string(),
                delimiter: entry.delimiter().map(ToOwned::to_owned),
                flags: entry
                    .attributes()
                    .iter()
                    .map(format_name_attribute)
                    .collect(),
            })
            .collect();
        mailbox.logout();
        Ok(result)
    }

    pub fn recent(
        &self,
        mailbox_name: &str,
        limit: usize,
        include_body: bool,
    ) -> Result<Vec<MessageSummary>> {
        let mut mailbox = self.connect()?;
        mailbox.select(mailbox_name)?;
        let uids = mailbox.uid_search("ALL")?;
        let selected = uids.into_iter().rev().take(limit).collect::<Vec<_>>();
        let messages = fetch_summaries(mailbox.session_mut(), &selected, include_body, None)?;
        mailbox.logout();
        Ok(messages)
    }

    pub fn recent_by_subject(
        &self,
        mailbox_name: &str,
        subject: &str,
        limit: usize,
        include_body: bool,
    ) -> Result<Vec<MessageSummary>> {
        let mut mailbox = self.connect()?;
        mailbox.select(mailbox_name)?;
        let query = format!("SUBJECT {}", imap_search_string(subject));
        let uids = mailbox.uid_search(&query)?;
        let selected = uids.into_iter().rev().take(limit).collect::<Vec<_>>();
        let messages = fetch_summaries(mailbox.session_mut(), &selected, include_body, None)?;
        mailbox.logout();
        Ok(messages
            .into_iter()
            .filter(|message| message.subject.trim() == subject)
            .collect())
    }

    pub fn search(
        &self,
        mailbox_name: &str,
        query: &str,
        limit: usize,
        include_body: bool,
    ) -> Result<(Vec<u32>, Vec<MessageSummary>)> {
        let mut mailbox = self.connect()?;
        mailbox.select(mailbox_name)?;
        let uids = mailbox.uid_search(query)?;
        let selected = uids.into_iter().rev().take(limit).collect::<Vec<_>>();
        let messages = fetch_summaries(mailbox.session_mut(), &selected, include_body, None)?;
        mailbox.logout();
        Ok((selected, messages))
    }

    pub fn fetch_one(
        &self,
        mailbox_name: &str,
        uid: u32,
        include_body: bool,
        save_attachments: Option<&Path>,
    ) -> Result<MessageSummary> {
        let mut mailbox = self.connect()?;
        mailbox.select(mailbox_name)?;
        let mut messages = fetch_summaries(
            mailbox.session_mut(),
            &[uid],
            include_body,
            save_attachments,
        )?;
        let message = messages
            .pop()
            .ok_or_else(|| anyhow!("message UID {uid} not found"))?;
        mailbox.logout();
        Ok(message)
    }

    pub fn mark_read(&self, request: MutationRequest) -> Result<MutationResult> {
        self.mark_seen(request, true)
    }

    pub fn mark_unread(&self, request: MutationRequest) -> Result<MutationResult> {
        self.mark_seen(request, false)
    }

    pub fn move_message(
        &self,
        mailbox_name: &str,
        uid: u32,
        to: &str,
        execute: bool,
    ) -> Result<MutationResult> {
        let mut mailbox = self.connect()?;
        mailbox.select(mailbox_name)?;
        let mut method = "UID MOVE".to_string();
        if execute {
            let encoded_to = encode_imap_utf7(to);
            let session = mailbox.session_mut();
            if session.uid_mv(uid.to_string(), &encoded_to).is_err() {
                method = "UID COPY + STORE Deleted + UID EXPUNGE".to_string();
                session.uid_copy(uid.to_string(), &encoded_to)?;
                session.uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)")?;
                expunge_uid(session, uid)?;
            }
        }
        mailbox.logout();
        Ok(MutationResult {
            action: "move".to_string(),
            dry_run: !execute,
            mailbox: Some(mailbox_name.to_string()),
            uid: Some(uid),
            to: Some(to.to_string()),
            method: Some(method),
        })
    }

    pub fn delete_message(&self, request: MutationRequest) -> Result<MutationResult> {
        let mut mailbox = self.connect()?;
        mailbox.select(&request.mailbox)?;
        if request.execute {
            mailbox.delete(request.uid)?;
        }
        mailbox.logout();
        Ok(MutationResult {
            action: "delete".to_string(),
            dry_run: !request.execute,
            mailbox: Some(request.mailbox),
            uid: Some(request.uid),
            to: None,
            method: None,
        })
    }

    pub fn create_mailbox(&self, name: &str, execute: bool) -> Result<MutationResult> {
        let mut mailbox = self.connect()?;
        if execute {
            mailbox.session_mut().create(encode_imap_utf7(name))?;
        }
        mailbox.logout();
        Ok(MutationResult {
            action: "create-mailbox".to_string(),
            dry_run: !execute,
            mailbox: Some(name.to_string()),
            uid: None,
            to: None,
            method: None,
        })
    }

    fn mark_seen(&self, request: MutationRequest, seen: bool) -> Result<MutationResult> {
        let mut mailbox = self.connect()?;
        mailbox.select(&request.mailbox)?;
        if request.execute {
            let flags = if seen {
                "+FLAGS.SILENT (\\Seen)"
            } else {
                "-FLAGS.SILENT (\\Seen)"
            };
            mailbox
                .session_mut()
                .uid_store(request.uid.to_string(), flags)?;
        }
        mailbox.logout();
        Ok(MutationResult {
            action: if seen { "mark-read" } else { "mark-unread" }.to_string(),
            dry_run: !request.execute,
            mailbox: Some(request.mailbox),
            uid: Some(request.uid),
            to: None,
            method: None,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MailboxInfo {
    pub name: String,
    pub raw_name: String,
    pub delimiter: Option<String>,
    pub flags: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MessageSummary {
    pub uid: u32,
    pub subject: String,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub date: String,
    pub timestamp: Option<i64>,
    pub message_id: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    #[serde(default)]
    pub attachments: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub saved_attachments: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct MutationRequest {
    pub mailbox: String,
    pub uid: u32,
    pub execute: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct MutationResult {
    pub action: String,
    pub dry_run: bool,
    pub mailbox: Option<String>,
    pub uid: Option<u32>,
    pub to: Option<String>,
    pub method: Option<String>,
}

pub fn imap_search_string(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// Expunge exactly one UID with `UID EXPUNGE` (RFC 4315 UIDPLUS), which
/// removes only the given message.
///
/// If the server lacks UIDPLUS this deliberately does NOT fall back to a bare
/// `EXPUNGE`: that would permanently delete every `\Deleted` message in the
/// mailbox, including unrelated mail another client flagged. The message keeps
/// its `\Deleted` flag (so it is hidden and cleaned up when the mailbox is
/// next expunged normally) but is never nuked alongside others.
fn expunge_uid(session: &mut ImapSession, uid: u32) -> Result<()> {
    session
        .uid_expunge(uid.to_string())
        .context("UID EXPUNGE failed; remote copy retained or flagged Deleted")?;
    Ok(())
}

fn fetch_summaries(
    session: &mut ImapSession,
    uids: &[u32],
    include_body: bool,
    save_attachments: Option<&Path>,
) -> Result<Vec<MessageSummary>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let uid_set = uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let fetches = session.uid_fetch(uid_set, "RFC822")?;
    let mut summaries = Vec::new();
    for fetch in fetches.iter() {
        summaries.push(parse_fetch(fetch, include_body, save_attachments)?);
    }
    summaries.sort_by_key(|message| std::cmp::Reverse(message.uid));
    Ok(summaries)
}

fn parse_fetch(
    fetch: &Fetch,
    include_body: bool,
    save_attachments: Option<&Path>,
) -> Result<MessageSummary> {
    let uid = fetch
        .uid
        .ok_or_else(|| anyhow!("server did not return UID"))?;
    let raw = fetch
        .body()
        .ok_or_else(|| anyhow!("server did not return message body"))?;
    let mut summary = summarize_raw(raw, include_body, save_attachments)?;
    summary.uid = uid;
    Ok(summary)
}

/// Parse raw RFC 5322 bytes into the legacy summary shape (uid left 0).
pub fn summarize_raw(
    raw: &[u8],
    include_body: bool,
    save_attachments: Option<&Path>,
) -> Result<MessageSummary> {
    let parsed = parse_mail(raw)?;
    let headers = &parsed.headers;
    let date_raw = headers.get_first_value("Date").unwrap_or_default();
    let timestamp = dateparse(&date_raw).ok();
    let attachments = attachment_names(&parsed);
    let links = extract_links(&parsed);
    let saved_attachments = if let Some(dir) = save_attachments {
        save_message_attachments(&parsed, dir)?
    } else {
        Vec::new()
    };

    Ok(MessageSummary {
        uid: 0,
        subject: decode_header(headers.get_first_value("Subject").as_deref()),
        from: decode_addresses(headers.get_first_header("From")),
        to: decode_addresses(headers.get_first_header("To")),
        cc: decode_addresses(headers.get_first_header("Cc")),
        date: date_raw,
        timestamp,
        message_id: headers.get_first_value("Message-ID").unwrap_or_default(),
        links,
        attachments,
        body: include_body.then(|| text_body(&parsed, 100_000)).flatten(),
        saved_attachments,
    })
}

fn decode_header(value: Option<&str>) -> String {
    value.unwrap_or_default().to_string()
}

fn decode_addresses(header: Option<&mailparse::MailHeader>) -> String {
    let Some(header) = header else {
        return String::new();
    };
    match addrparse_header(header) {
        Ok(addrs) => addrs.to_string(),
        Err(_) => decode_header(Some(header.get_value().as_str())),
    }
}

fn text_body(parsed: &ParsedMail<'_>, max_chars: usize) -> Option<String> {
    let mut html_candidate = None;
    for part in flatten_parts(parsed) {
        let disposition = part.get_content_disposition();
        if disposition.disposition == mailparse::DispositionType::Attachment {
            continue;
        }
        let mimetype = part.ctype.mimetype.to_ascii_lowercase();
        if mimetype == "text/plain" {
            return part
                .get_body()
                .ok()
                .map(|body| truncate_chars(&body, max_chars));
        }
        if mimetype == "text/html" && html_candidate.is_none() {
            html_candidate = part
                .get_body()
                .ok()
                .map(|body| truncate_chars(&html_to_text(&body), max_chars));
        }
    }
    html_candidate
}

fn html_to_text(html: &str) -> String {
    let mut text = String::new();
    let mut tag = String::new();
    let mut in_tag = false;
    let mut in_script = false;
    let mut in_style = false;
    let mut entity = String::new();
    let mut in_entity = false;

    for ch in html.chars() {
        if in_tag {
            if ch == '>' {
                let normalized = tag
                    .trim()
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let closing = tag.trim_start().starts_with('/');

                match normalized.as_str() {
                    "script" => in_script = !closing,
                    "style" => in_style = !closing,
                    "br" | "p" | "div" | "tr" | "li" | "table" | "section" | "article" | "h1"
                    | "h2" | "h3" | "h4" | "h5" | "h6"
                        if !in_script && !in_style =>
                    {
                        push_newline(&mut text);
                    }
                    _ => {}
                }

                tag.clear();
                in_tag = false;
            } else {
                tag.push(ch);
            }
            continue;
        }

        if ch == '<' {
            in_tag = true;
            tag.clear();
            continue;
        }

        if in_script || in_style {
            continue;
        }

        if in_entity {
            if ch == ';' {
                text.push_str(&decode_html_entity(&entity));
                entity.clear();
                in_entity = false;
            } else if entity.len() < 16 {
                entity.push(ch);
            } else {
                text.push('&');
                text.push_str(&entity);
                entity.clear();
                in_entity = false;
                text.push(ch);
            }
            continue;
        }

        if ch == '&' {
            in_entity = true;
            entity.clear();
        } else {
            text.push(ch);
        }
    }

    if in_entity {
        text.push('&');
        text.push_str(&entity);
    }

    normalize_text_whitespace(&text)
}

fn push_newline(text: &mut String) {
    if !text.ends_with('\n') {
        text.push('\n');
    }
}

fn decode_html_entity(entity: &str) -> String {
    match entity {
        "nbsp" => " ".to_string(),
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" => "'".to_string(),
        value if value.starts_with("#x") || value.starts_with("#X") => {
            u32::from_str_radix(&value[2..], 16)
                .ok()
                .and_then(char::from_u32)
                .map(|ch| ch.to_string())
                .unwrap_or_else(|| format!("&{entity};"))
        }
        value if value.starts_with('#') => value[1..]
            .parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .map(|ch| ch.to_string())
            .unwrap_or_else(|| format!("&{entity};")),
        _ => format!("&{entity};"),
    }
}

fn decode_html_entities(value: &str) -> String {
    let mut output = String::new();
    let mut entity = String::new();
    let mut in_entity = false;

    for ch in value.chars() {
        if in_entity {
            if ch == ';' {
                output.push_str(&decode_html_entity(&entity));
                entity.clear();
                in_entity = false;
            } else if entity.len() < 16 {
                entity.push(ch);
            } else {
                output.push('&');
                output.push_str(&entity);
                entity.clear();
                in_entity = false;
                output.push(ch);
            }
            continue;
        }

        if ch == '&' {
            in_entity = true;
            entity.clear();
        } else {
            output.push(ch);
        }
    }

    if in_entity {
        output.push('&');
        output.push_str(&entity);
    }

    output
}

fn normalize_text_whitespace(text: &str) -> String {
    let mut lines = Vec::new();
    for line in text.lines() {
        let mut normalized = String::new();
        let mut previous_space = false;
        for ch in line.chars() {
            if ch.is_whitespace() {
                if !previous_space {
                    normalized.push(' ');
                    previous_space = true;
                }
            } else {
                normalized.push(ch);
                previous_space = false;
            }
        }
        let trimmed = normalized.trim();
        if !trimmed.is_empty() {
            lines.push(trimmed.to_string());
        }
    }
    lines.join("\n")
}

fn attachment_names(parsed: &ParsedMail<'_>) -> Vec<String> {
    flatten_parts(parsed)
        .into_iter()
        .enumerate()
        .filter_map(|(index, part)| part_filename(part, index + 1))
        .collect()
}

fn save_message_attachments(parsed: &ParsedMail<'_>, output_dir: &Path) -> Result<Vec<String>> {
    std::fs::create_dir_all(output_dir)?;
    let mut saved = Vec::new();
    for (index, part) in flatten_parts(parsed).into_iter().enumerate() {
        let Some(filename) = part_filename(part, index + 1) else {
            continue;
        };
        let safe = safe_filename(&filename, index + 1);
        let target: PathBuf = output_dir.join(safe);
        let body = part.get_body_raw()?;
        std::fs::write(&target, body)?;
        saved.push(target.display().to_string());
    }
    Ok(saved)
}

fn part_filename(part: &ParsedMail<'_>, fallback_index: usize) -> Option<String> {
    let disposition = part.get_content_disposition();
    if let Some(filename) = disposition.params.get("filename") {
        return Some(filename.clone());
    }
    if let Some(name) = part.ctype.params.get("name") {
        return Some(name.clone());
    }
    let mimetype = part.ctype.mimetype.to_ascii_lowercase();
    if mimetype.starts_with("image/") {
        let ext = mimetype
            .strip_prefix("image/")
            .filter(|value| !value.is_empty())
            .unwrap_or("img");
        return Some(format!("inline-image-{fallback_index}.{ext}"));
    }
    None
}

fn extract_links(parsed: &ParsedMail<'_>) -> Vec<String> {
    let mut links = Vec::new();
    for part in flatten_parts(parsed) {
        let disposition = part.get_content_disposition();
        if disposition.disposition == mailparse::DispositionType::Attachment {
            continue;
        }
        let mimetype = part.ctype.mimetype.to_ascii_lowercase();
        if mimetype == "text/plain" {
            if let Ok(body) = part.get_body() {
                links.extend(extract_urls(&body));
            }
        } else if mimetype == "text/html"
            && let Ok(body) = part.get_body()
        {
            links.extend(extract_html_attr_urls(&body));
            links.extend(extract_urls(&html_to_text(&body)));
        }
    }
    dedupe_preserve_order(links)
}

fn extract_html_attr_urls(html: &str) -> Vec<String> {
    let mut links = Vec::new();
    for attr in ["href=", "src="] {
        let mut rest = html;
        while let Some(index) = rest.to_ascii_lowercase().find(attr) {
            rest = &rest[index + attr.len()..];
            let trimmed = rest.trim_start();
            let Some(quote) = trimmed.chars().next() else {
                break;
            };
            if quote != '"' && quote != '\'' {
                continue;
            }
            let value = &trimmed[quote.len_utf8()..];
            let Some(end) = value.find(quote) else {
                break;
            };
            let link = decode_html_entities(value[..end].trim());
            if is_supported_link(&link) {
                links.push(link);
            }
            rest = &value[end + quote.len_utf8()..];
        }
    }
    links
}

fn extract_urls(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|token| {
            let cleaned = token.trim_matches(|ch: char| {
                matches!(
                    ch,
                    '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
                )
            });
            let cleaned = cleaned.trim_end_matches(['.', ':', '!', '?']);
            is_supported_link(cleaned).then(|| cleaned.to_string())
        })
        .collect()
}

fn is_supported_link(value: &str) -> bool {
    value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("mailto:")
        || value.starts_with("cid:")
}

fn dedupe_preserve_order(values: Vec<String>) -> Vec<String> {
    let mut unique = Vec::new();
    for value in values {
        if !unique.iter().any(|existing| existing == &value) {
            unique.push(value);
        }
    }
    unique
}

pub(crate) fn flatten_parts<'a>(parsed: &'a ParsedMail<'a>) -> Vec<&'a ParsedMail<'a>> {
    if parsed.subparts.is_empty() {
        vec![parsed]
    } else {
        parsed.subparts.iter().flat_map(flatten_parts).collect()
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

pub fn safe_filename(value: &str, fallback_index: usize) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | ' ') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim()
        .to_string();
    if sanitized.is_empty() {
        format!("attachment-{fallback_index}")
    } else {
        sanitized
    }
}

fn format_name_attribute(attr: &NameAttribute<'_>) -> String {
    match attr {
        NameAttribute::NoInferiors => "\\NoInferiors".to_string(),
        NameAttribute::NoSelect => "\\NoSelect".to_string(),
        NameAttribute::Marked => "\\Marked".to_string(),
        NameAttribute::Unmarked => "\\Unmarked".to_string(),
        NameAttribute::Custom(value) => value.to_string(),
    }
}

pub fn encode_imap_utf7(value: &str) -> String {
    let mut output = String::new();
    let mut unicode_run = String::new();

    for ch in value.chars() {
        if (' '..='~').contains(&ch) {
            flush_utf7_run(&mut output, &mut unicode_run);
            if ch == '&' {
                output.push_str("&-");
            } else {
                output.push(ch);
            }
        } else {
            unicode_run.push(ch);
        }
    }
    flush_utf7_run(&mut output, &mut unicode_run);
    output
}

fn flush_utf7_run(output: &mut String, run: &mut String) {
    if run.is_empty() {
        return;
    }
    let mut bytes = Vec::new();
    for unit in run.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    let encoded = STANDARD
        .encode(bytes)
        .trim_end_matches('=')
        .replace('/', ",");
    output.push('&');
    output.push_str(&encoded);
    output.push('-');
    run.clear();
}

pub fn decode_imap_utf7(value: &str) -> String {
    let mut output = String::new();
    let mut rest = value;

    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        rest = &rest[start + 1..];
        let Some(end) = rest.find('-') else {
            output.push('&');
            output.push_str(rest);
            return output;
        };
        let token = &rest[..end];
        if token.is_empty() {
            output.push('&');
        } else {
            let mut standard = token.replace(',', "/");
            standard.push_str(&"=".repeat((4 - standard.len() % 4) % 4));
            match STANDARD.decode(standard) {
                Ok(bytes) => {
                    let units = bytes
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                        .collect::<Vec<_>>();
                    match String::from_utf16(&units) {
                        Ok(decoded) => output.push_str(&decoded),
                        Err(_) => {
                            output.push('&');
                            output.push_str(token);
                            output.push('-');
                        }
                    }
                }
                Err(_) => {
                    output.push('&');
                    output.push_str(token);
                    output.push('-');
                }
            }
        }
        rest = &rest[end + 1..];
    }
    output.push_str(rest);
    output
}
