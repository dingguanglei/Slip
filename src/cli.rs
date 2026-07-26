//! Scriptable CLI: chat operations with JSON output, plus low-level mailbox
//! debug commands. Credentials come from the environment, never from argv.

use crate::cache::MailCache;
use crate::chat::ChatClient;
use crate::core::{MailConfig, MailCore, MutationRequest};
use crate::llm::LlmConfig;
use crate::providers::Security;
use crate::tui;
use anyhow::Result;
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "slip")]
#[command(about = "Chat over email: any IMAP/SMTP mailbox becomes a messenger.")]
struct Cli {
    /// Override the IMAP host (otherwise from provider preset / env).
    #[arg(long)]
    imap_host: Option<String>,

    #[arg(long)]
    imap_port: Option<u16>,

    /// ssl | starttls | plain
    #[arg(long)]
    imap_security: Option<Security>,

    #[arg(long)]
    smtp_host: Option<String>,

    #[arg(long)]
    smtp_port: Option<u16>,

    #[arg(long)]
    smtp_security: Option<Security>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Open the interactive terminal UI.
    Tui {
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
    },

    /// Add a chat contact to the local address book.
    ChatAdd {
        #[arg(long)]
        email: String,
    },

    /// Set a local display name for a contact (empty --name clears it).
    ChatName {
        #[arg(long)]
        email: String,

        #[arg(long, default_value = "")]
        name: String,
    },

    /// Send a Slip chat message. Use @path tokens to attach files.
    ChatSend {
        #[arg(long)]
        to: String,

        #[arg(long)]
        body: String,
    },

    /// Sync new Slip chat mail into local sessions (cursor-based).
    ///
    /// Chat mail is moved out of the inbox into the dedicated Slip folder and
    /// archived there. Pass --burn to instead delete it after local save.
    ChatSync {
        #[arg(long, default_value = "INBOX")]
        mailbox: String,

        #[arg(long, default_value_t = 200)]
        limit: usize,

        /// Delete synced chat mail from the server after saving it locally.
        #[arg(long)]
        burn: bool,
    },

    /// Block on IMAP IDLE and print a JSON line the moment chat mail arrives.
    ChatWatch {
        #[arg(long, default_value = "INBOX")]
        mailbox: String,

        /// Delete synced chat mail from the server after saving it locally.
        #[arg(long)]
        burn: bool,

        /// Poll interval (seconds) when the server does not support IDLE.
        #[arg(long, default_value_t = 15)]
        poll_secs: u64,

        /// Exit after this many sync events (0 = run until killed).
        #[arg(long, default_value_t = 0)]
        max_events: usize,
    },

    /// List local chat sessions.
    ChatSessions,

    /// Print a local chat session.
    ChatResume {
        #[arg(long)]
        email: String,
    },

    /// Show this installation's identity public key and fingerprint.
    Identity,

    /// List peer keys learned via TOFU.
    Peers,

    /// Show key/encryption details for one contact.
    ContactInfo {
        #[arg(long)]
        email: String,
    },

    /// Accept a contact's changed key after out-of-band verification.
    Trust {
        #[arg(long)]
        email: String,
    },

    /// Enable/disable encryption toward one contact.
    Encrypt {
        #[arg(long)]
        email: String,

        #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
        enabled: bool,
    },

    /// Re-send the most recent failed message in a conversation.
    Retry {
        #[arg(long)]
        email: String,
    },

    /// Resend every failed outgoing message whose backoff has elapsed.
    RetryAll,

    /// List mailboxes/folders.
    ListMailboxes,

    /// Show recent messages in a mailbox.
    Recent {
        #[arg(long, default_value = "INBOX")]
        mailbox: String,

        #[arg(long, default_value_t = 10)]
        limit: usize,

        #[arg(long)]
        body: bool,
    },

    /// Search messages with IMAP search syntax, e.g. SUBJECT "invoice".
    Search {
        #[arg(long, default_value = "INBOX")]
        mailbox: String,

        #[arg(long)]
        query: String,

        #[arg(long, default_value_t = 10)]
        limit: usize,

        #[arg(long)]
        body: bool,
    },

    /// Fetch one message by UID.
    Fetch {
        #[arg(long, default_value = "INBOX")]
        mailbox: String,

        #[arg(long)]
        uid: u32,

        #[arg(long)]
        no_body: bool,

        #[arg(long)]
        save_attachments: Option<PathBuf>,

        #[arg(long, default_value_t = true)]
        cache: bool,
    },

    /// Search locally cached message bodies.
    CacheSearch {
        #[arg(long)]
        query: String,

        #[arg(long)]
        mailbox: Option<String>,

        #[arg(long, default_value_t = 20)]
        limit: usize,
    },

