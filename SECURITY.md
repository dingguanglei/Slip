# Security Policy

## Supported versions

The project is pre-1.0. Security fixes target the `main` branch.

## Reporting

Open a private report through GitHub security advisories if available. If
not, open a minimal public issue that describes the affected area without
sharing credentials, tokens, message bodies, or mailbox dumps.

## Credential handling

- Do not use normal account passwords; use app passwords or authorization
  codes with IMAP enabled.
- Do not commit `.env`, cache directories, key files, or mailbox exports.
- CLI credentials must be supplied through environment variables.
- The optional TUI cached login (`login.json`) and the identity keypair (`identity.json`)
  are stored under `SLIP_HOME`/`~/.slip` with mode 0600 on Unix.
- `SLIP_PASSPHRASE` encrypts the identity secret key at rest with
  Argon2id + XChaCha20-Poly1305, so a stolen `identity.json` is useless
  without the passphrase. Set it before first run, or on a later run to
  migrate an existing plaintext key (clearing it migrates back). Slip has
  no recovery path: lose the passphrase and the identity — and any
  ciphertext only it can open — is unrecoverable.
- TLS certificate verification cannot be disabled by a runtime switch.
- Logging is off unless `SLIP_LOG` is set, and then records metadata only
  (addresses, message ids, sizes, counts) — never passwords, auth codes,
  keys, or message bodies. The account secret is additionally registered
  with the logger and masked to `***` in every line as a backstop.

## Encryption model

Slip messages are end-to-end encrypted with `box-v1` (X25519 +
XSalsa20-Poly1305 via the RustCrypto `crypto_box` crate) once both sides
have seen each other's key — automatic after one round trip. Trust is TOFU:
a changed sender key suspends encryption and warns until the user re-trusts
it. Full format and threat model: [docs/PROTOCOL.md](docs/PROTOCOL.md).

Known limitations, by design of email as a transport:

- Metadata (addresses, timing, approximate sizes, the `[slip/chat]`
  subject) is visible to mail providers.
- First contact sends an empty public-key exchange, never user content.
  Unknown or changed keys block text and attachment sends (including retries).
  The TOFU window still allows an active provider-level MITM at first contact;
  compare fingerprints out of band before sensitive conversations.
- Web listens only on 127.0.0.1 with a per-process access token. It has no
  public hosting mode. Web credentials are held in memory, not persisted.
- Received chat is removed only after successful validation and local storage.
  Local history is plaintext; account directories use mode 0700 on Unix.
  Remote cleanup cannot guarantee deletion of provider backups or unrecognized copies.
- No forward secrecy yet: compromise of an identity key exposes archived
  past traffic. Ratcheting is on the roadmap.

## Desktop application boundary

Electron starts a loopback-only Rust backend on a random port with an ephemeral access token. Renderer Node.js is disabled; sandbox, context isolation and web security are enabled. External navigation, new windows and permission requests are denied. Mailbox credentials are not inherited from the shell and are kept only in the current backend session. Closing the app terminates the backend; local identity, history and attachments remain on the device. Distribution packages contain only app resources and the matching backend, never development account stores.
