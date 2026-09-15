# Slip Wire Protocol v1

> Current application policy: v1 text and attachments MUST be encrypted. The
> only plaintext send is an empty envelope with no media for public-key
> exchange. The legacy/plaintext layouts below document decoding compatibility,
> not permission to transmit user content. Web rejects legacy content for
> ingestion/cleanup and always enables validated save-before-delete.


Slip turns ordinary mailboxes into chat endpoints. Every Slip message is a
standard RFC 5322 email, so it survives any SMTP/IMAP provider and degrades
gracefully in a normal mail client. This document is the canonical
specification for protocol version 1.

## Design goals

1. **Chat semantics over mail transport.** Message identity, ordering,
   deduplication, and delivery status are defined by Slip, not by the mailbox.
2. **Multimedia first.** A message is text plus zero or more media items
   (image, audio, file), each described by a manifest, not guessed from MIME.
3. **Opportunistic end-to-end encryption.** Identity keys ride on every
   message; once both sides have seen each other's key, payloads are encrypted
   automatically. Trust is TOFU (trust on first use) with key-change alarms.
4. **Graceful degradation.** A non-Slip mail client sees a readable text part
   and normal attachments (or a short notice for encrypted messages).
5. **Push, not polling.** Delivery latency is whatever email takes, but
   arrival must be surfaced immediately via IMAP IDLE.

## Message identification

A mail is a Slip chat message when **both** hold:

