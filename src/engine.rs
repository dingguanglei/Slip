//! Frontend-agnostic chat engine.
//!
//! `Engine` owns the background threads (a worker for commands and an
//! IMAP-IDLE watcher for arrivals) and speaks to a frontend through two
//! channels: the frontend pushes [`Command`]s and drains [`Event`]s. The TUI,
//! a future GUI, or a headless driver all reuse this same engine — none of
//! them touch the network or the store directly.

use crate::chat::{ChatClient, ChatSessionSummary, NewMessageNotice};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, Sender, channel},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const POLL_FALLBACK: Duration = Duration::from_secs(15);
const SYNC_LIMIT: usize = 200;
/// How often the worker wakes to run the automatic resend queue.
const RETRY_TICK: Duration = Duration::from_secs(20);

/// A request from the frontend to the engine.
#[derive(Clone, Debug)]
pub enum Command {
    /// Send a message (optionally with attachment paths) to a contact.
    Send {
        to: String,
        body: String,
        attachments: Vec<std::path::PathBuf>,
    },
    /// Run an incremental sync now.
    Sync,
    /// Add a contact to the address book.
    AddContact(String),
    /// Accept a contact's changed identity key.
    Trust(String),
    /// Enable/disable encryption toward a contact.
    Encrypt(String, bool),
    /// Re-send the last failed message to a contact.
    Retry(String),
    /// Mark a conversation read up to its newest message.
    MarkRead(String),
}

/// A notification from the engine to the frontend.
#[derive(Clone, Debug)]
pub enum Event {
    /// The full, re-sorted session list (sidebar data).
    Sessions(Vec<ChatSessionSummary>),
    /// A specific conversation changed on disk and should be reloaded.
    SessionUpdated(String),
    /// New inbound messages worth alerting the user about.
    Notices(Vec<NewMessageNotice>),
    /// Contacts whose identity key changed (need out-of-band verification).
    KeyChanges(Vec<String>),
    /// A transient status line for the frontend to show.
    Status(String),
    /// The watcher connection state ("listening (IMAP IDLE)", …).
    WatcherStatus(String),
}