    /// Analyze one message with a local GGUF model through llama.cpp.
    Analyze {
        #[arg(long, default_value = "INBOX")]
        mailbox: String,

        #[arg(long)]
        uid: u32,

        #[arg(
            long,
            default_value = "总结邮件重点、待办事项、风险等级，并给出适合搜索的关键词。"
        )]
        prompt: String,

        #[arg(long)]
        model: Option<PathBuf>,

        #[arg(long)]
        llm_bin: Option<PathBuf>,

        #[arg(long, default_value_t = 512)]
        max_tokens: usize,

        #[arg(long, default_value_t = 0.2)]
        temperature: f32,
    },

    /// Mark one message as read. Dry-run unless --execute is provided.
    MarkRead(MutationUidArgs),

    /// Mark one message as unread. Dry-run unless --execute is provided.
    MarkUnread(MutationUidArgs),

    /// Move one message to another mailbox. Dry-run unless --execute is provided.
    Move {
        #[arg(long, default_value = "INBOX")]
        mailbox: String,

        #[arg(long)]
        uid: u32,

        #[arg(long)]
        to: String,

        #[arg(long)]
        execute: bool,
    },

    /// Delete one message and expunge the mailbox. Dry-run unless --execute is provided.
    Delete(MutationUidArgs),

    /// Create a mailbox/folder. Dry-run unless --execute is provided.
    CreateMailbox {
        #[arg(long)]
        name: String,

        #[arg(long)]
        execute: bool,
    },
}

#[derive(Parser, Debug)]
struct MutationUidArgs {
    #[arg(long, default_value = "INBOX")]
    mailbox: String,

    #[arg(long)]
    uid: u32,

    #[arg(long)]
    execute: bool,
}

impl From<MutationUidArgs> for MutationRequest {
    fn from(args: MutationUidArgs) -> Self {
        Self {
            mailbox: args.mailbox,
            uid: args.uid,
            execute: args.execute,
        }
    }
}

