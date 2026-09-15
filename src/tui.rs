//! Ratatui frontend.
//!
//! This is a thin view layer over [`crate::engine::Engine`]: it renders state
//! and translates key presses into [`Command`]s, then drains
//! [`EngineEvent`]s each frame. All network and store work lives on the
//! engine's threads, so the UI never blocks (docs/DESIGN.md). A GUI frontend
//! would drive the same engine the same way.

use crate::{
    cache::{DeliveryStatus, MailCache, StoredChatLine},
    chat::{
        ChatClient, ChatSessionSummary, NewMessageNotice, format_chat_timestamp, looks_like_email,
        parse_composer, stored_chat_timestamp,
    },
    core::{MailConfig, MailCore},
    engine::{Command, Engine, Event as EngineEvent},
    providers::{Endpoint, Security, provider_for_address},
};
use anyhow::{Result, anyhow};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::{
    collections::HashMap,
    io::{self, Stdout, Write},
    sync::mpsc::{Receiver, Sender, channel},
    thread,
    time::Duration,
};

/// Inline image thumbnail budget, in terminal cells.
const THUMB_COLS: u16 = 40;
const THUMB_ROWS: u16 = 14;
/// Cap background decode jobs started per UI tick, so opening a big
/// conversation drains across frames instead of spawning a thread storm.
const MAX_THUMBS_PER_TICK: usize = 4;

/// Read and render one image file into half-block thumbnail lines. Runs on a
/// background thread. Returns an empty Vec on any failure (too large,
/// unreadable, undecodable) — the empty result is still cached so the image
/// is never retried.
fn decode_thumbnail(path: &str) -> Vec<Line<'static>> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(_) => return Vec::new(),
    };
    if meta.len() > crate::imgview::MAX_THUMB_FILE_BYTES {
        return Vec::new();
    }
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    crate::imgview::render_thumbnail(&bytes, THUMB_COLS, THUMB_ROWS).unwrap_or_default()
}

// ----------------------------------------------------------------------
// Entry point
// ----------------------------------------------------------------------

pub fn run(mailbox: String) -> Result<()> {
    crate::logging::init();
    let cache = MailCache::default();
    let mut login_status = String::new();
    let config = match cache.load_login() {
        Ok(Some(config)) => Some(config),
        Ok(None) => None,
        Err(err) => {
            login_status = format!("Login cache unreadable: {err:#}");
            None
        }
    };

    let mut guard = TerminalGuard::new()?;
    let config = match config {
        Some(config) => config,
        None => match run_login_wizard(&mut guard, &cache, login_status)? {
            Some(config) => config,
            None => return Ok(()),
        },
    };

    run_chat(&mut guard, cache, config, mailbox)
}

// ----------------------------------------------------------------------
// Login wizard
// ----------------------------------------------------------------------

#[derive(Clone, Copy, Eq, PartialEq)]
enum WizardStep {
    Address,
    ImapServer,
    SmtpServer,
    Secret,
}

struct Wizard {
    step: WizardStep,
    address: String,
    imap_input: String,
    smtp_input: String,
    secret: String,
    status: String,
}

impl Wizard {
    fn provider(&self) -> Option<&'static crate::providers::Provider> {
        provider_for_address(&self.address)
    }

    fn custom_needed(&self) -> bool {
        looks_like_email(self.address.trim()) && self.provider().is_none()
    }

    fn field(&mut self) -> &mut String {
        match self.step {
            WizardStep::Address => &mut self.address,
            WizardStep::ImapServer => &mut self.imap_input,
            WizardStep::SmtpServer => &mut self.smtp_input,
            WizardStep::Secret => &mut self.secret,
        }
    }

    fn config(&self) -> Result<MailConfig> {
        let address = self.address.trim().to_ascii_lowercase();
        if !looks_like_email(&address) {
            return Err(anyhow!("enter a valid email address"));
        }
        if self.secret.trim().is_empty() {
            return Err(anyhow!("password / authorization code is required"));
        }
        let config = match self.provider() {
            Some(provider) => MailConfig::from_provider(provider, address, self.secret.clone()),
            None => {
                let imap = parse_endpoint(&self.imap_input, 993)?;
                let smtp = parse_endpoint(&self.smtp_input, 465)?;
                MailConfig::custom(address, self.secret.clone(), imap, smtp)
            }
        };
        Ok(config)
    }
}

/// Parse "host", "host:port", or "host:port:security".
fn parse_endpoint(input: &str, default_port: u16) -> Result<Endpoint> {
    let input = input.trim();
    if input.is_empty() {
        return Err(anyhow!(
            "server is required (host:port or host:port:security)"
        ));
    }
    let mut parts = input.split(':');
    let host = parts
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("server host is required"))?
        .to_string();
    let port = match parts.next() {
        Some(value) => value
            .parse::<u16>()
            .map_err(|_| anyhow!("invalid port: {value}"))?,
        None => default_port,
    };
    let security = match parts.next() {
        Some(value) => value.parse::<Security>().map_err(|err| anyhow!("{err}"))?,
        None => match port {
            993 | 465 => Security::Ssl,
            587 | 143 => Security::StartTls,
            25 => Security::Plain,
            _ => Security::Ssl,
        },
    };
    Ok(Endpoint::new(host, port, security))
}

