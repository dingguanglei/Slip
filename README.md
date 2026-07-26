# Slip

<p align="center">
  <img src="assets/slip-icon.svg" alt="Slip icon" width="128">
</p>

<p align="center">
  <a href="https://github.com/dingguanglei/Slip/actions/workflows/ci.yml"><img src="https://github.com/dingguanglei/Slip/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License MIT">
</p>

Slip turns an IMAP/SMTP mailbox into an encrypted terminal messenger, without
requiring a separate chat account or service.

## Core capabilities

- **Email transport.** Use a mailbox that exposes IMAP/SMTP and supports an
  app password or authorization code.
- **Live terminal chat.** IMAP IDLE delivers new messages to the Ratatui
  interface, with polling fallback when IDLE is unavailable.
- **Automatic encryption.** After peers exchange keys, text and attachments
  use authenticated end-to-end encryption. TOFU key changes stop encrypted
  sending until explicitly trusted.
- **Text and media.** Send text, images, audio, and files with integrity
  metadata and inline image previews.
- **Reliable local history.** SQLite storage, deduplication, delivery states,
  retries, incremental sync, and mailbox archiving keep conversations usable.

## Download

Download the `v0.1.0` archive for your platform from
[GitHub Releases](https://github.com/dingguanglei/Slip/releases), extract it,
and run `slip-tui` (`slip-tui.exe` on Windows).

Available builds: Linux x64, Linux arm64, macOS arm64, and Windows x64.

On first run, enter your mailbox address, IMAP/SMTP settings when needed, and
an app password or authorization code.

## Security boundaries

The first message to a new contact is plaintext because no peer key is known
yet. Email providers can still observe addresses, timing, sizes, and the
message subject. TOFU does not prevent a first-contact interception; compare
fingerprints out of band for sensitive conversations. Slip does not currently
provide forward secrecy.

See [SECURITY.md](SECURITY.md) for the threat model.

## Documentation

- [Protocol](docs/PROTOCOL.md)
- [Architecture](docs/ARCHITECTURE.md)
- [Product design](docs/DESIGN.md)
- [Roadmap](docs/ROADMAP.md)

## License

MIT. See [LICENSE](LICENSE).
