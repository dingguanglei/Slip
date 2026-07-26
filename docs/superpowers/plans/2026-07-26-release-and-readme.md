# Slip v0.1.0 Release and README Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish four native `slip-tui` archives in the GitHub v0.1.0 Release and replace the README with a concise core-capabilities landing page.

**Architecture:** A four-entry GitHub Actions matrix builds and uploads native platform archives. A dependent release job downloads all archives, verifies their count, generates checksums, and publishes only after every build succeeds. The README becomes a small public entrypoint while detailed technical material stays in the existing documents.

**Tech Stack:** Rust/Cargo, GitHub Actions, shell, PowerShell, Markdown

## Global Constraints

- Release version and tag are exactly `0.1.0` and `v0.1.0`.
- Build only `slip-tui`; do not include `slip-cli`.
- Targets are exactly Linux x64, Linux arm64, macOS arm64, and Windows x64.
- Every archive contains the TUI executable, `README.md`, and `LICENSE`.
- Publish four archives plus one `SHA256SUMS` file only after all builds pass.
- Keep the README English-only and avoid universal mailbox compatibility claims.
- Preserve the required first-contact plaintext and provider-visible metadata disclosures.

---

### Task 1: Replace README with the concise public landing page

**Files:**
- Modify: `README.md`

**Interfaces:**
- Consumes: Existing documentation links and GitHub Releases URL.
- Produces: The README bundled in every platform archive.

- [ ] **Step 1: Run the scope assertion against the current README**

```bash
python3 - <<'PY'
from pathlib import Path

text = Path("README.md").read_text()
required = [
    "## Core capabilities",
    "## Download",
    "## Security boundaries",
    "## Documentation",
    "## License",
]
removed = [
    "## CLI",
    "## Local data",
    "## Development",
    "## Screenshots",
]
assert all(item in text for item in required)
assert all(item not in text for item in removed)
assert len(text.splitlines()) <= 100
PY
```

Expected: FAIL because the current README uses `## Highlights`, contains the
detailed sections, and is longer than 100 lines.

- [ ] **Step 2: Write the concise README**

Keep the logo and CI/license badges. Replace the remaining body with:

```markdown
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
```

- [ ] **Step 3: Re-run the README scope assertion**

Run the Python assertion from Step 1.

Expected: PASS.

- [ ] **Step 4: Validate Markdown links and whitespace**

```bash
git diff --check
python3 - <<'PY'
from pathlib import Path
import re

text = Path("README.md").read_text()
for target in re.findall(r"\[[^\]]+\]\(([^)]+)\)", text):
    if "://" not in target:
        assert Path(target).exists(), target
PY
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add README.md
git commit -m "Simplify README for release"
```

### Task 2: Make Release publication atomic across four native TUI builds

**Files:**
- Modify: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: A pushed tag matching `v*`, `Cargo.lock`, `README.md`, and `LICENSE`.
- Produces: Four workflow artifacts consumed by the release job, then four Release archives and `SHA256SUMS`.

- [ ] **Step 1: Run workflow contract assertions against the current workflow**

```bash
python3 - <<'PY'
from pathlib import Path

text = Path(".github/workflows/release.yml").read_text()
required = [
    "ubuntu-24.04-arm",
    "aarch64-unknown-linux-gnu",
    "actions/upload-artifact@v4",
    "actions/download-artifact@v4",
    "needs: build",
    "--bin slip-tui",
    "SHA256SUMS",
]
for value in required:
    assert value in text, value
assert "--bin slip-cli" not in text
assert "x86_64-apple-darwin" not in text
PY
```

Expected: FAIL at `ubuntu-24.04-arm` because the current workflow lacks Linux
arm64 and publishes independently from each build job.

- [ ] **Step 2: Replace the workflow with the four-build plus release-job design**

Use these matrix entries:

```yaml
include:
  - os: ubuntu-24.04
    target: x86_64-unknown-linux-gnu
  - os: ubuntu-24.04-arm
    target: aarch64-unknown-linux-gnu
  - os: macos-14
    target: aarch64-apple-darwin
  - os: windows-2022
    target: x86_64-pc-windows-msvc
```

Build with:

```yaml
run: cargo build --release --locked --target ${{ matrix.target }} --bin slip-tui
```

Package Unix targets as
`slip-tui-${version}-${target}.tar.gz` and Windows as
`slip-tui-${version}-${target}.zip`. Upload each archive using
`actions/upload-artifact@v4` with `retention-days: 1`.

Add a separate Ubuntu `release` job:

```yaml
release:
  needs: build
  runs-on: ubuntu-24.04
  permissions:
    contents: write
```

Download all matrix artifacts with `actions/download-artifact@v4` using
`merge-multiple: true`. Fail unless exactly four `.tar.gz`/`.zip` files exist.
Generate `SHA256SUMS` with `sha256sum`, then publish all five files using
`softprops/action-gh-release@v2` with `generate_release_notes: true` and
`fail_on_unmatched_files: true`.

- [ ] **Step 3: Re-run workflow contract assertions**

Run the Python assertion from Step 1.

Expected: PASS.

- [ ] **Step 4: Validate YAML and inspect the final diff**

```bash
python3 - <<'PY'
from pathlib import Path
import yaml

document = yaml.safe_load(Path(".github/workflows/release.yml").read_text())
assert isinstance(document, dict)
assert "jobs" in document
PY
git diff --check
git diff -- .github/workflows/release.yml
```

Expected: YAML parses, whitespace check passes, and the diff contains exactly
four requested targets with publication moved to the dependent release job.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "Publish four-platform TUI releases"
```

### Task 3: Verify, push, tag, and monitor v0.1.0

**Files:**
- Verify: `README.md`
- Verify: `.github/workflows/release.yml`
- Verify: `Cargo.toml`

**Interfaces:**
- Consumes: The two implementation commits and the existing `0.1.0` Cargo package version.
- Produces: GitHub tag and Release `v0.1.0` with five downloadable assets.

- [ ] **Step 1: Run the local release gate**

```bash
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked --bin slip-tui
git diff --check
```

Expected: All commands exit 0; Cargo may repeat the known future-compatibility
warning for `imap-proto v0.10.2`.

- [ ] **Step 2: Push main**

```bash
git push origin main
```

Expected: Remote `main` advances to the implementation commit.

- [ ] **Step 3: Create and push the release tag**

```bash
test "$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)" = "0.1.0"
git tag -a v0.1.0 -m "Slip v0.1.0"
git push origin refs/tags/v0.1.0
```

Expected: GitHub receives the new annotated tag and starts the Release workflow.

- [ ] **Step 4: Monitor the Release workflow**

Use the connected GitHub API to inspect tag-triggered workflow runs and wait
until the Release workflow reaches a terminal state.

Expected: Four build jobs and the dependent release job all conclude
`success`. If a build fails, inspect its log, fix the workflow or source,
delete the failed tag and incomplete Release if present, commit the fix, and
recreate the same tag only after verification.

- [ ] **Step 5: Verify Release assets**

Query the GitHub v0.1.0 Release and confirm these assets exist:

```text
slip-tui-0.1.0-x86_64-unknown-linux-gnu.tar.gz
slip-tui-0.1.0-aarch64-unknown-linux-gnu.tar.gz
slip-tui-0.1.0-aarch64-apple-darwin.tar.gz
slip-tui-0.1.0-x86_64-pc-windows-msvc.zip
SHA256SUMS
```

Expected: Exactly the five requested uploaded assets are present, each archive
is non-empty, and the Release URL is
`https://github.com/dingguanglei/Slip/releases/tag/v0.1.0`.