fn run_login_wizard(
    guard: &mut TerminalGuard,
    cache: &MailCache,
    initial_status: String,
) -> Result<Option<MailConfig>> {
    let mut wizard = Wizard {
        step: WizardStep::Address,
        address: String::new(),
        imap_input: String::new(),
        smtp_input: String::new(),
        secret: String::new(),
        status: initial_status,
    };

    loop {
        guard.terminal.draw(|frame| draw_wizard(frame, &wizard))?;
        if !event::poll(Duration::from_millis(120))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                wizard.field().clear();
            }
            KeyCode::Backspace => {
                wizard.field().pop();
            }
            KeyCode::BackTab | KeyCode::Up => {
                wizard.step = match wizard.step {
                    WizardStep::Address => WizardStep::Address,
                    WizardStep::ImapServer => WizardStep::Address,
                    WizardStep::SmtpServer => WizardStep::ImapServer,
                    WizardStep::Secret if wizard.custom_needed() => WizardStep::SmtpServer,
                    WizardStep::Secret => WizardStep::Address,
                };
                wizard.status.clear();
            }
            KeyCode::Enter => match wizard.step {
                WizardStep::Address => {
                    let address = wizard.address.trim().to_string();
                    if !looks_like_email(&address) {
                        wizard.status = "Enter a valid email address.".to_string();
                        continue;
                    }
                    if let Some(provider) = wizard.provider() {
                        wizard.status = format!(
                            "{}: IMAP {}, SMTP {}",
                            provider.display_name,
                            provider.imap().label(),
                            provider.smtp().label()
                        );
                        wizard.step = WizardStep::Secret;
                    } else {
                        wizard.status =
                            "Unknown domain — enter the IMAP server details.".to_string();
                        wizard.step = WizardStep::ImapServer;
                    }
                }
                WizardStep::ImapServer => match parse_endpoint(&wizard.imap_input, 993) {
                    Ok(endpoint) => {
                        wizard.status = format!("IMAP {}", endpoint.label());
                        wizard.step = WizardStep::SmtpServer;
                    }
                    Err(err) => wizard.status = err.to_string(),
                },
                WizardStep::SmtpServer => match parse_endpoint(&wizard.smtp_input, 465) {
                    Ok(endpoint) => {
                        wizard.status = format!("SMTP {}", endpoint.label());
                        wizard.step = WizardStep::Secret;
                    }
                    Err(err) => wizard.status = err.to_string(),
                },
                WizardStep::Secret => match wizard.config() {
                    Ok(config) => {
                        wizard.status = "Connecting…".to_string();
                        guard.terminal.draw(|frame| draw_wizard(frame, &wizard))?;
                        match MailCore::new(config.clone()).check_login() {
                            Ok(()) => {
                                if let Err(err) = cache.save_login(&config) {
                                    wizard.status = format!("Login ok; cache failed: {err:#}");
                                }
                                return Ok(Some(config));
                            }
                            Err(err) => {
                                wizard.status = format!("Login failed: {err:#}");
                            }
                        }
                    }
                    Err(err) => wizard.status = err.to_string(),
                },
            },
            KeyCode::Char(ch) => {
                wizard.field().push(ch);
            }
            _ => {}
        }
    }
}

fn draw_wizard(frame: &mut Frame<'_>, wizard: &Wizard) {
    let (title, prompt, hint) = match wizard.step {
        WizardStep::Address => (
            "Sign in with your mailbox",
            "Email",
            "Any IMAP mailbox works. QQ, Gmail, Outlook, 163/126, iCloud, Yahoo are preset.",
        ),
        WizardStep::ImapServer => (
            "IMAP server",
            "IMAP",
            "host:port or host:port:security (ssl | starttls | plain), e.g. imap.example.com:993",
        ),
        WizardStep::SmtpServer => (
            "SMTP server",
            "SMTP",
            "host:port or host:port:security, e.g. smtp.example.com:465",
        ),
        WizardStep::Secret => (
            "Authorize mailbox",
            wizard
                .provider()
                .map(|provider| provider.secret_label)
                .unwrap_or("Password"),
            wizard
                .provider()
                .map(|provider| provider.secret_hint)
                .unwrap_or("Use the account's IMAP password or app password."),
        ),
    };
    let input = match wizard.step {
        WizardStep::Address => wizard.address.clone(),
        WizardStep::ImapServer => wizard.imap_input.clone(),
        WizardStep::SmtpServer => wizard.smtp_input.clone(),
        WizardStep::Secret => "*".repeat(wizard.secret.chars().count()),
    };

    let accent = Style::default()
        .fg(Color::Rgb(70, 95, 255))
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::Rgb(105, 112, 116));
    let strong = Style::default()
        .fg(Color::Rgb(30, 50, 56))
        .add_modifier(Modifier::BOLD);

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("Slip", strong),
            Span::raw("  "),
            Span::styled("chat over email", dim),
        ]),
        Line::from(""),
        Line::from(Span::styled(title, accent)),
        Line::from(""),
        Line::from(Span::styled(hint, dim)),
        Line::from(""),
    ];
    if wizard.step != WizardStep::Address {
        lines.push(Line::from(vec![
            Span::styled("Email   ", dim),
            Span::styled(wizard.address.clone(), strong),
        ]));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(prompt, strong)));
    lines.push(Line::from(vec![
        Span::styled("› ", dim),
        Span::styled(input, strong),
        Span::styled("█", Style::default().fg(Color::Rgb(0, 150, 140))),
    ]));
    lines.push(Line::from(""));
    if !wizard.status.trim().is_empty() {
        lines.push(Line::from(Span::styled(wizard.status.clone(), dim)));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        "Enter continue · Shift+Tab back · Ctrl+U clear · Esc quit",
        dim,
    )));

    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        frame.area(),
    );
}

// ----------------------------------------------------------------------
// Chat screen
// ----------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct SlashCommand {
    name: &'static str,
    description: &'static str,
}

