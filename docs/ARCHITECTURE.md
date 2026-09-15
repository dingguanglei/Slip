# Architecture

Slip has a Rust mail/chat core and an Electron desktop frontend and an embedded local Web UI. SMTP/IMAP is the
transport; the local device is the encryption endpoint. There is no hosted chat
backend and no browser-to-provider credential connection.

## Components

| Component | Responsibility |
| --- | --- |
| `core.rs` | IMAP/SMTP over TLS; PEEK fetches; IDLE; single-UID cleanup; Gmail Trash handling |
| `protocol.rs` | MIME v1 envelope; authenticated text/media encryption; integrity checks; canonical outbound wire digests |
| `crypto.rs` | X25519 + XSalsa20-Poly1305 identity and seals; optional private-key encryption at rest |
| `cache.rs` | SQLite WAL with FULL synchronization; account state, peer keys, messages, outbound receipts; local media |
| `chat.rs` | Strict sending, public-key exchange, validation, deduplication, retries, safe remote cleanup |
| `engine.rs` | Command worker and IMAP IDLE watcher; explicit frontend cleanup policy |
| `bin/slip-web.rs` | Loopback HTTP API, account isolation, bounded uploads, account-scoped downloads |
| `web/` | Static HTML/CSS/JavaScript IM interface, embedded in the binary; no npm/CDN dependency |

The TUI and debug CLI remain consumers of the Rust core. The desktop app starts the same local Web engine. The CLI includes `chat-exchange --email` for headless peers.

## Sending

1. Check a usable peer key exists; unknown, disabled, or changed keys cannot send.
2. Build an encrypted envelope and encrypt every attachment. Only an empty
   public-key exchange may use plaintext MIME.
3. Copy local media into a unique directory; fsync files/directories. Persist
   the outgoing message as `sending` and its canonical wire digest.
4. Submit ciphertext over SMTP. Persist `sent` (accepted by server) or `failed`.
5. Retries preserve the message ID and check encryption again. Every wire
   version is remembered so uncertain SMTP outcomes can be deduplicated.

## Receiving and cleanup

1. Subject search produces candidates only; no mail is moved based on a search hit.
2. Fetch with `BODY.PEEK[]`; validate protocol, authenticated envelope, recipient,
   peer key and attachment hashes before persisting any chat content.
3. Persist media and SQLite before deleting any received copy. Corrupt, legacy,
   wrong-recipient and unknown-key-change messages stay remote.
4. Recognize provider-created sent copies only by a canonical MIME digest
   recorded locally before sending, plus account/key/protocol headers. Do not
   trust `From: me` or a matching ID alone.
5. Delete a single UID. Gmail first moves the validated copy to its special-use
   Trash, then purges its Trash UID. Never use a bare `EXPUNGE`.
6. Report retained/unresolved cleanup counts. Cleanup mode rescans candidates,
   so a failed deletion or a retained message is not hidden behind an advanced
   cursor. Non-cleanup CLI sync retains incremental cursor behavior.

A per-client sync mutex serializes the watcher and manual sync. SQLite handles
concurrent readers; read-modify-write peer/contact operations use the shared
cache lock. Unreadable mail never reserves a deduplication ID. Historical
incomplete placeholders can be replaced by subsequently verified content.

## Web boundary

The service listens on `127.0.0.1`, checks Host, requires a random Bearer token
for all account/message/media APIs, provides no CORS grant, and sends no-store
and CSP headers. Eight worker threads bound HTTP concurrency. Web explicitly
enables cleanup regardless of the TUI environment preference.

Uploads contain bytes and names, never host paths. Media downloads identify an
account, contact, message ID and media index; the canonical file path must be
within that account's store. HTML is rendered as text. Raster images use local
Blob previews; other files download as octet streams.

## Desktop process boundary

`desktop/main.cjs` starts `slip-web --parent-stdio` with a restricted environment
and an ephemeral loopback port. It creates an isolated renderer with Node.js
disabled, denies external navigation and permission requests, and manages
native attachment downloads. Closing the parent pipe terminates the backend.
The desktop package contains only the allowlisted application resources and
the backend for its target architecture. No account state is shipped.
