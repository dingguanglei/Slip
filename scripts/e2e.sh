#!/usr/bin/env bash
# End-to-end test: two mailboxes on a local GreenMail server exchange Slip
# messages (text, image, audio, file, encrypted, key rotation, push).
#
# Requires: docker, cargo, python3. Run from the repository root:
#   ./scripts/e2e.sh
set -euo pipefail

CONTAINER=slip-greenmail
SMTP_PORT=3025
IMAP_PORT=3143
# Fixed throwaway credentials used only by the local GreenMail container.
TEST_PASSWORD=slip-e2e-only-password
TEST_PASSPHRASE=slip-e2e-only-passphrase
WRONG_TEST_PASSPHRASE=slip-e2e-wrong-passphrase
WORK="${SLIP_E2E_DIR:-$(mktemp -d /tmp/slip-e2e.XXXXXX)}"
BIN=(cargo run --quiet --bin slip-cli --)

say()  { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }
pass() { printf '\033[1;32mPASS\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31mFAIL\033[0m %s\n' "$*"; exit 1; }

cleanup() {
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

say "Starting GreenMail ($CONTAINER)"
cleanup
docker run -d --name "$CONTAINER" \
  -p "$SMTP_PORT:3025" -p "$IMAP_PORT:3143" \
  -e GREENMAIL_OPTS="-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 -Dgreenmail.users=alice:$TEST_PASSWORD@slip.test,bob:$TEST_PASSWORD@slip.test -Dgreenmail.users.login=email -Dgreenmail.verbose" \
  greenmail/standalone:2.1.3 >/dev/null

imap_ready=0
for _ in $(seq 1 60); do
  if (exec 3<>"/dev/tcp/127.0.0.1/$IMAP_PORT") 2>/dev/null; then
    exec 3>&- 3<&-
    imap_ready=1
    break
  fi
  sleep 0.5
done
[ "$imap_ready" = "1" ] || fail "GreenMail IMAP did not become ready"

smtp_ready=0
for _ in $(seq 1 60); do
  if python3 - "$SMTP_PORT" "$TEST_PASSWORD" >/dev/null 2>&1 <<'PY'
import smtplib
import sys

with smtplib.SMTP("127.0.0.1", int(sys.argv[1]), timeout=1) as smtp:
    smtp.login("alice@slip.test", sys.argv[2])
PY
  then
    smtp_ready=1
    break
  fi
  sleep 0.5
done
[ "$smtp_ready" = "1" ] || fail "GreenMail SMTP authentication did not become ready"

env_common=(
  "SLIP_IMAP_HOST=127.0.0.1" "SLIP_IMAP_PORT=$IMAP_PORT" "SLIP_IMAP_SECURITY=plain"
  "SLIP_SMTP_HOST=127.0.0.1" "SLIP_SMTP_PORT=$SMTP_PORT" "SLIP_SMTP_SECURITY=plain"
  "SLIP_SEND_MIN_MS=0"
)

alice() {
  env "${env_common[@]}" \
    SLIP_ADDRESS=alice@slip.test SLIP_PASSWORD="$TEST_PASSWORD" SLIP_HOME="$WORK/alice" \
    "${BIN[@]}" "$@"
}
bob() {
  env "${env_common[@]}" \
    SLIP_ADDRESS=bob@slip.test SLIP_PASSWORD="$TEST_PASSWORD" SLIP_HOME="$WORK/bob" \
    "${BIN[@]}" "$@"
}

jget() { python3 -c "import json,sys;d=json.load(sys.stdin);print(eval(sys.argv[1],{},{'d':d}))" "$1"; }

# GreenMail delivers over SMTP asynchronously, so a sync issued right after a
# send can race ahead of delivery. Retry the recipient's sync (given as the
# command + args) until it saves at least one message, up to ~12s. Echoes the
# final sync JSON so callers can assert on it.
sync_until_saved() {
  local out=""
  for _ in $(seq 1 12); do
    out=$("$@")
    if [ "$(echo "$out" | jget "d['sync']['saved']" 2>/dev/null)" -ge 1 ] 2>/dev/null; then
      echo "$out"
      return 0
    fi
    sleep 1
  done
  echo "$out"
}

say "Building slip-cli"
cargo build --quiet --bin slip-cli

say "Test fixtures"
mkdir -p "$WORK"
printf '\x89PNG\r\n\x1a\n0123456789' > "$WORK/photo.png"
head -c 4096 /dev/urandom > "$WORK/voice.ogg"
printf '%%PDF-1.4 slip e2e test document' > "$WORK/report.pdf"

say "1. Identities exist and differ"
A_FP=$(alice identity | jget "d['fingerprint']")
B_FP=$(bob identity | jget "d['fingerprint']")
[ -n "$A_FP" ] && [ -n "$B_FP" ] && [ "$A_FP" != "$B_FP" ] || fail "identities"
pass "alice=$A_FP bob=$B_FP"

say "2. First message (plaintext; no peer key yet)"
SENT=$(alice chat-send --to bob@slip.test --body "hello bob, first contact")
[ "$(echo "$SENT" | jget "d['sent']['encrypted']")" = "False" ] || fail "first send should be plaintext"
[ "$(echo "$SENT" | jget "d['sent']['status']")" = "sent" ] || fail "send status"
pass "sent plaintext"

say "3. Bob syncs: message arrives, TOFU records alice, mail moves to Slip folder"
SYNC=$(sync_until_saved bob chat-sync)
[ "$(echo "$SYNC" | jget "d['sync']['saved']")" = "1" ] || fail "bob should save 1 message"
[ "$(echo "$SYNC" | jget "d['sync']['relocated']")" -ge 1 ] || fail "mail should move to Slip folder"
[ "$(echo "$SYNC" | jget "d['sync']['burned']")" = "0" ] || fail "default is archive, not burn"
BODY=$(bob chat-resume --email alice@slip.test | jget "d['session']['messages'][0]['body']")
[ "$BODY" = "hello bob, first contact" ] || fail "message body mismatch: $BODY"
PEER_FP=$(bob peers | jget "d['peers']['alice@slip.test']['fingerprint']")
[ "$PEER_FP" = "$A_FP" ] || fail "TOFU fingerprint mismatch"
# Isolation: nothing left in INBOX, the chat is in the Slip folder.
INBOX_LEFT=$(bob search --mailbox INBOX --query 'SUBJECT "[slip/chat]"' | jget "len(d['uids'])")
SLIP_HELD=$(bob search --mailbox Slip --query 'SUBJECT "[slip/chat]"' | jget "len(d['uids'])")
[ "$INBOX_LEFT" = "0" ] || fail "INBOX should be clean, has $INBOX_LEFT"
[ "$SLIP_HELD" -ge 1 ] || fail "Slip folder should hold the chat, has $SLIP_HELD"
pass "received + TOFU ($PEER_FP), isolated in Slip folder"

say "4. Bob's reply is automatically encrypted"
REPLY=$(bob chat-send --to alice@slip.test --body "hi alice, this is secret")
[ "$(echo "$REPLY" | jget "d['sent']['encrypted']")" = "True" ] || fail "reply should be encrypted"
pass "encrypted automatically"

say "5. Alice receives and decrypts; TOFU records bob"
SYNC=$(sync_until_saved alice chat-sync)
[ "$(echo "$SYNC" | jget "d['sync']['saved']")" = "1" ] || fail "alice should save 1"
MSG=$(alice chat-resume --email bob@slip.test)
[ "$(echo "$MSG" | jget "d['session']['messages'][-1]['body']")" = "hi alice, this is secret" ] || fail "decrypt failed"
[ "$(echo "$MSG" | jget "d['session']['messages'][-1]['encrypted']")" = "True" ] || fail "not marked encrypted"
pass "decrypted"

say "6. Multimedia: image + audio + file in one encrypted message"
SENT=$(alice chat-send --to bob@slip.test --body "media test @$WORK/photo.png @$WORK/voice.ogg @$WORK/report.pdf")
[ "$(echo "$SENT" | jget "d['sent']['encrypted']")" = "True" ] || fail "media send should be encrypted now"
SYNC=$(sync_until_saved bob chat-sync)
[ "$(echo "$SYNC" | jget "d['sync']['saved']")" = "1" ] || fail "bob media sync"
LAST=$(bob chat-resume --email alice@slip.test)
KINDS=$(echo "$LAST" | jget "[m['kind'] for m in d['session']['messages'][-1]['media']]")
[ "$KINDS" = "['image', 'audio', 'file']" ] || fail "media kinds: $KINDS"
for f in $(echo "$LAST" | jget "'\n'.join(m['path'] for m in d['session']['messages'][-1]['media'])"); do
  [ -s "$f" ] || fail "media file missing: $f"
done
cmp -s "$WORK/photo.png" "$(echo "$LAST" | jget "d['session']['messages'][-1]['media'][0]['path']")" \
  || fail "decrypted image differs from original"
pass "image/audio/file round trip, bytes identical"

say "6b. Attachment path with spaces and non-ASCII (quoted, as terminals paste)"
SPACED="$WORK/截图 2026-06-24 21-06-11.png"
cp "$WORK/photo.png" "$SPACED"
SENT=$(alice chat-send --to bob@slip.test --body "看这张 @'$SPACED'")
[ "$(echo "$SENT" | jget "len(d['sent']['attachments'])")" = "1" ] || fail "spaced path not attached"
sync_until_saved bob chat-sync >/dev/null
GOTNAME=$(bob chat-resume --email alice@slip.test | jget "d['session']['messages'][-1]['media'][0]['name']")
[ "$GOTNAME" = "截图 2026-06-24 21-06-11.png" ] || fail "wrong media name: $GOTNAME"
pass "quoted path with spaces + 中文 attached and received"

say "7. Push: bob watches (IDLE), alice sends, arrival is immediate"
WATCH_OUT="$WORK/watch.json"
( bob chat-watch --max-events 1 > "$WATCH_OUT" 2>/dev/null ) &
WATCH_PID=$!
sleep 3  # let the watcher connect and enter IDLE
T0=$(date +%s%N)
alice chat-send --to bob@slip.test --body "push me" >/dev/null
if ! timeout 30 tail --pid=$WATCH_PID -f /dev/null 2>/dev/null; then
  kill $WATCH_PID 2>/dev/null || true
  fail "watch did not exit within 30s of the send"
fi
T1=$(date +%s%N)
LATENCY_MS=$(( (T1 - T0) / 1000000 ))
grep -q 'push me' "$WATCH_OUT" || fail "watch output missing the message"
pass "pushed in ${LATENCY_MS}ms after send"

say "8. Key rotation raises an alarm WITHOUT downgrading to plaintext"
# A spoofed/new key must never silently drop the conversation to plaintext:
# Slip keeps encrypting to the last trusted key and only warns.
rm "$WORK/alice/identity.json"
alice chat-send --to bob@slip.test --body "i have a new key" >/dev/null
SYNC=$(sync_until_saved bob chat-sync)
[ "$(echo "$SYNC" | jget "d['sync']['key_changes']")" = "['alice@slip.test']" ] || fail "key change not detected"
INFO=$(bob contact-info --email alice@slip.test)
[ "$(echo "$INFO" | jget "d['contact']['pending_fingerprint'] is not None")" = "True" ] || fail "no pending key"
[ "$(echo "$INFO" | jget "d['contact']['encryption_active']")" = "True" ] || fail "encryption must NOT downgrade on key change"
NEXT=$(bob chat-send --to alice@slip.test --body "careful reply")
[ "$(echo "$NEXT" | jget "d['sent']['encrypted']")" = "True" ] || fail "must keep encrypting to the trusted key"
pass "key change flagged, no plaintext downgrade"

say "9. /trust switches to the new key and a fresh round trip works"
bob trust --email alice@slip.test >/dev/null
INFO=$(bob contact-info --email alice@slip.test)
[ "$(echo "$INFO" | jget "d['contact']['encryption_active']")" = "True" ] || fail "trust broke encryption"
[ "$(echo "$INFO" | jget "d['contact']['pending_fingerprint'] is None")" = "True" ] || fail "pending key not cleared"
NEW_A_FP=$(alice identity | jget "d['fingerprint']")
[ "$(echo "$INFO" | jget "d['contact']['peer_fingerprint']")" = "$NEW_A_FP" ] || fail "trusted wrong key"
# After trust, alice must be able to read bob's encrypted reply again.
bob chat-send --to alice@slip.test --body "trusted you again" >/dev/null
sync_until_saved alice chat-sync >/dev/null
GOT=$(alice chat-resume --email bob@slip.test | jget "d['session']['messages'][-1]['body']")
[ "$GOT" = "trusted you again" ] || fail "alice could not read reply after re-trust: $GOT"
pass "re-trusted new key $NEW_A_FP, round trip restored"

say "10. Inbox stays clean; conversations live in the Slip folder"
INBOX_LEFT=$(bob search --mailbox INBOX --query 'SUBJECT "[slip/chat]"' | jget "len(d['uids'])")
SLIP_HELD=$(bob search --mailbox Slip --query 'SUBJECT "[slip/chat]"' | jget "len(d['uids'])")
[ "$INBOX_LEFT" = "0" ] || fail "bob INBOX still has $INBOX_LEFT chat mails"
[ "$SLIP_HELD" -ge 3 ] || fail "Slip folder should archive the conversation, has $SLIP_HELD"
pass "INBOX clean, Slip folder archives $SLIP_HELD messages"

say "11. --burn deletes from the server after saving locally"
SLIP_BEFORE=$(bob search --mailbox Slip --query 'SUBJECT "[slip/chat]"' | jget "len(d['uids'])")
alice chat-send --to bob@slip.test --body "burn this one" >/dev/null
SYNC=$(sync_until_saved bob chat-sync --burn)
[ "$(echo "$SYNC" | jget "d['sync']['saved']")" -ge 1 ] || fail "burn-mode should still save locally"
[ "$(echo "$SYNC" | jget "d['sync']['burned']")" -ge 1 ] || fail "burn-mode should delete from server"
GOT=$(bob chat-resume --email alice@slip.test | jget "d['session']['messages'][-1]['body']")
[ "$GOT" = "burn this one" ] || fail "burn-mode did not save the message: $GOT"
SLIP_AFTER=$(bob search --mailbox Slip --query 'SUBJECT "[slip/chat]"' | jget "len(d['uids'])")
[ "$SLIP_AFTER" = "$SLIP_BEFORE" ] || fail "burned message should not stay in Slip ($SLIP_BEFORE -> $SLIP_AFTER)"
pass "burn mode: saved locally, deleted from server"

say "12. Failed send is queued and auto-resent (retry queue)"
# Send with a dead SMTP port so the send fails but is saved as Failed.
alice_badsmtp() {
  env "${env_common[@]}" SLIP_SMTP_PORT=1 \
    SLIP_ADDRESS=alice@slip.test SLIP_PASSWORD="$TEST_PASSWORD" SLIP_HOME="$WORK/alice" \
    "${BIN[@]}" "$@"
}
alice_badsmtp chat-send --to bob@slip.test --body "retry me please" >/dev/null 2>&1 || true
# Find the message by body (same-second sends tie-break by id, so not always last).
status_of() { alice chat-resume --email bob@slip.test \
  | jget "[m['status'] for m in d['session']['messages'] if m['body']=='retry me please'][0]"; }
[ "$(status_of)" = "failed" ] || fail "failed send should be status=failed, got $(status_of)"
pass "send failure recorded as failed + queued"
# Now retry-all with the good SMTP: it should resend and flip to sent.
RESENT=$(alice retry-all | jget "d['resent_contacts']")
[ "$RESENT" = "['bob@slip.test']" ] || fail "retry-all should resend to bob, got $RESENT"
[ "$(status_of)" = "sent" ] || fail "message should be sent after retry, got $(status_of)"
sync_until_saved bob chat-sync >/dev/null
GOT=$(bob chat-resume --email alice@slip.test \
  | jget "[m['body'] for m in d['session']['messages'] if m['body']=='retry me please']")
[ "$GOT" = "['retry me please']" ] || fail "bob should receive the auto-resent message: $GOT"
pass "queued message auto-resent and delivered"

say "13. Spam-folder fallback recovers misfiled chat mail"
bob create-mailbox --name Junk --execute >/dev/null
SENT=$(alice chat-send --to bob@slip.test --body "misfiled to junk")
[ "$(echo "$SENT" | jget "d['sent']['status']")" = "sent" ] || fail "step13 send failed: $SENT"
# Wait for delivery, then simulate the provider filing it as spam:
# move it INBOX -> Junk before bob syncs.
JUID=""
for _ in $(seq 1 20); do
  JUID=$(bob search --mailbox INBOX --query 'SUBJECT "[slip/chat]"' | jget "d['uids'][-1]" 2>/dev/null || true)
  [ -n "$JUID" ] && break
  sleep 1
done
[ -n "$JUID" ] || fail "message never arrived in INBOX"
bob move --mailbox INBOX --uid "$JUID" --to Junk --execute >/dev/null
[ "$(bob search --mailbox Junk --query 'SUBJECT "[slip/chat]"' | jget "len(d['uids'])")" -ge 1 ] || fail "message should be in Junk"
# With INBOX empty of it, only the spam sweep can deliver it.
sync_until_saved bob chat-sync >/dev/null
DEL=$(bob chat-resume --email alice@slip.test \
  | jget "[m['body'] for m in d['session']['messages'] if m['body']=='misfiled to junk']")
[ "$DEL" = "['misfiled to junk']" ] || fail "spam sweep should recover the message: $DEL"
[ "$(bob search --mailbox Junk --query 'SUBJECT "[slip/chat]"' | jget "len(d['uids'])")" = "0" ] || fail "Junk should be swept clean"
pass "misfiled spam message recovered into Slip folder"

say "14. Identity key is encrypted at rest under SLIP_PASSPHRASE"
CARL_HOME="$WORK/carl"
carl() {
  env "${env_common[@]}" \
    SLIP_ADDRESS=alice@slip.test SLIP_PASSWORD="$TEST_PASSWORD" SLIP_HOME="$CARL_HOME" \
    SLIP_PASSPHRASE="$TEST_PASSPHRASE" "${BIN[@]}" "$@"
}
CARL_FP=$(carl identity | jget "d['fingerprint']")
[ -n "$CARL_FP" ] || fail "encrypted identity should still yield a fingerprint"
# On disk: no plaintext secret, an encrypted block instead.
HAS_SECRET=$(python3 -c "import json;print('secret' in json.load(open('$CARL_HOME/identity.json')))")
HAS_ENC=$(python3 -c "import json;print('encrypted' in json.load(open('$CARL_HOME/identity.json')))")
[ "$HAS_SECRET" = "False" ] || fail "plaintext secret must not be on disk when encrypted"
[ "$HAS_ENC" = "True" ] || fail "encrypted block must be present"
# Correct passphrase re-opens the same identity.
AGAIN_FP=$(carl identity | jget "d['fingerprint']")
[ "$AGAIN_FP" = "$CARL_FP" ] || fail "correct passphrase must reopen same identity"
# Wrong passphrase is rejected, not silently regenerated.
if env "${env_common[@]}" SLIP_ADDRESS=alice@slip.test SLIP_PASSWORD="$TEST_PASSWORD" \
    SLIP_HOME="$CARL_HOME" SLIP_PASSPHRASE="$WRONG_TEST_PASSPHRASE" \
    "${BIN[@]}" identity >/dev/null 2>&1; then
  fail "wrong passphrase must fail, not open the identity"
fi
pass "identity encrypted at rest ($CARL_FP), wrong passphrase rejected"

say "ALL E2E TESTS PASSED"
echo "work dir: $WORK"