const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/open",
        description: "Open a conversation (alias: /resume)",
    },
    SlashCommand {
        name: "/add",
        description: "Invite or accept a contact by email",
    },
    SlashCommand {
        name: "/sync",
        description: "Sync now (usually automatic)",
    },
    SlashCommand {
        name: "/info",
        description: "Show keys and encryption state",
    },
    SlashCommand {
        name: "/trust",
        description: "Accept a changed contact key",
    },
    SlashCommand {
        name: "/encrypt",
        description: "encrypt on|off for this contact",
    },
    SlashCommand {
        name: "/retry",
        description: "Resend the last failed message",
    },
    SlashCommand {
        name: "/name",
        description: "Set a local name: /name <email> <alias>",
    },
    SlashCommand {
        name: "/help",
        description: "Show commands",
    },
    SlashCommand {
        name: "/quit",
        description: "Exit Slip",
    },
];

#[derive(Clone, Copy, Eq, PartialEq)]
enum Focus {
    Contacts,
    Composer,
}

struct App {
    account: String,
    sessions: Vec<ChatSessionSummary>,
    selected: usize,
    active: Option<String>,
    lines: Vec<StoredChatLine>,
    composer: String,
    slash_selected: usize,
    chat_scroll: usize,
    focus: Focus,
    status: String,
    watcher_status: String,
    quit: bool,
    /// A modal panel (from /info or /help). Any key dismisses it. Shown over
    /// the chat area so key fingerprints are readable, not lost on the
    /// transient status line.
    overlay: Option<String>,
    /// Inline image thumbnails, keyed by the media's local path. A key is
    /// present as soon as decoding is dispatched (empty = pending or failed),
    /// so an image is only ever decoded once. Decoding runs on a background
    /// thread; results arrive over `thumb_rx`.
    thumbnails: HashMap<String, Vec<Line<'static>>>,
    thumb_tx: Sender<(String, Vec<Line<'static>>)>,
    thumb_rx: Receiver<(String, Vec<Line<'static>>)>,
    images_enabled: bool,
    local: ChatClient,
    engine: Engine,
}

impl App {
    fn selected_contact(&self) -> Option<&str> {
        self.sessions
            .get(self.selected)
            .map(|summary| summary.contact.as_str())
    }

    fn refresh_sessions(&mut self) {
        match self.local.sessions() {
            Ok(sessions) => {
                let previous = self.selected_contact().map(ToOwned::to_owned);
                self.sessions = sessions;
                if let Some(previous) = previous {
                    self.selected = self
                        .sessions
                        .iter()
                        .position(|summary| summary.contact == previous)
                        .unwrap_or(0);
                }
                self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
            }
            Err(err) => self.status = format!("Session list failed: {err:#}"),
        }
    }

    /// Reload the open conversation. `reset_scroll` is true only when the user
    /// deliberately opens a conversation; background reloads (a pushed message
    /// arriving, a delivery-status update) keep the scroll position so they do
    /// not yank the view while the user is reading older history.
    fn reload_active(&mut self, reset_scroll: bool) {
        let Some(active) = self.active.clone() else {
            self.lines.clear();
            return;
        };
        match self.local.session(&active) {
            Ok(session) => {
                self.lines = session.messages;
                if reset_scroll {
                    self.chat_scroll = 0;
                }
                // Dispatch happens on the UI loop tick (build_thumbnails),
                // off the UI thread; nothing to decode synchronously here.
            }
            Err(err) => self.status = format!("Load session failed: {err:#}"),
        }
    }

    /// Drain any thumbnails that finished decoding on background threads.
    fn drain_thumbnails(&mut self) {
        while let Ok((path, lines)) = self.thumb_rx.try_recv() {
            self.thumbnails.insert(path, lines);
        }
    }

    /// Dispatch background decoding for not-yet-seen image attachments in the
    /// open conversation. Reading and decoding never run on the UI thread, so
    /// a huge or malicious image cannot freeze or OOM the interface. Each
    /// image is dispatched once (a cache key is inserted immediately as a
    /// pending marker); at most `MAX_THUMBS_PER_TICK` decodes start per call so
    /// opening a large conversation drains across frames instead of spawning a
    /// thread storm.
    fn build_thumbnails(&mut self) {
        if !self.images_enabled {
            return;
        }
        let mut dispatched = 0;
        let paths: Vec<String> = self
            .lines
            .iter()
            .flat_map(|line| line.media.iter())
            .filter(|media| media.kind == crate::protocol::MediaKind::Image)
            .map(|media| media.path.clone())
            .collect();
        for path in paths {
            if dispatched >= MAX_THUMBS_PER_TICK {
                break;
            }
            if self.thumbnails.contains_key(&path) {
                continue;
            }
            // Pending marker: an empty thumbnail renders nothing and blocks
            // re-dispatch. The worker overwrites it (empty again on failure).
            self.thumbnails.insert(path.clone(), Vec::new());
            let tx = self.thumb_tx.clone();
            thread::spawn(move || {
                let lines = decode_thumbnail(&path);
                let _ = tx.send((path, lines));
            });
            dispatched += 1;
        }
    }

    fn open_chat(&mut self, address: &str) {
        let address = address.trim().to_ascii_lowercase();
        if !looks_like_email(&address) {
            self.status = "Usage: /open <email>".to_string();
            return;
        }
        self.engine.send(Command::AddContact(address.clone()));
        self.active = Some(address.clone());
        self.reload_active(true);
        self.engine.send(Command::MarkRead(address.clone()));
        self.refresh_sessions();
        if let Some(position) = self
            .sessions
            .iter()
            .position(|summary| summary.contact == address)
        {
            self.selected = position;
        }
        self.focus = Focus::Composer;
        self.status = format!("Chat: {address}");
    }

    fn submit_composer(&mut self) {
        let input = self.composer.trim().to_string();
        self.composer.clear();
        self.slash_selected = 0;
        if input.is_empty() {
            return;
        }
        if let Some(command) = input.strip_prefix('/') {
            self.run_slash(command.trim());
            return;
        }
        let Some(contact) = self.active.clone() else {
            self.status = "Open a conversation first: /open <email>".to_string();
            return;
        };
        let parsed = parse_composer(&input);
        for path in &parsed.attachments {
            if !path.is_file() {
                self.status = format!("Attachment not found: {}", path.display());
                return;
            }
        }
        if !self.engine.send(Command::Send {
            to: contact.clone(),
            body: parsed.body,
            attachments: parsed.attachments,
        }) {
            self.status = "Engine stopped; cannot send.".to_string();
            return;
        }
        self.status = format!("Sending to {contact}…");
    }

    fn run_slash(&mut self, command: &str) {
        let mut parts = command.split_whitespace();
        let name = parts.next().unwrap_or_default();
        let arg = parts.next().map(ToOwned::to_owned);
        match name {
            "open" | "resume" => match arg {
                Some(address) => self.open_chat(&address),
                None => {
                    self.focus = Focus::Contacts;
                    self.status = "Pick a contact (↑/↓, Enter).".to_string();
                }
            },
            "add" => match arg {
                Some(address) if looks_like_email(address.trim()) => {
                    let address = address.trim().to_ascii_lowercase();
                    self.engine.send(Command::AddContact(address.clone()));
                    self.status = format!("Queued invitation/acceptance for {address}.");
                }
                _ => self.status = "Usage: /add <email>".to_string(),
            },
            "name" => {
                // /name <email> <alias words…> — the alias is the rest.
                let alias = parts.collect::<Vec<_>>().join(" ");
                match arg {
                    Some(address) if looks_like_email(address.trim()) => {
                        match self.local.set_alias(address.trim(), &alias) {
                            Ok(Some(name)) => {
                                self.status = format!("Named {} → {name}", address.trim());
                                self.reload_active(false);
                                self.refresh_sessions();
                            }
                            Ok(None) => {
                                self.status = format!("Cleared name for {}", address.trim());
                                self.refresh_sessions();
                            }
                            Err(err) => self.status = format!("name failed: {err:#}"),
                        }
                    }
                    _ => self.status = "Usage: /name <email> <alias>".to_string(),
                }
            }
            "sync" => {
                self.engine.send(Command::Sync);
                self.status = "Syncing…".to_string();
            }
            "info" => match self.active.clone() {
                Some(contact) => match self.local.contact_info(&contact) {
                    Ok(info) => {
                        use crate::grouped_fingerprint;
                        let mut text = String::new();
                        text.push_str(&format!("Conversation with {contact}\n\n"));
                        text.push_str(&format!(
                            "Your safety code    {}\n",
                            grouped_fingerprint(&info.my_fingerprint)
                        ));
                        text.push_str(&format!(
                            "Their safety code   {}\n",
                            info.peer_fingerprint
                                .as_deref()
                                .map(grouped_fingerprint)
                                .unwrap_or_else(|| "(no key seen yet)".to_string())
                        ));
                        text.push_str(&format!(
                            "Encryption          {}\n",
                            if info.encryption_active {
                                "ON — messages are end-to-end encrypted"
                            } else if info.encrypt_enabled {
                                "off — waiting for their key (first message is plaintext)"
                            } else {
                                "off — disabled for this contact (/encrypt on)"
                            }
                        ));
                        if let Some(pending) = &info.pending_fingerprint {
                            text.push_str(&format!(
                                "\n⚠ Their identity key CHANGED.\n  Pending safety code {}\n  \
                                 Verify it with them out-of-band, then run /trust to accept it.\n  \
                                 Until then, messages keep going to the old trusted key.",
                                grouped_fingerprint(pending)
                            ));
                        }
                        text.push_str(
                            "\n\nRead the safety codes aloud to each other to confirm no one is\n\
                             in the middle. No forward secrecy yet: a stolen identity key can\n\
                             decrypt past messages an attacker archived.\n\n\
                             Press any key to close.",
                        );
                        self.overlay = Some(text);
                    }
                    Err(err) => self.status = format!("info failed: {err:#}"),
                },
                None => self.status = "Open a conversation first.".to_string(),
            },
            "trust" => match self.active.clone() {
                Some(contact) => {
                    self.engine.send(Command::Trust(contact));
                }
                None => self.status = "Open a conversation first.".to_string(),
            },
            "encrypt" => match (self.active.clone(), arg.as_deref()) {
                (Some(contact), Some("on")) => {
                    self.engine.send(Command::Encrypt(contact, true));
                }
                (Some(contact), Some("off")) => {
                    self.engine.send(Command::Encrypt(contact, false));
                }
                (Some(_), _) => self.status = "Usage: /encrypt on|off".to_string(),
                (None, _) => self.status = "Open a conversation first.".to_string(),
            },
            "retry" => match self.active.clone() {
                Some(contact) => {
                    self.engine.send(Command::Retry(contact));
                    self.status = "Retrying…".to_string();
                }
                None => self.status = "Open a conversation first.".to_string(),
            },
            "help" => {
                let mut text = String::from("Slip commands\n\n");
                for command in SLASH_COMMANDS {
                    text.push_str(&format!("  {:<10} {}\n", command.name, command.description));
                }
                text.push_str(
                    "\nSwitch chats   Ctrl+P / Ctrl+N (prev/next), or ↑/↓ then Enter,\n\
                     \x20              or /open <email>\n\
                     Send           type a message, Enter\n\
                     Attach         @path (drag a file in after typing @)\n\
                     Open image     Ctrl+O (newest, full-size)\n\
                     Scroll history PgUp / PgDn\n\n\
                     Press any key to close.",
                );
                self.overlay = Some(text);
            }
            "quit" | "exit" => {
                self.quit = true;
            }
            "" => {}
            other => self.status = format!("Unknown command: /{other}"),
        }
    }

    fn slash_menu_active(&self) -> bool {
        self.composer.trim_start().starts_with('/')
    }

    fn slash_matches(&self) -> Vec<&'static SlashCommand> {
        let prefix = self
            .composer
            .split_whitespace()
            .next()
            .unwrap_or(&self.composer)
            .trim();
        let matches = SLASH_COMMANDS
            .iter()
            .filter(|command| command.name.starts_with(prefix))
            .collect::<Vec<_>>();
        if matches.is_empty() {
            SLASH_COMMANDS.iter().collect()
        } else {
            matches
        }
    }

    fn accept_slash_selection(&mut self) {
        let matches = self.slash_matches();
        if let Some(command) = matches.get(self.slash_selected.min(matches.len().saturating_sub(1)))
        {
            self.composer = format!("{} ", command.name);
        }
        self.slash_selected = 0;
    }

    fn move_selection(&mut self, delta: i32) {
        if self.sessions.is_empty() {
            return;
        }
        let len = self.sessions.len() as i32;
        let current = self.selected as i32;
        self.selected = (current + delta).rem_euclid(len) as usize;
    }

    /// Move the highlight and immediately open that conversation — a one-key
    /// switch (Ctrl+N / Ctrl+P), like switching buffers in a chat client.
    fn switch_selection(&mut self, delta: i32) {
        self.move_selection(delta);
        self.open_selected();
    }

    fn open_selected(&mut self) {
        if let Some(contact) = self.selected_contact().map(ToOwned::to_owned) {
            self.open_chat(&contact);
        }
    }

    fn open_latest_media(&mut self) {
        let Some(path) = self
            .lines
            .iter()
            .rev()
            .flat_map(|line| line.media.iter().rev())
            .map(|media| media.path.clone())
            .next()
            .or_else(|| {
                self.lines
                    .iter()
                    .rev()
                    .flat_map(|line| line.attachments.iter().rev())
                    .next()
                    .cloned()
            })
        else {
            self.status = "No media in this conversation.".to_string();
            return;
        };
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        match std::process::Command::new(opener)
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => self.status = format!("Opened {path}"),
            Err(err) => self.status = format!("Open failed ({opener}): {err}"),
        }
    }
}

