# Roadmap

Slip is an email-backed chat application. This roadmap separates shipped
behavior from the next production-grade work.

## Shipped

Transport and accounts:

- login with any IMAP/SMTP mailbox; presets for QQ, Gmail, Outlook,
  NetEase 163/126 (with the IMAP ID handshake), iCloud, Yahoo
- custom-server login step (host:port:security) for unknown domains
- ssl / starttls / plain security modes, pure-Rust TLS (rustls)
- `SLIP_*` environment credentials with legacy `QQ_*`/`GMAIL_*` fallback

Protocol (docs/PROTOCOL.md):

- versioned envelope with client-generated message ids
- multimedia manifest: image/audio/file kinds, MIME, size, SHA-256
- end-to-end encryption (`box-v1`: X25519 + XSalsa20-Poly1305)
- TOFU key exchange on every message, key-change alarms, /trust flow
- v0 legacy mail still readable; future versions degrade gracefully

Reliability:

- UID cursor per mailbox (UIDVALIDITY-aware) — incremental sync
- dedup by message id (content-triple fallback for legacy lines)
- delivery states sending/sent/failed with /retry under the same id
- burn-after-save that never deletes unread/unreadable mail

Product:

- IMAP IDLE push with reconnect/backoff and polling fallback
- immediate arrival notification: bell, OSC 9, unread badges, live update
- persistent contact pane with unread counts and previews
- media rendering with kind icons and Ctrl+O open
- login wizard, /info /trust /encrypt /retry commands
- chat-watch, identity, peers, contact-info CLI commands
- GreenMail end-to-end suite (scripts/e2e.sh) covering text, media,
  encryption, key rotation, push latency, and burn behavior

## Known limitations to address

- Unread counting compares the sender-claimed timestamp against the
  last-read mark, so a message delivered late or from a peer with a slow
  clock may not raise the unread badge. A per-message read flag (with the
  SQLite migration) fixes this properly.
- The login wizard runs the connect/login check on the UI thread, so a
  wrong or unreachable server can freeze the wizard until the socket
  timeout elapses. Move the check to the worker thread.

## Next: reliability

- retry queue with automatic backoff for failed sends (manual /retry today)
- SQLite store once the message schema stabilizes
- import/export backup command for `~/.slip`
- multi-account profiles
- delivery receipts (protocol-level acks)

## Next: product polish

- message search inside conversations
- inline image preview via terminal graphics protocols where available
- configurable notification behavior
- structured logs with secret redaction

## Next: security

- forward secrecy: per-conversation ratcheting on top of box-v1
- encrypt-to-self copies so own messages are recoverable across devices
- optional passphrase protection for identity.json
- fingerprint QR/emoji comparison flow

Do not invent custom cryptography. Use reviewed Rust crypto crates and keep
protocol fixtures small enough to audit.

## Release checklist

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `./scripts/e2e.sh` (GreenMail round trip)
- live smoke test against a real QQ/Gmail mailbox
- verify no TypeScript/JavaScript source files are present
- verify no secrets are written outside ignored local cache paths
