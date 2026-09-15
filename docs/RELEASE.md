Slip 0.2.0 brings email-based encrypted conversations to the desktop.

- Electron desktop app with a Rust messaging engine; no separate runtime installation.
- Manual mailbox login, local history, public-key friend requests and encrypted text/attachments.
- A quieter light interface with icon navigation, contacts, image previews and native file downloads.
- Validated local persistence before targeted remote cleanup; retry failed cleanup without touching ordinary mail.

## Downloads

- **macOS ARM64**: macOS 13 or newer. Extract the ZIP and open Slip.app.
- **Windows x64**: Windows 10 / 11. Extract the entire ZIP and run Slip.exe.
- **Ubuntu ARM64 / AMD64**: Ubuntu 22.04 or newer. Install the matching DEB with `sudo apt install ./<package>.deb`.

SHA-256 hashes are provided in SHA256SUMS. Packages contain no accounts, credentials, conversation records or demonstration mode.

## Validation and limitations

Source format/static checks and local functional validation completed before release. Linux ARM64 was used for desktop development; Windows backend smoke checks ran under Wine/QEMU. Native macOS and Windows GUI operation and Ubuntu x86-64 desktop operation have not been verified on physical target machines.

The apps are unsigned and the macOS app is not notarized. OS security prompts may appear. Signing is not claimed by this release.

Messages and attachments remain plaintext in local storage after decryption. First-contact identity verification uses TOFU, requires out-of-band fingerprint checks, and does not provide forward secrecy. Mail providers still see routing metadata; cleanup cannot remove their internal backups. See SECURITY.md for the full model.