fn run_chat(
    guard: &mut TerminalGuard,
    cache: MailCache,
    config: MailConfig,
    mailbox: String,
) -> Result<()> {
    let core = MailCore::new(config);
    let client = ChatClient::new(core, cache)?;
    let account = client.account_address().to_string();

    // The Engine owns the worker + IDLE-watcher threads and the command/event
    // channels; the frontend only sends commands and drains events.
    let engine = Engine::start(client.clone(), mailbox);
    let (thumb_tx, thumb_rx) = channel::<(String, Vec<Line<'static>>)>();

    let mut app = App {
        account,
        sessions: Vec::new(),
        selected: 0,
        active: None,
        lines: Vec::new(),
        composer: String::new(),
        slash_selected: 0,
        chat_scroll: 0,
        focus: Focus::Contacts,
        status: "Welcome to Slip. /open <email> starts a conversation.".to_string(),
        watcher_status: "connecting…".to_string(),
        quit: false,
        overlay: None,
        thumbnails: HashMap::new(),
        thumb_tx,
        thumb_rx,
        images_enabled: crate::imgview::thumbnails_enabled(),
        local: client,
        engine,
    };
    app.refresh_sessions();

    ui_loop(guard, &mut app)
}

fn ui_loop(guard: &mut TerminalGuard, app: &mut App) -> Result<()> {
    loop {
        // Collect finished thumbnails and dispatch decoding for new images,
        // both off the UI thread.
        app.drain_thumbnails();
        app.build_thumbnails();

        // Drain events the engine's threads have produced.
        while let Some(event) = app.engine.try_next() {
            match event {
                EngineEvent::Sessions(sessions) => {
                    let previous = app.selected_contact().map(ToOwned::to_owned);
                    app.sessions = sessions;
                    if let Some(previous) = previous
                        && let Some(position) = app
                            .sessions
                            .iter()
                            .position(|summary| summary.contact == previous)
                    {
                        app.selected = position;
                    }
                    app.selected = app.selected.min(app.sessions.len().saturating_sub(1));
                }
                EngineEvent::SessionUpdated(contact) => {
                    if app.active.as_deref() == Some(contact.as_str()) {
                        app.reload_active(false);
                        app.engine.send(Command::MarkRead(contact));
                    }
                }
                EngineEvent::Notices(notices) => {
                    notify(&notices);
                    if let Some(first) = notices.first() {
                        app.status = format!(
                            "New message · {}{}",
                            first.preview,
                            if notices.len() > 1 {
                                format!(" (+{} more)", notices.len() - 1)
                            } else {
                                String::new()
                            }
                        );
                    }
                }
                EngineEvent::KeyChanges(contacts) => {
                    app.status = format!(
                        "⚠ identity key changed for {} — verify fingerprints (/info) then /trust",
                        contacts.join(", ")
                    );
                }
                EngineEvent::Status(status) => app.status = status,
                EngineEvent::WatcherStatus(status) => app.watcher_status = status,
            }
        }

        guard.terminal.draw(|frame| draw(frame, app))?;

        if app.quit {
            return Ok(());
        }

        if !event::poll(Duration::from_millis(120))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // A modal overlay (/info, /help) swallows the next key press to close.
        if app.overlay.is_some() {
            app.overlay = None;
            continue;
        }

        match key.code {
            KeyCode::Esc => return Ok(()),
            KeyCode::Tab if app.slash_menu_active() => app.accept_slash_selection(),
            KeyCode::Tab => {
                app.focus = match app.focus {
                    Focus::Contacts => Focus::Composer,
                    Focus::Composer => Focus::Contacts,
                };
            }
            KeyCode::Enter if app.slash_menu_active() && !slash_ready(&app.composer) => {
                app.accept_slash_selection();
            }
            // Enter opens the highlighted contact whenever the composer is
            // empty — even if another conversation is already open — so
            // browsing the list with the arrows and pressing Enter switches.
            KeyCode::Enter if app.focus == Focus::Contacts || app.composer.is_empty() => {
                app.open_selected()
            }
            KeyCode::Enter => app.submit_composer(),
            KeyCode::Up if app.slash_menu_active() => {
                let len = app.slash_matches().len().max(1);
                app.slash_selected = (app.slash_selected + len - 1) % len;
            }
            KeyCode::Down if app.slash_menu_active() => {
                let len = app.slash_matches().len().max(1);
                app.slash_selected = (app.slash_selected + 1) % len;
            }
            KeyCode::Up if app.focus == Focus::Contacts || app.composer.is_empty() => {
                app.move_selection(-1)
            }
            KeyCode::Down if app.focus == Focus::Contacts || app.composer.is_empty() => {
                app.move_selection(1)
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.switch_selection(1)
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.switch_selection(-1)
            }
            KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.open_latest_media()
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.composer.clear();
                app.slash_selected = 0;
            }
            KeyCode::PageUp => app.chat_scroll = app.chat_scroll.saturating_add(8),
            KeyCode::PageDown => app.chat_scroll = app.chat_scroll.saturating_sub(8),
            KeyCode::Backspace => {
                app.composer.pop();
                app.slash_selected = 0;
            }
            KeyCode::Char(ch) => {
                app.focus = Focus::Composer;
                app.composer.push(ch);
                app.slash_selected = 0;
            }
            _ => {}
        }
    }
}

