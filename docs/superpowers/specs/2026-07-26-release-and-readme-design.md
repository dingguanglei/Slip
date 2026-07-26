# Slip v0.1.0 Release and README Design

## Goal

Publish Slip v0.1.0 with one TUI archive for each requested platform, and
replace the long project README with a concise public landing page focused on
the product's core capabilities.

## Release assets

The release must contain exactly these platform archives:

| Platform | GitHub runner | Rust target | Archive |
|---|---|---|---|
| Linux x64 | `ubuntu-24.04` | `x86_64-unknown-linux-gnu` | `.tar.gz` |
| Linux arm64 | `ubuntu-24.04-arm` | `aarch64-unknown-linux-gnu` | `.tar.gz` |
| macOS arm64 | `macos-14` | `aarch64-apple-darwin` | `.tar.gz` |
| Windows x64 | `windows-2022` | `x86_64-pc-windows-msvc` | `.zip` |

Each archive contains:

- the `slip-tui` executable (`slip-tui.exe` on Windows);
- `README.md`;
- `LICENSE`.

The release also contains a `SHA256SUMS` file covering all four archives.
The CLI binary and macOS x64 binary are outside this release's scope.

## Workflow

Pushing a version tag matching `v*` starts a four-entry native build matrix.
Each build compiles only `slip-tui` with `cargo build --release --locked`,
packages its archive, and uploads the archive as a workflow artifact.

A separate `release` job depends on the whole build matrix. It downloads all
four artifacts, verifies that four archives exist, generates `SHA256SUMS`, and
creates or updates the GitHub Release for the tag. This prevents a partially
successful matrix from publishing a partial release.

The first production tag is `v0.1.0`, matching the version in `Cargo.toml`.
GitHub-generated release notes are enabled.

## README

The README remains English-only and keeps:

1. the icon, CI badge, license badge, and a one-sentence product position;
2. five core capabilities:
   - chat over an IMAP/SMTP mailbox;
   - push-style TUI conversations with IMAP IDLE;
   - automatic end-to-end encryption after key exchange, with TOFU warnings;
   - encrypted text and media attachments;
   - local history, retries, deduplication, and mailbox cleanup;
3. a minimal download-and-run section linked to GitHub Releases;
4. a compact security-boundary note;
5. links to protocol, architecture, security, and license documents.

Detailed CLI commands, environment variables, local file layout, development
commands, repository layout, screenshots, and long provider lists are removed
from the landing page. Existing detailed documents remain available through
links.

The README must not claim universal mailbox compatibility. It states that the
mailbox must expose IMAP/SMTP and support an app password or authorization
code. It also discloses that the first contact message is plaintext before key
exchange and that providers can observe email metadata.

## Verification and publication

Before tagging:

- `cargo fmt --check`;
- `cargo test --locked`;
- `cargo clippy --all-targets --locked -- -D warnings`;
- `cargo build --release --locked --bin slip-tui`;
- validate the workflow YAML and inspect its matrix and asset names;
- verify the README links and requested scope.

After pushing `main` and the `v0.1.0` tag:

- wait for the Release workflow to finish;
- verify all four jobs and the final release job succeeded;
- verify the GitHub Release contains four archives plus `SHA256SUMS`;
- report asset URLs and any remaining warnings.

If any build fails, no release job runs and no partial release is accepted as
complete.