/// Whether to delete Slip mail from the server after saving it locally.
/// Enabled by default; Web/Desktop enforce cleanup independently of this setting.
pub fn burn_remote() -> bool {
    std::env::var("SLIP_BURN")
        .map(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(true)
}

/// Owns the worker and watcher threads and the command/event channels.
pub struct Engine {
    commands: Sender<Command>,
    events: Receiver<Event>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    watcher: Option<JoinHandle<()>>,
}

impl Engine {
    /// Start the background threads for `client`, watching `mailbox` (the
    /// arrival inbox). Returns immediately; work happens on the threads.
    pub fn start(client: ChatClient, mailbox: String) -> Self {
        Self::start_with_cleanup(client, mailbox, burn_remote())
    }

    /// Explicit policy for frontends such as Web, where cleanup is required.
    pub fn start_with_cleanup(client: ChatClient, mailbox: String, burn: bool) -> Self {
        let (command_tx, command_rx) = channel::<Command>();
        let (event_tx, event_rx) = channel::<Event>();
        let stop = Arc::new(AtomicBool::new(false));

        let worker = spawn_worker(
            client.clone(),
            mailbox.clone(),
            command_rx,
            event_tx.clone(),
            burn,
        );
        let watcher = spawn_watcher(client, mailbox, stop.clone(), event_tx, burn);

        Self {
            commands: command_tx,
            events: event_rx,
            stop,
            worker: Some(worker),
            watcher: Some(watcher),
        }
    }

    /// Queue a command for the worker. Fails only if the engine is shutting
    /// down (the worker thread is gone).
    pub fn send(&self, command: Command) -> bool {
        self.commands.send(command).is_ok()
    }

    /// Take the next pending event without blocking, if any.
    pub fn try_next(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    /// Block up to `timeout` for the next event (useful for a GUI/event loop
    /// that would rather sleep than spin).
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Event> {
        self.events.recv_timeout(timeout).ok()
    }

    /// Signal the threads to stop. Called automatically on drop.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown();
        // The worker exits when its command sender is dropped; the watcher
        // exits on the next stop-flag check. Detach rather than block the
        // frontend's teardown on a network read.
        drop(self.worker.take());
        drop(self.watcher.take());
    }
}

fn spawn_worker(
    client: ChatClient,
    mailbox: String,
    commands: Receiver<Command>,
    events: Sender<Event>,
    burn: bool,
) -> JoinHandle<()> {
    thread::spawn(move || {
        // First catch-up sync so the frontend has data before IDLE reports.
        run_sync(&client, &mailbox, &events, burn);
        // Block for commands, but wake periodically to run the resend queue.
        loop {
            let command = match commands.recv_timeout(RETRY_TICK) {
                Ok(command) => command,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    run_retry(&client, &events);
                    // Sent-folder copies and failed cleanup may not wake INBOX IDLE.
                    if burn {
                        run_sync(&client, &mailbox, &events, burn);
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            match command {
                Command::Send {
                    to,
                    body,
                    attachments,
                } => {
                    match client.send_parts(&to, &body, &attachments) {
                        Ok(result) => {
                            let mut note = format!(
                                "Sent to {to}{}",
                                if result.encrypted { " 🔒" } else { "" }
                            );
                            for warning in &result.warnings {
                                note.push_str(&format!(" · {warning}"));
                            }
                            let _ = events.send(Event::Status(note));
                        }
                        Err(err) => {
                            let _ = events.send(Event::Status(format!(
                                "Send failed: {err:#} · /retry to resend"
                            )));
                        }
                    }
                    let _ = events.send(Event::SessionUpdated(to));
                    send_sessions(&client, &events);
                }
                Command::Sync => run_sync(&client, &mailbox, &events, burn),
                Command::AddContact(address) => {
                    let _ = client.add_contact(&address);
                    send_sessions(&client, &events);
                }
                Command::Trust(contact) => {
                    match client.trust_peer(&contact) {
                        Ok(info) => {
                            let _ = events.send(Event::Status(format!(
                                "Trusted new key for {contact}: {}",
                                info.peer_fingerprint.unwrap_or_default()
                            )));
                        }
                        Err(err) => {
                            let _ = events.send(Event::Status(format!("trust failed: {err:#}")));
                        }
                    }
                    let _ = events.send(Event::SessionUpdated(contact));
                }
                Command::Encrypt(contact, enabled) => match client.set_encrypt(&contact, enabled) {
                    Ok(info) => {
                        let _ = events.send(Event::Status(format!(
                            "Encryption to {contact}: {}",
                            if info.encryption_active { "ON" } else { "off" }
                        )));
                    }
                    Err(err) => {
                        let _ = events.send(Event::Status(format!("encrypt: {err:#}")));
                    }
                },
                Command::Retry(contact) => {
                    match client.retry_last_failed(&contact) {
                        Ok(_) => {
                            let _ = events.send(Event::Status(format!("Resent to {contact}.")));
                        }
                        Err(err) => {
                            let _ = events.send(Event::Status(format!("retry: {err:#}")));
                        }
                    }
                    let _ = events.send(Event::SessionUpdated(contact));
                    send_sessions(&client, &events);
                }
                Command::MarkRead(contact) => {
                    let _ = client.mark_read(&contact);
                    send_sessions(&client, &events);
                }
            }
        }
    })
}

/// Run one pass of the automatic resend queue and report progress.
fn run_retry(client: &ChatClient, events: &Sender<Event>) {
    match client.retry_pending() {
        Ok(changed) if !changed.is_empty() => {
            let _ = events.send(Event::Status(format!(
                "Resent {} pending message(s)",
                changed.len()
            )));
            for contact in changed {
                let _ = events.send(Event::SessionUpdated(contact));
            }
            send_sessions(client, events);
        }
        Ok(_) => {}
        Err(err) => {
            let _ = events.send(Event::Status(format!("retry queue: {err:#}")));
        }
    }
}

fn spawn_watcher(
    client: ChatClient,
    mailbox: String,
    stop: Arc<AtomicBool>,
    events: Sender<Event>,
    burn: bool,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let sync_events = events.clone();
        let sessions_client = client.clone();
        client.watch(
            &mailbox,
            burn,
            POLL_FALLBACK,
            &stop,
            move |result| {
                if !result.new_messages.is_empty() {
                    let _ = sync_events.send(Event::Notices(result.new_messages.clone()));
                }
                if !result.key_changes.is_empty() {
                    let _ = sync_events.send(Event::KeyChanges(result.key_changes.clone()));
                }
                for notice in &result.new_messages {
                    let _ = sync_events.send(Event::SessionUpdated(notice.contact.clone()));
                }
                let _ = sync_events.send(Event::Status(sync_status(result)));
                send_sessions(&sessions_client, &sync_events);
            },
            move |status| {
                crate::logging::info("watch", "status", &[("state", &status)]);
                let _ = events.send(Event::WatcherStatus(status.to_string()));
            },
        );
    })
}

fn run_sync(client: &ChatClient, mailbox: &str, events: &Sender<Event>, burn: bool) {
    match client.sync(mailbox, SYNC_LIMIT, burn) {
        Ok(result) => {
            let _ = events.send(Event::Status(sync_status(&result)));
            if !result.new_messages.is_empty() {
                let _ = events.send(Event::Notices(result.new_messages.clone()));
            }
            if !result.key_changes.is_empty() {
                let _ = events.send(Event::KeyChanges(result.key_changes.clone()));
            }
            for notice in &result.new_messages {
                let _ = events.send(Event::SessionUpdated(notice.contact.clone()));
            }
            send_sessions(client, events);
        }
        Err(err) => {
            let _ = events.send(Event::Status(format!("Sync failed: {err:#}")));
        }
    }
}

fn send_sessions(client: &ChatClient, events: &Sender<Event>) {
    if let Ok(sessions) = client.sessions() {
        let _ = events.send(Event::Sessions(sessions));
    }
}

fn sync_status(result: &crate::chat::ChatSyncResult) -> String {
    format!(
        "已保存 {} 条 · 已清理 {} 封 · 验证保留 {} 封 · 清理待重试 {} 封",
        result.saved, result.burned, result.retained, result.cleanup_pending
    )
}