/// Bell + OSC 9 desktop notification: arrival must be loud and immediate.
fn notify(notices: &[NewMessageNotice]) {
    let mut stdout = io::stdout();
    let _ = stdout.write_all(b"\x07");
    if let Some(first) = notices.first() {
        let text = format!(
            "Slip: {}{}",
            first.preview,
            if notices.len() > 1 {
                format!(" (+{})", notices.len() - 1)
            } else {
                String::new()
            }
        );
        let sanitized: String = text
            .chars()
            .filter(|ch| !ch.is_control())
            .take(120)
            .collect();
        let _ = stdout.write_all(format!("\x1b]9;{sanitized}\x07").as_bytes());
    }
    let _ = stdout.flush();
}

// ----------------------------------------------------------------------
// Drawing
// ----------------------------------------------------------------------

fn draw(frame: &mut Frame<'_>, app: &App) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),
            Constraint::Length(composer_height(app)),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Min(30)])
        .split(vertical[0]);

    draw_contacts(frame, horizontal[0], app);
    draw_chat(frame, horizontal[1], app);
    draw_composer(frame, vertical[1], app);
    draw_status(frame, vertical[2], app);

    if let Some(text) = &app.overlay {
        draw_overlay(frame, vertical[0], text);
    }
}

/// A centered modal panel over the chat area (used by /info and /help).
fn draw_overlay(frame: &mut Frame<'_>, area: Rect, text: &str) {
    let lines: Vec<Line<'static>> = text
        .lines()
        .map(|line| {
            let style = if line.starts_with('⚠') {
                Style::default()
                    .fg(Color::Rgb(200, 80, 30))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Rgb(30, 50, 56))
            };
            Line::from(Span::styled(line.to_string(), style))
        })
        .collect();

    let width = area.width.saturating_sub(6).clamp(20, 70);
    let height = ((lines.len() as u16) + 2).min(area.height.saturating_sub(2));
    let panel = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(ratatui::widgets::Clear, panel);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Rgb(70, 95, 255))),
            )
            .wrap(Wrap { trim: false }),
        panel,
    );
}