pub fn run() -> Result<()> {
    crate::logging::init();
    let cli = Cli::parse();

    if let Command::Tui { mailbox } = cli.command {
        return tui::run(mailbox);
    }

    let mut config = MailConfig::from_env()?;
    apply_overrides(&mut config, &cli);
    let core = MailCore::new(config);

    match cli.command {
        Command::Tui { .. } => unreachable!("TUI command is handled before env login"),
        Command::ChatAdd { email } => {
            let client = chat_client(core)?;
            print_json(&serde_json::json!({
                "ok": true,
                "contacts": client.add_contact(&email)?
            }))
        }
        Command::ChatName { email, name } => {
            let client = chat_client(core)?;
            print_json(&serde_json::json!({
                "ok": true,
                "email": email,
                "alias": client.set_alias(&email, &name)?
            }))
        }
        Command::ChatSend { to, body } => {
            let client = chat_client(core)?;
            print_json(&serde_json::json!({
                "ok": true,
                "sent": client.send(&to, &body)?
            }))
        }
        Command::ChatSync {
            mailbox,
            limit,
            burn,
        } => {
            let client = chat_client(core)?;
            print_json(&serde_json::json!({
                "ok": true,
                "sync": client.sync(&mailbox, limit, burn)?
            }))
        }
        Command::ChatWatch {
            mailbox,
            burn,
            poll_secs,
            max_events,
        } => {
            let client = chat_client(core)?;
            let stop = AtomicBool::new(false);
            let mut events = 0usize;
            client.watch(
                &mailbox,
                burn,
                Duration::from_secs(poll_secs.max(1)),
                &stop,
                |sync| {
                    let _ = print_json(&serde_json::json!({"event": "sync", "sync": sync}));
                    events += 1;
                    if max_events > 0 && events >= max_events {
                        stop.store(true, Ordering::Relaxed);
                    }
                },
                |status| {
                    let _ = print_json(&serde_json::json!({"event": "status", "status": status}));
                },
            );
            Ok(())
        }
        Command::ChatSessions => {
            let client = chat_client(core)?;
            print_json(&serde_json::json!({
                "ok": true,
                "sessions": client.sessions()?
            }))
        }
        Command::ChatResume { email } => {
            let client = chat_client(core)?;
            print_json(&serde_json::json!({
                "ok": true,
                "session": client.session(&email)?
            }))
        }
        Command::Identity => {
            let client = chat_client(core)?;
            print_json(&serde_json::json!({
                "ok": true,
                "address": client.account_address(),
                "public_key": client.identity_public_key(),
                "fingerprint": client.identity_fingerprint(),
            }))
        }
        Command::Peers => {
            let client = chat_client(core)?;
            print_json(&serde_json::json!({
                "ok": true,
                "peers": client.peers()?
            }))
        }
        Command::ContactInfo { email } => {
            let client = chat_client(core)?;
            print_json(&serde_json::json!({
                "ok": true,
                "contact": client.contact_info(&email)?
            }))
        }
        Command::Trust { email } => {
            let client = chat_client(core)?;
            print_json(&serde_json::json!({
                "ok": true,
                "contact": client.trust_peer(&email)?
            }))
        }
        Command::Encrypt { email, enabled } => {
            let client = chat_client(core)?;
            print_json(&serde_json::json!({
                "ok": true,
                "contact": client.set_encrypt(&email, enabled)?
            }))
        }
        Command::Retry { email } => {
            let client = chat_client(core)?;
            print_json(&serde_json::json!({
                "ok": true,
                "sent": client.retry_last_failed(&email)?
            }))
        }
        Command::RetryAll => {
            let client = chat_client(core)?;
            print_json(&serde_json::json!({
                "ok": true,
                "resent_contacts": client.retry_pending()?
            }))
        }
        Command::ListMailboxes => print_json(&serde_json::json!({
            "ok": true,
            "mailboxes": core.list_mailboxes()?
        })),
        Command::Recent {
            mailbox,
            limit,
            body,
        } => print_json(&serde_json::json!({
            "ok": true,
            "mailbox": mailbox,
            "messages": core.recent(&mailbox, limit, body)?
        })),
        Command::Search {
            mailbox,
            query,
            limit,
            body,
        } => {
            let (uids, messages) = core.search(&mailbox, &query, limit, body)?;
            print_json(&serde_json::json!({
                "ok": true,
                "mailbox": mailbox,
                "query": query,
                "uids": uids,
                "messages": messages
            }))
        }
        Command::Fetch {
            mailbox,
            uid,
            no_body,
            save_attachments,
            cache,
        } => {
            let message = core.fetch_one(&mailbox, uid, !no_body, save_attachments.as_deref())?;
            let cache_dir = if cache && message.body.is_some() {
                let mail_cache = MailCache::default();
                mail_cache.save_message(&mailbox, &message)?;
                Some(mail_cache.root().display().to_string())
            } else {
                None
            };
            print_json(&serde_json::json!({
                "ok": true,
                "mailbox": mailbox,
                "cache_dir": cache_dir,
                "message": message
            }))
        }
        Command::CacheSearch {
            query,
            mailbox,
            limit,
        } => {
            let cache = MailCache::default();
            print_json(&serde_json::json!({
                "ok": true,
                "cache_dir": cache.root(),
                "query": query,
                "hits": cache.search(&query, mailbox.as_deref(), limit)?
            }))
        }
        Command::Analyze {
            mailbox,
            uid,
            prompt,
            model,
            llm_bin,
            max_tokens,
            temperature,
        } => {
            let cache = MailCache::default();
            let message = match cache.load_message(&mailbox, uid)? {
                Some(message)
                    if message
                        .body
                        .as_deref()
                        .is_some_and(|body| !body.trim().is_empty()) =>
                {
                    message
                }
                _ => {
                    let message = core.fetch_one(&mailbox, uid, true, None)?;
                    cache.save_message(&mailbox, &message)?;
                    message
                }
            };
            let llm = LlmConfig::from_options(model, llm_bin, max_tokens, temperature)?;
            print_json(&serde_json::json!({
                "ok": true,
                "mailbox": mailbox,
                "uid": uid,
                "cache_dir": cache.root(),
                "analysis": llm.analyze_message(&message, &prompt)?
            }))
        }
        Command::MarkRead(args) => print_json(&wrap_mutation(core.mark_read(args.into())?)),
        Command::MarkUnread(args) => print_json(&wrap_mutation(core.mark_unread(args.into())?)),
        Command::Move {
            mailbox,
            uid,
            to,
            execute,
        } => print_json(&wrap_mutation(
            core.move_message(&mailbox, uid, &to, execute)?,
        )),
        Command::Delete(args) => print_json(&wrap_mutation(core.delete_message(args.into())?)),
        Command::CreateMailbox { name, execute } => {
            print_json(&wrap_mutation(core.create_mailbox(&name, execute)?))
        }
    }
}

fn apply_overrides(config: &mut MailConfig, cli: &Cli) {
    if let Some(host) = &cli.imap_host {
        config.imap.host = host.clone();
    }
    if let Some(port) = cli.imap_port {
        config.imap.port = port;
    }
    if let Some(security) = cli.imap_security {
        config.imap.security = security;
    }
    if let Some(host) = &cli.smtp_host {
        config.smtp.host = host.clone();
    }
    if let Some(port) = cli.smtp_port {
        config.smtp.port = port;
    }
    if let Some(security) = cli.smtp_security {
        config.smtp.security = security;
    }
}

fn chat_client(core: MailCore) -> Result<ChatClient> {
    ChatClient::new(core, MailCache::default())
}

fn wrap_mutation(result: crate::core::MutationResult) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "action": result.action,
        "dry_run": result.dry_run,
        "mailbox": result.mailbox,
        "uid": result.uid,
        "to": result.to,
        "method": result.method
    })
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
