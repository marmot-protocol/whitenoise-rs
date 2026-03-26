#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: $0 <MOBILE_NPUB> [CONVO_FILE] [GROUP_NAME]"
  echo "Example: $0 npub1... marmot_conversation.txt \"Marmot Load Test\""
}

if [[ $# -lt 1 ]]; then
  usage
  exit 1
fi

MOBILE_NPUB="$1"
CONVO_FILE="${2:-marmot_conversation.txt}"
GROUP_NAME_ARG="${3:-}"

WN="./target/release/wn"
GROUP_NAME="${GROUP_NAME_ARG:-${GROUP_NAME:-Marmot Load Test}}"
SEND_DELAY="${SEND_DELAY:-0.02}"
ALICE_NAME="${ALICE_NAME:-Alice}"
BOB_NAME="${BOB_NAME:-Bob}"
GROUP_CREATOR="${GROUP_CREATOR:-alice}" # alice|bob

if [[ ! -x "$WN" ]]; then
  echo "Missing $WN"
  echo "Build first:"
  echo "  cargo build --release --features cli --bin wn --bin wnd"
  exit 1
fi

if [[ ! -f "$CONVO_FILE" ]]; then
  echo "Conversation file not found: $CONVO_FILE"
  exit 1
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "Missing required tool: jq"
  exit 1
fi

wait_for_key_package() {
  local target_pubkey="$1"
  local timeout_seconds="${2:-60}"
  local start_ts now status

  start_ts="$(date +%s)"
  while true; do
    status="$($WN --json keys check "$target_pubkey" 2>/dev/null | jq -r '.result.status // empty')"
    if [[ "$status" == "valid" ]]; then
      return 0
    fi

    now="$(date +%s)"
    if (( now - start_ts >= timeout_seconds )); then
      echo "Timed out waiting for valid key package for: $target_pubkey"
      return 1
    fi
    sleep 2
  done
}

wait_for_group_visibility() {
  local account_pubkey="$1"
  local group_id="$2"
  local timeout_seconds="${3:-120}"
  local start_ts now output

  start_ts="$(date +%s)"
  while true; do
    output="$($WN --account "$account_pubkey" groups invites 2>/dev/null || true)"
    if printf "%s\n" "$output" | rg -q "$group_id"; then
      return 0
    fi

    output="$($WN --account "$account_pubkey" groups list 2>/dev/null || true)"
    if printf "%s\n" "$output" | rg -q "$group_id"; then
      return 0
    fi

    now="$(date +%s)"
    if (( now - start_ts >= timeout_seconds )); then
      echo "Timed out waiting for group/invite visibility for account: $account_pubkey"
      return 1
    fi
    sleep 2
  done
}

echo "== 1) Ensure daemon is running (release mode) =="
$WN daemon start || true
$WN daemon status

echo "Waiting for daemon readiness..."
ready=0
for _ in $(seq 1 30); do
  if $WN --json whoami >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 1
done

if [[ "$ready" -ne 1 ]]; then
  echo "Daemon did not become ready within 30s."
  echo "Try starting it manually in another terminal:"
  echo "  ./target/release/wnd"
  echo "Then rerun this script."
  exit 1
fi

echo
echo "== 2) Create identities (Alice + Bob) =="
ALICE="$($WN --json create-identity | jq -r '.result.pubkey')"
BOB="$($WN --json create-identity | jq -r '.result.pubkey')"
echo "ALICE=$ALICE"
echo "BOB=$BOB"

echo
echo "Publishing local key packages for Alice and Bob..."
$WN --account "$ALICE" keys publish >/dev/null
$WN --account "$BOB" keys publish >/dev/null

echo
echo "== 3) Set profile metadata names =="
$WN --account "$ALICE" profile update --name "$ALICE_NAME" --display-name "$ALICE_NAME"
$WN --account "$BOB" profile update --name "$BOB_NAME" --display-name "$BOB_NAME"

echo
echo "== 4) Verify relay defaults for both accounts =="
echo "--- ALICE relays ---"
$WN --account "$ALICE" relays list
echo "--- BOB relays ---"
$WN --account "$BOB" relays list

echo
if [[ "$GROUP_CREATOR" == "alice" ]]; then
  CREATOR_ACC="$ALICE"
  CREATOR_LABEL="Alice"
  OTHER_ACC="$BOB"
  OTHER_LABEL="Bob"
elif [[ "$GROUP_CREATOR" == "bob" ]]; then
  CREATOR_ACC="$BOB"
  CREATOR_LABEL="Bob"
  OTHER_ACC="$ALICE"
  OTHER_LABEL="Alice"
else
  echo "Invalid GROUP_CREATOR=$GROUP_CREATOR (use alice or bob)"
  exit 1
fi

echo "Group creator: $CREATOR_LABEL"

echo
echo "== 5) Check mobile key package availability =="
$WN --account "$CREATOR_ACC" keys check "$MOBILE_NPUB"

echo
echo "Waiting for valid key package for ${OTHER_LABEL}..."
wait_for_key_package "$OTHER_ACC" 90

echo
echo "== 6) Create group and invite $OTHER_LABEL + mobile =="
$WN --account "$CREATOR_ACC" groups create "$GROUP_NAME" "$OTHER_ACC" "$MOBILE_NPUB"

echo
echo "== 7) Resolve group id =="
GID="$($WN --account "$CREATOR_ACC" groups list | awk '/mls_group_id:/ {print $2; exit}')"
if [[ -z "$GID" ]]; then
  echo "Could not determine group id from groups list."
  echo "Run manually: $WN --account \"$CREATOR_ACC\" groups list"
  exit 1
fi
echo "GID=$GID"

echo
echo "== 8) ${OTHER_LABEL} accepts invite =="
echo "Waiting for invite/group visibility on ${OTHER_LABEL} account..."
wait_for_group_visibility "$OTHER_ACC" "$GID" 180

accepted=0
for _ in $(seq 1 15); do
  if $WN --account "$OTHER_ACC" groups accept "$GID" >/dev/null 2>&1; then
    accepted=1
    break
  fi
  sleep 2
done

if [[ "$accepted" -ne 1 ]]; then
  echo "Failed to accept invite for ${OTHER_LABEL} after retries."
  echo "Try manually:"
  echo "  $WN --account \"$OTHER_ACC\" groups invites"
  echo "  $WN --account \"$OTHER_ACC\" groups accept \"$GID\""
  exit 1
fi

echo
echo "== 9) WAIT for mobile acceptance =="
read -r -p "Accept invite on mobile for ${MOBILE_NPUB}, then press Enter to continue..."

echo
echo "== 10) Verify group members =="
$WN --account "$CREATOR_ACC" groups members "$GID"

echo
echo "== 11) Send scenario messages from file =="
sent=0
failed=0
line_no=0

while IFS= read -r line || [[ -n "$line" ]]; do
  line_no=$((line_no + 1))
  [[ -z "$line" ]] && continue

  if [[ "$line" == Alice:* ]]; then
    ACC="$ALICE"
    MSG="${line#Alice: }"
  elif [[ "$line" == Bob:* ]]; then
    ACC="$BOB"
    MSG="${line#Bob: }"
  else
    ACC="$ALICE"
    MSG="$line"
  fi

  if $WN --account "$ACC" messages send "$GID" "$MSG" >/dev/null 2>&1; then
    sent=$((sent + 1))
  else
    failed=$((failed + 1))
    echo "failed line $line_no"
  fi

  sleep "$SEND_DELAY"
done < "$CONVO_FILE"

echo
echo "== DONE =="
echo "ALICE=$ALICE"
echo "BOB=$BOB"
echo "GID=$GID"
echo "CONVO_FILE=$CONVO_FILE"
echo "sent=$sent failed=$failed total_lines_read=$line_no"