fn composer_height(app: &App) -> u16 {
    if app.slash_menu_active() {
        (app.slash_matches().len() as u16 + 5).min(14)
    } else {
        4
    }
}

fn draw_contacts(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let dim = Style::default().fg(Color::Rgb(105, 112, 116));
    let strong = Style::default()
        .fg(Color::Rgb(30, 50, 56))
        .add_modifier(Modifier::BOLD);
    let accent = Style::default()
        .fg(Color::Rgb(70, 95, 255))
        .add_modifier(Modifier::BOLD);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Slip  ", strong),
            Span::styled(app.account.clone(), dim),
        ]),
        Line::from(""),
    ];

    if app.sessions.is_empty() {
        lines.push(Line::from(Span::styled("No conversations yet.", dim)));
        lines.push(Line::from(Span::styled("/add <email> to start.", dim)));
    }

    let max_rows = (area.height as usize).saturating_sub(4) / 2;
    let start = if app.selected >= max_rows.max(1) {
        app.selected + 1 - max_rows.max(1)
    } else {
        0
    };
    for (index, summary) in app
        .sessions
        .iter()
        .enumerate()
        .skip(start)
        .take(max_rows.max(1))
    {
        let selected = index == app.selected;
        let open = app.active.as_deref() == Some(summary.contact.as_str());
        let marker = if selected { "› " } else { "  " };
        let unread = if summary.unread > 0 {
            format!(" ({})", summary.unread)
        } else {
            String::new()
        };
        let name_style = if selected {
            accent
        } else if open {
            strong
        } else {
            Style::default().fg(Color::Rgb(70, 82, 88))
        };
        // Show the alias (if any) as the primary name; the raw email becomes
        // the subtitle so it stays discoverable.
        let display = summary.alias.as_deref().unwrap_or(&summary.contact);
        let mut spans = vec![
            Span::styled(marker, if selected { accent } else { dim }),
            Span::styled(fit(display, 22), name_style),
        ];
        if !unread.is_empty() {
            spans.push(Span::styled(
                unread,
                Style::default()
                    .fg(Color::Rgb(200, 80, 30))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        lines.push(Line::from(spans));
        let subtitle = if summary.alias.is_some() {
            fit(&summary.contact, 24)
        } else {
            fit(&summary.preview, 24)
        };
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(subtitle, dim),
        ]));
    }

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(Style::default().fg(Color::Rgb(180, 185, 188))),
        ),
        area,
    );
}

