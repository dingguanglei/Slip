# Slip Product Design

Slip is a chat application. Email is only its transport. The bar for every
interaction: *it must feel like a messenger, never like a mail client.*

## Product principles

1. **No registration.** Your email address is your identity; logging into
   IMAP/SMTP is the whole onboarding.
2. **Any mailbox.** Well-known providers are preconfigured; everything else
   works through a custom-server step. QQ Mail and Gmail are first-class,
   not exclusive.
3. **Latency-tolerant, notification-eager.** Delivery takes as long as email
   takes; that is acceptable. What is not acceptable is finding out late:
   arrival must be pushed to the user the moment the mailbox learns of it
   (IMAP IDLE → bell + desktop notification + unread badge).
4. **Multimedia is designed, not bolted on.** Text, images, audio, and files
   each have an explicit representation in the protocol manifest, in the
   store, and in the UI.
5. **Local store is the product's memory.** Mail is burned after save by
   default; conversations live in `~/.slip`, not in the mailbox.
6. **Private by default, honest about limits.** Encryption turns on by itself
   once keys are exchanged; the UI always shows whether a conversation is
   encrypted and warns loudly on key changes.

## First run

```text
slip
└── Login wizard
    1. Email address        → provider auto-detected from domain
    2. (unknown domain)     → IMAP host/port, SMTP host/port, security
    3. Password / auth code → provider-specific hint (QQ auth code, Gmail
                              app password, …)
    4. Connect check        → IMAP login + folder probe with a clear error
                              and a fix hint on failure
    5. Saved                → login.json (0600); next launch goes straight
                              to chat
```

Provider presets: QQ, Gmail, Outlook, NetEase 163/126, iCloud, Yahoo, and
`custom`. Presets fill IMAP/SMTP endpoints, security mode, and the secret
hint; NetEase presets also send the IMAP `ID` handshake their servers demand.

## Main screen

```text
┌ Slip ─ alice@qq.com ──────────────────────────────────────────────┐
│ Contacts               │ bob@gmail.com          🔒 encrypted      │
│                        │ ─────────────────────────────────────────│
│ › bob@gmail.com    (2) │           me 14:02 ✓                     │
│   carol@163.com        │           did you get the mockups?       │
│   dave@qq.com          │                                          │
│                        │ bob 14:05                                │
│                        │ yes — one comment on the header          │
│                        │ 🖼 image mockup-v2.png (1.2 MB)          │
│                        │                                          │
│ ─ status ────────────  │ ┌ composer ─────────────────────────────┐│
│ ● idle listening       │ │ › sounds good @./v3.png▌              ││
└────────────────────────┴─────────────────────────────────────────-┘
```

- **Left pane:** persistent contact list — unread badge, last-message
  preview, sorted by recency. `Ctrl+N/P` or `↑/↓` (empty composer) moves.
- **Right pane:** the open conversation. Own messages right-aligned tag
  `me`, peer messages left. Every message shows time and delivery state.
- **Composer:** plain text sends text; `@path` tokens attach media; slash
  commands autocomplete inline (the existing menu).

### Message rendering

| Content | Rendering |
| --- | --- |
| text | wrapped body text |
| image | `🖼 image name (size)` + open hint |
| audio | `🎵 audio name (size)` + open hint |
| file | `📄 file name (size)` + open hint |
| encrypted | `🔒` on the header line |
| key change | red banner in conversation + status warning |

`o` (or `Enter` on a selected message) opens the newest media item with the
system handler (`xdg-open`/`open`). Media bodies live under
`~/.slip/attachments/`.

### Delivery states

`⏳ sending` → `✓ sent` (SMTP accepted) → `✗ failed` (kept locally with the
error; `/retry` resends). No read receipts in v1.

### Arrival notification

The moment the watcher hears about new mail: incremental sync runs, then —

1. terminal bell (`BEL`),
2. OSC 9 desktop notification (`"Slip: <sender>: <preview>"`) on terminals
   that support it (harmless elsewhere),
3. unread badge + contact list bump,
4. if the conversation is open, the message renders immediately and the
   view follows the tail.

### Slash commands

```text
/open [email]     open a conversation (alias: /resume)
/add <email>      add a contact
/sync             manual incremental sync
/info             conversation details: keys, fingerprints, encryption state
/trust            accept a changed peer key after verifying out-of-band
/encrypt on|off   per-contact encryption override (default: on when possible)
/retry            resend the last failed message
/help             command list
/quit             leave (Esc also works)
```

## Architecture

```text
┌────────────┐  UiEvent (mpsc)   ┌──────────────┐
│  UI thread │◄──────────────────│ worker thread│──► IMAP sync conn
│  (ratatui) │──────────────────►│ (sync/send)  │──► SMTP conn
└────────────┘  Command (mpsc)   └──────▲───────┘
                                        │ SyncNow
                                 ┌──────┴───────┐
                                 │ watcher      │──► IMAP IDLE conn
                                 │ thread       │    (INBOX)
                                 └──────────────┘
```

- The UI thread never touches the network; it polls key events and drains
  `UiEvent`s each frame, so the interface stays responsive during sends and
  syncs.
- The worker owns the store mutations: build MIME → SMTP send → persist →
  emit status events; incremental sync → persist → emit new-message events.
- The watcher holds a second IMAP connection in IDLE and only ever says
  "something changed"; reconnects with capped backoff; falls back to polling
  when IDLE is unavailable.

Module map:

```text
src/providers.rs  provider registry, security modes, env/credential rules
src/core.rs       IMAP/SMTP transport, TLS/STARTTLS/plain, IDLE watcher
src/protocol.rs   Slip envelope build/parse (spec: docs/PROTOCOL.md)
src/crypto.rs     identity keys, TOFU peer store, box-v1 sealing
src/cache.rs      ~/.slip persistence: sessions, cursors, peers, media
src/chat.rs       send/sync orchestration shared by CLI and TUI
src/tui.rs        ratatui frontend (threads, panes, wizard)
src/cli.rs        scriptable frontend (JSON out), incl. chat-watch
```

## Non-goals for v1

Group conversations, read receipts, message edit/delete, multi-account,
mail-reading features (Slip is not a mail client), audio recording (send
audio files; recording needs native audio stacks), forward secrecy
(documented in the threat model; on the roadmap).

## Testing strategy

- **Unit:** protocol round-trips, crypto vectors, cursor logic, provider
  table, composer parsing.
- **Integration (`tests/`):** synthetic RFC 5322 fixtures through the full
  parse path, plaintext and encrypted.
- **End-to-end (`scripts/e2e.sh`):** a GreenMail container provides
  `alice@slip.test` / `bob@slip.test`; the CLI exercises text, image, audio,
  file, and encrypted round trips both ways, plus `chat-watch` push latency,
  against real IMAP/SMTP sockets.
- **Live smoke:** the same flows against a real QQ/Gmail account before a
  release (manual, credentials never committed).
