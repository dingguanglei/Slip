# Architecture

Slip is a single-crate Rust application: a reusable core (transport,
protocol, crypto, storage, orchestration) with two frontends — a terminal UI
and a scriptable CLI. The product design lives in [DESIGN.md](DESIGN.md) and
the wire format in [PROTOCOL.md](PROTOCOL.md); this file describes how the
code is arranged.

## Boundaries

```text
src/providers.rs
  Provider registry:
  - preset IMAP/SMTP endpoints for QQ/Gmail/Outlook/163/126/iCloud/Yahoo
  - Security modes: ssl (implicit TLS), starttls, plain (test servers)
  - domain auto-detection for the login wizard

src/core.rs
  Owns network transport:
  - IMAP over rustls or plaintext (MailStream), STARTTLS upgrade
  - persistent MailboxSession: select, uid_search, fetch_raw, delete,
    ensure_folder, move_to (UID MOVE), IDLE
  - SMTP sending of prebuilt RFC 5322 bytes (lettre send_raw)
  - NetEase IMAP ID handshake; provider-aware login help
  - the dedicated Slip folder config (slip_folder)
  - legacy MIME summary parsing for the debug CLI

src/protocol.rs
  Owns the Slip wire format (v1):
  - build_mail: headers, text fallback, JSON envelope, media parts
  - parse_mail_bytes: v1 (plain and box-v1), v0 legacy, future versions
  - media manifest with kind/mime/size/sha256 integrity

src/crypto.rs
  Owns keys and sealing:
  - X25519 identity keypair in identity.json (0600)
  - fingerprints (hex SHA-256 prefix)
  - box-v1 sealing/opening (nonce || crypto_box ciphertext)

src/cache.rs
  Owns local persistence under ~/.slip:
  - slip.db (SQLite, WAL): a `messages` row table (one row per chat line,
    appended in O(1), deduped by (contact, msg_id)) plus a `kv` table for
    contacts, TOFU peers, and sync state (UID cursors, read marks). A
    pre-SQLite `sessions/*.json` + `state/peers/contacts/aliases.json`
    layout is imported once on first open; originals renamed to *.migrated.
  - login.json (v2 + legacy migration), identity.json — kept as separate
    0600 files (credentials and the identity key stay out of the DB)
  - attachment copies for sent and received media

src/chat.rs
  Owns chat semantics (ChatClient), shared by every frontend:
  - send: media staging, opportunistic encryption, delivery states, retry
  - composer parsing (quote/escape-aware @path + drag-drop)
  - sync: relocate inbox chat mail into the Slip folder, then cursor-based
    incremental fetch + id dedup; burn or archive
  - watch: IDLE loop with reconnect/backoff and poll fallback
  - TOFU peer updates, key-change detection, trust management
  - session summaries, unread derivation, read marks

src/engine.rs
  Frontend-agnostic orchestration (Engine):
  - owns the worker + watcher threads and the Command/Event channels
  - a frontend only sends Commands and drains Events; it never touches
    the network or the store directly

src/imgview.rs  Decode image bytes to ratatui half-block thumbnail Lines.
src/tui.rs      Ratatui view layer over the Engine (threads below).
src/cli.rs      Stable JSON command-line entry points.
src/llm.rs      Optional local GGUF analysis for the debug reader.
```

## Threads (via the Engine)

```text
┌────────────┐   Event (mpsc)    ┌──────────────┐
│  frontend  │◄──────────────────│ worker thread│──► IMAP sync connection
│ (TUI/GUI)  │──────────────────►│ (send/sync)  │──► SMTP connection
└────────────┘  Command (mpsc)   └──────────────┘
       ▲          via Engine      ┌──────────────┐
       └──────── Event ───────────│ watcher      │──► IMAP IDLE connection
                                  │ thread       │    (INBOX)
                                  └──────────────┘
```

`Engine::start` spawns the threads and hands back the two channels. The
frontend never blocks on the network: it renders state and turns input into
Commands, draining Events each frame (`try_next`) or blocking with
`recv_timeout`. The worker owns all store mutations triggered by commands;
the watcher runs its own incremental syncs when IDLE fires and pushes
results as Events. A GUI would drive the same Engine identically.

## Data flow

Sending:

```text
composer input
  -> chat::parse_composer (@path tokens)
  -> protocol::MediaSource::from_path (classify, hash)
  -> cache: outgoing attachment copies + line (status=sending)
  -> protocol::build_mail (+ crypto seal when peer key is trusted)
  -> core::send_raw_mail (SMTP)
  -> cache: line status=sent | failed(+/retry)
```

Receiving:

```text
IDLE wake (or /sync)
  -> relocate: MOVE inbox [slip/chat] mail into the Slip folder
  -> select the Slip folder, read its cursor from slip.db
  -> UID SEARCH UID <last+1>:* SUBJECT "[slip/chat]"
  -> fetch raw bytes -> protocol::parse_mail_bytes
  -> drop inbound mail claiming to be from us (anti-forgery)
  -> TOFU peer update (record / key-change flag; no plaintext downgrade)
  -> media bytes written under attachments/, hashes verified
  -> session line insert (dedup by Slip id)
  -> archive in the Slip folder, or burn when SLIP_BURN is set
     (only when fully parsed; unreadable/incomplete mail stays)
  -> cursor advance, notify UI (bell, OSC 9, unread)
  -> a fallback pass sweeps the inbox for any mail a move missed
```

## Persistence format

JSON-on-disk, one file per concern, to stay inspectable pre-1.0. Sessions
are per-contact arrays of message records; serde defaults keep files written
by the QQ/Gmail-era MVP readable. A move to SQLite stays on the roadmap for
when the schema stabilizes.

## Failure handling

- Local persistence always precedes remote deletion; a failed burn is
  retried next sync and deduplicated by message id.
- Sends persist as `sending` before SMTP and settle to `sent`/`failed`;
  `/retry` re-sends under the same id, so double delivery dedups.
- The watcher reconnects with capped exponential backoff and degrades to
  polling when the server lacks IDLE.
- Unreadable Slip mail (future versions, undecryptable payloads) is shown
  as a placeholder and never burned.

## Rust-only policy

Slip contains no TypeScript/JavaScript, no Node manifests, and no vendored
frontend runtime code. Reference products may inform interaction patterns,
but implementation is written and reviewed as Rust. TLS is pure Rust
(rustls); no system OpenSSL is required.