fn draw_chat(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let dim = Style::default().fg(Color::Rgb(105, 112, 116));
    let strong = Style::default()
        .fg(Color::Rgb(30, 50, 56))
        .add_modifier(Modifier::BOLD);

    let mut lines = Vec::new();
    match &app.active {
        None => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  ↑/↓ to pick a contact, Enter to open — or /open <email>.",
                dim,
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Messages travel as ordinary email — Slip turns them into chat.",
                dim,
            )));
        }
        Some(contact) => {
            let encryption = app
                .local
                .contact_info(contact)
                .ok()
                .map(|info| {
                    if info.pending_fingerprint.is_some() {
                        "⚠ key changed — /info"
                    } else if info.encryption_active {
                        "🔒 encrypted"
                    } else {
                        "plaintext (peer key not seen yet)"
                    }
                })
                .unwrap_or("");
            let alias = app
                .sessions
                .iter()
                .find(|summary| &summary.contact == contact)
                .and_then(|summary| summary.alias.clone());
            let mut header = vec![Span::styled(
                alias.clone().unwrap_or_else(|| contact.clone()),
                strong,
            )];
            if let Some(_alias) = &alias {
                header.push(Span::styled(format!("  <{contact}>"), dim));
            }
            header.push(Span::raw("   "));
            header.push(Span::styled(encryption, dim));
            lines.push(Line::from(header));
            lines.push(Line::from(""));

            if app.lines.is_empty() {
                lines.push(Line::from(Span::styled("No messages yet.", dim)));
            }

            let body_width = (area.width as usize).saturating_sub(6).max(20);
            let mut rendered: Vec<Line<'static>> = Vec::new();
            for line in &app.lines {
                rendered.extend(render_message(line, body_width, &app.thumbnails));
            }

            // chat_scroll counts lines from the bottom (0 = follow tail).
            let visible = (area.height as usize).saturating_sub(3);
            let total = rendered.len();
            let end = total.saturating_sub(app.chat_scroll);
            let start = end.saturating_sub(visible);
            lines.extend(rendered.into_iter().take(end).skip(start));
        }
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_message(
    line: &StoredChatLine,
    width: usize,
    thumbnails: &HashMap<String, Vec<Line<'static>>>,
) -> Vec<Line<'static>> {
    let mine = line.sender == "me";
    let name_style = if mine {
        Style::default()
            .fg(Color::Rgb(145, 92, 0))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Rgb(0, 94, 110))
            .add_modifier(Modifier::BOLD)
    };
    let dim = Style::default().fg(Color::DarkGray);
    let body_style = Style::default().fg(Color::Rgb(30, 50, 56));

    let mut header = vec![
        Span::styled(format!("{} ", line.sender), name_style),
        Span::styled(short_time(line), dim),
    ];
    if line.encrypted {
        header.push(Span::styled(" 🔒", dim));
    }
    if mine {
        let (glyph, style) = match line.status {
            DeliveryStatus::Sending => (" ⏳", dim),
            DeliveryStatus::Sent => (" ✓", Style::default().fg(Color::Rgb(0, 140, 90))),
            DeliveryStatus::Failed => (
                " ✗ failed",
                Style::default()
                    .fg(Color::Rgb(190, 40, 40))
                    .add_modifier(Modifier::BOLD),
            ),
        };
        header.push(Span::styled(glyph, style));
    }

    let mut out = vec![Line::from(header)];

    for flag in &line.flags {
        if flag == "key-changed" {
            out.push(Line::from(Span::styled(
                "  ⚠ sent under a changed identity key",
                Style::default().fg(Color::Rgb(190, 40, 40)),
            )));
        } else if let Some(reason) = flag.strip_prefix("unreadable:") {
            out.push(Line::from(Span::styled(
                format!("  ⚠ {}", fit(reason, width)),
                Style::default().fg(Color::Rgb(190, 40, 40)),
            )));
        } else if let Some(name) = flag.strip_prefix("hash-mismatch:") {
            out.push(Line::from(Span::styled(
                format!("  ⚠ integrity check failed: {name}"),
                Style::default().fg(Color::Rgb(190, 40, 40)),
            )));
        }
    }

    if !line.body.trim().is_empty() {
        for wrapped in wrap_text(line.body.trim(), width) {
            out.push(Line::from(Span::styled(format!("  {wrapped}"), body_style)));
        }
    }

    for media in &line.media {
        let icon = match media.kind {
            crate::protocol::MediaKind::Image => "🖼",
            crate::protocol::MediaKind::Audio => "🎵",
            crate::protocol::MediaKind::File => "📄",
        };
        out.push(Line::from(Span::styled(
            format!(
                "  {icon} {} {} ({}) · Ctrl+O opens",
                media.kind.label(),
                media.name,
                human_size(media.size)
            ),
            Style::default().fg(Color::Rgb(85, 95, 100)),
        )));
        // Inline half-block thumbnail for images, indented under the label.
        if let Some(thumb) = thumbnails.get(&media.path) {
            // Crop each half-block row to the pane width (2-space indent + at
            // most width-2 cells). Cropping shows a partial image on a narrow
            // pane, which is far better than wrapping every row into a garbled
            // staircase.
            let budget = width.saturating_sub(2);
            for row in thumb {
                let mut spans = Vec::with_capacity(budget.min(row.spans.len()) + 1);
                spans.push(Span::raw("  "));
                spans.extend(row.spans.iter().take(budget).cloned());
                out.push(Line::from(spans));
            }
        }
    }
    // Legacy lines: attachments without structured media entries.
    if line.media.is_empty() {
        for attachment in &line.attachments {
            out.push(Line::from(Span::styled(
                format!("  📄 {attachment}"),
                Style::default().fg(Color::Rgb(85, 95, 100)),
            )));
        }
    }

    out.push(Line::from(""));
    out
}