- `Subject:` is exactly `[slip/chat]` (cheap server-side IMAP filter), and
- an `X-Slip-Version` header is present (v0 legacy mail lacks it; see
  [Compatibility](#compatibility)).

### Headers

| Header | Required | Value |
| --- | --- | --- |
| `Subject` | yes | exactly `[slip/chat]` |
| `X-Slip-Version` | yes | `1` |
| `X-Slip-Id` | yes | 32 lowercase hex chars — 128-bit random, client-generated |
| `X-Slip-Key` | yes | base64 (44 chars) of the sender's X25519 public key |
| `X-Slip-Enc` | yes | `none` or `box-v1` |

`X-Slip-Id` is the canonical deduplication key across resends, multi-device
reads, and provider-side copies. `Message-ID` is still emitted for mail-client
threading but is never trusted for identity (relays may rewrite it).

## Plaintext payload (`X-Slip-Enc: none`)

```text
multipart/mixed
├── text/plain; charset=utf-8            human-readable fallback (the text body)
├── application/x-slip+json              canonical envelope (UTF-8 JSON)
└── 0..n attachment parts                Content-Disposition: attachment
```

### Envelope JSON

```json
{
  "v": 1,
  "id": "9f2c58a1f0e6420bb0cd6a5c3f8e7d21",
  "ts": 1752570000,
  "from": "alice@example.com",
  "to": ["bob@example.com"],
  "text": "look at this",
  "media": [
    {
      "idx": 1,
      "kind": "image",
      "name": "photo.png",
      "mime": "image/png",
      "size": 48213,
      "sha256": "…64 hex…"
    }
  ]
}
```

- `id`/`ts` must match the headers (`ts` is Unix seconds).
- `media[].idx` is the 1-based position among the mail's *attachment parts*
  (parts with an attachment disposition, in document order).
- `media[].kind` is one of `image`, `audio`, `file`. The sender classifies by
  MIME type (`image/*` → image, `audio/*` → audio, else file); receivers must
  honor the manifest, not re-guess.
- `media[].sha256` is the hash of the decoded attachment bytes; receivers
  verify and flag mismatches.
- `text` may be empty when the message is media-only.

Receivers prefer the JSON envelope; the `text/plain` part exists only for
foreign mail clients.

## Encrypted payload (`X-Slip-Enc: box-v1`)

Cipher: `crypto_box` — X25519 + XSalsa20-Poly1305 (NaCl `box`), from the
audited RustCrypto `crypto_box` crate. Static-static: sender's identity secret
key with recipient's identity public key. This authenticates the sender (only
a holder of the sender key can produce a valid box for that key pair).

Every encrypted item is laid out as:

```text
nonce (24 random bytes) || box ciphertext
```

Structure:

```text
multipart/mixed
├── text/plain                           "Encrypted Slip message. Open with Slip: …"
├── application/x-slip-encrypted         base64 body: sealed envelope JSON
└── 0..n attachments named "1.slip" …    application/octet-stream, sealed bytes
```

- The sealed envelope is the same JSON as plaintext mode; real filenames,
  MIME types, sizes, and hashes live **only** inside it. Attachment parts are
  renamed `<idx>.slip` so providers learn nothing but sizes.
- `media[].sha256` refers to the *plaintext* bytes, checked after decryption.
- The envelope's `from`/`to`/`ts` bind the ciphertext to the conversation and
  block cross-conversation replay: receivers verify `from` matches the TOFU
  key binding for the sender address and `to` contains their own address.

### Identity and trust (TOFU)

- Each installation generates one X25519 keypair on first run, stored in
  `~/.slip/identity.json` (mode 0600). No registration, no key server.
- Every outgoing message carries the sender's public key in `X-Slip-Key`.
- On the first message from a contact, the receiver records the key in
  `~/.slip/slip.db` (TOFU peers) with `fingerprint = hex(SHA-256(pubkey))[..16]`.
- Once a peer key is known, outgoing messages to that contact are encrypted
  without a plaintext fallback. Disabled or pending keys block sends.
- If a message arrives under a **different** key for a known contact, the
  message stays remote, the UI raises a key-change warning, and
  encryption to that contact is suspended until the user re-trusts the new
  key (`/trust`).
- Fingerprints are displayed (`/info`) for out-of-band verification.

### Threat model

- **Protects:** message text and media contents from mail providers and
  passive interceptors, once keys are exchanged; sender authenticity between
  established peers.
- **Does not protect:** metadata (addresses, timing, approximate sizes, the
  `[slip/chat]` marker) — inherent to email; against an active MITM who substitutes
  keys during first contact (mitigate by comparing fingerprints out-of-band).
- **No forward secrecy in v1** (static-static). Compromise of an identity key
  exposes past traffic that the attacker archived. Roadmap: ratcheting.

## Deduplication and ordering

- Primary dedup key: `X-Slip-Id` (envelope `id`). A receiver that has stored
  an id must ignore further copies.
- Legacy v0 lines (no id) dedup by `(sender, body, timestamp)` as before.
- Display order: envelope `ts`, tie-broken by id. Clock skew between peers is
  accepted (chat apps show sender-claimed time).

## Sync model

- Cursor per `(account, mailbox)`: `{ uidvalidity, last_uid }`, persisted in
  `~/.slip/slip.db` (sync state).
- Incremental fetch: `UID SEARCH UID <last_uid+1>:* SUBJECT "[slip/chat]"`,
  then `UID FETCH` of the matches only.
- If `UIDVALIDITY` changes, the cursor resets and a bounded rescan runs.
- **Burn-after-save**: Web always enables cleanup. Debug CLI uses `--burn`.
  Once a validated message and media are durably stored, its remote UID is
  removed. Cleanup mode rescans candidates so failures are retried.
- Only exact outbound MIME digests recorded locally authorize sent-copy cleanup.
  Gmail copies go through special-use Trash before targeted UID EXPUNGE.
- `BODY.PEEK[]` avoids changing read flags. Never issue an unqualified EXPUNGE.
- Local persistence always precedes remote deletion; a failed burn is retried
  on the next sync and deduplicated by id.

## Push (immediate arrival notification)

- A dedicated IMAP connection sits in `IDLE` on `INBOX`; any untagged
  `EXISTS`/`RECENT` triggers an incremental sync immediately.
- IDLE re-issues before the 29-minute RFC deadline; disconnects reconnect
  with capped exponential backoff (2s, 4s, … 60s max).
- Servers without the IDLE capability fall back to short polling
  (`SLIP_POLL_SECS`, default 15s).
- On new inbound messages the UI must notify at once: terminal bell, an OSC 9
  desktop notification where the terminal supports it, unread badges, and (if
  the conversation is open) immediate render.

## Size limits

Most providers cap raw mail size (Gmail ≈ 25 MB, QQ ≈ 50 MB) and base64 adds
~33%. Slip warns when total attachment bytes exceed 18 MB and refuses over
30 MB with a clear error naming the limit.

## Compatibility

The parser can recognize legacy envelopes, but the chat client only accepts
fully validated v1 messages for local chat delivery and remote cleanup.
Legacy or future versions are retained on the mail server; recognizing a
matching subject is never permission to delete a message.