fn draw_composer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let dim = Style::default().fg(Color::Rgb(95, 95, 95));
    let target = app.active.as_deref().unwrap_or("no conversation");
    let input_style = if app.composer.is_empty() {
        Style::default().fg(Color::Rgb(30, 50, 56))
    } else {
        Style::default()
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    };

    let mut text = vec![
        Line::from(vec![
            Span::styled("› ", Style::default().fg(Color::Rgb(0, 94, 110))),
            Span::styled(app.composer.clone(), input_style),
            Span::styled("█", Style::default().fg(Color::Rgb(0, 150, 140))),
        ]),
        Line::from(vec![
            Span::styled(format!("to: {target}"), dim),
            Span::styled(
                "  ·  Ctrl+P/N switch chat  ·  @path attaches  ·  /help",
                dim,
            ),
        ]),
    ];

    if app.slash_menu_active() {
        text.push(Line::from(""));
        let matches = app.slash_matches();
        let selected = app.slash_selected.min(matches.len().saturating_sub(1));
        for (index, command) in matches.iter().enumerate() {
            let active = index == selected;
            let style = if active {
                Style::default()
                    .fg(Color::Rgb(70, 95, 255))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Rgb(70, 70, 70))
            };
            text.push(Line::from(vec![
                Span::styled(if active { "› " } else { "  " }, style),
                Span::styled(format!("{:<10}", command.name), style),
                Span::styled(command.description, dim),
            ]));
        }
    }

    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(Color::Rgb(110, 112, 112))),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let status = if app.quit { "bye" } else { app.status.as_str() };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("● {} ", app.watcher_status),
                Style::default().fg(Color::Rgb(0, 140, 90)),
            ),
            Span::styled(status.to_string(), Style::default().fg(Color::Gray)),
        ])),
        area,
    );
}

// ----------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

fn slash_ready(input: &str) -> bool {
    let trimmed = input.trim();
    let Some(first) = trimmed.split_whitespace().next() else {
        return false;
    };
    if !SLASH_COMMANDS.iter().any(|command| command.name == first) {
        return false;
    }
    trimmed == first || input.contains(char::is_whitespace)
}

fn short_time(line: &StoredChatLine) -> String {
    let ts = stored_chat_timestamp(line);
    if ts <= 0 {
        return line.date.clone();
    }
    let full = format_chat_timestamp(ts);
    let today = format_chat_timestamp(crate::chat::now_epoch_seconds_i64());
    if full.get(..10) == today.get(..10) {
        full.get(11..16).map(ToOwned::to_owned).unwrap_or(full)
    } else {
        full.get(..16).map(ToOwned::to_owned).unwrap_or(full)
    }
}

pub(crate) fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn fit(value: &str, max: usize) -> String {
    let mut out: String = value.chars().take(max).collect();
    if value.chars().count() > max {
        out.push('…');
    }
    out
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for raw_line in text.lines() {
        let mut current = String::new();
        let mut current_width = 0usize;
        for word in raw_line.split_whitespace() {
            let word_width = word.chars().count();
            if current_width > 0 && current_width + 1 + word_width > width {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            if word_width >= width {
                // Hard-break very long tokens.
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                }
                for chunk in word
                    .chars()
                    .collect::<Vec<_>>()
                    .chunks(width.max(1))
                    .map(|chunk| chunk.iter().collect::<String>())
                {
                    if !current.is_empty() {
                        lines.push(std::mem::take(&mut current));
                    }
                    current = chunk;
                }
                current_width = current.chars().count();
                continue;
            }
            if current_width > 0 {
                current.push(' ');
                current_width += 1;
            }
            current.push_str(word);
            current_width += word_width;
        }
        if !current.is_empty() || raw_line.trim().is_empty() {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}
