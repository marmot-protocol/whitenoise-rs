# Marmot Scenario Instructions

This guide explains how to run the end-to-end CLI scenario using:

- `run_marmot_scenario.sh`
- `marmot_conversation.txt`
- a mobile user `npub` as the 3rd member

The scenario creates fresh Alice/Bob identities, sets metadata names, creates a group, waits for mobile acceptance, and sends a large realistic conversation.

## Prerequisites

- You are in the repo root.
- You have `jq` installed.
- You have the mobile account `npub` ready.
- `marmot_conversation.txt` exists in the repo root.

## 1) Build release binaries

Use release mode so default relays are the repo's public defaults (not localhost debug relays).

```bash
cargo build --release --features cli --bin wn --bin wnd
```

## 2) Make script executable

```bash
chmod +x run_marmot_scenario.sh
```

## 3) Run the scenario

### Basic usage

```bash
./run_marmot_scenario.sh <MOBILE_NPUB>
```

### With custom conversation file

```bash
./run_marmot_scenario.sh <MOBILE_NPUB> marmot_conversation.txt
```

### With custom group name

```bash
./run_marmot_scenario.sh <MOBILE_NPUB> marmot_conversation.txt "Marmot Field Ops"
```

Note: the script now waits up to 30 seconds for the daemon to be responsive before creating identities.

## Script arguments

```text
./run_marmot_scenario.sh <MOBILE_NPUB> [CONVO_FILE] [GROUP_NAME]
```

- `MOBILE_NPUB` (required): the mobile user's `npub1...`
- `CONVO_FILE` (optional): defaults to `marmot_conversation.txt`
- `GROUP_NAME` (optional): defaults to `Marmot Load Test`

## Optional environment overrides

You can optionally set:

- `ALICE_NAME` (default: `Alice`)
- `BOB_NAME` (default: `Bob`)
- `GROUP_CREATOR` (`alice` or `bob`, default: `alice`)
- `SEND_DELAY` (default: `0.02`)

Example:

```bash
ALICE_NAME="Alice Marmot" \
BOB_NAME="Bob Marmot" \
GROUP_CREATOR=alice \
SEND_DELAY=0.03 \
./run_marmot_scenario.sh <MOBILE_NPUB> marmot_conversation.txt "Marmot Load Test"
```

## What the script does

1. Starts/checks daemon with `./target/release/wn`.
2. Creates fresh Alice and Bob identities.
3. Publishes key packages for Alice and Bob.
4. Sets profile metadata names for each.
5. Verifies relay defaults for each account.
6. Checks mobile key package availability.
7. Waits until the non-creator local account key package is valid.
8. Creates group (creator is Alice or Bob based on `GROUP_CREATOR`), inviting the other + mobile.
9. Resolves group ID (`GID`).
10. Waits for invite/group visibility on the non-creator account, then accepts invite (with retries).
11. Pauses and waits for you to accept invite on mobile.
12. Verifies members.
13. Sends messages from the conversation file, mapping sender from line prefixes:
   - `Alice: ...` -> Alice account
   - `Bob: ...` -> Bob account
   - any other line -> Alice fallback
12. Prints final run summary (`ALICE`, `BOB`, `GID`, sent/failed counters).

## Mobile acceptance step

When prompted:

```text
Accept invite on mobile for <MOBILE_NPUB>, then press Enter to continue...
```

Accept the invite in the mobile app first, then return to terminal and press Enter.

## Troubleshooting

### `Missing ./target/release/wn`

Run:

```bash
cargo build --release --features cli --bin wn --bin wnd
```

### `daemon started` then `daemon not running`

This usually means the daemon had not finished booting yet (startup race) or failed to launch.

- Re-run the script (it waits for readiness now).
- If it still fails, run daemon manually in a separate terminal:

```bash
./target/release/wnd
```

Then run the scenario script in another terminal.

### `Missing required tool: jq`

Install `jq` and rerun.

### Could not determine group id

Manually inspect groups:

```bash
./target/release/wn --account "<CREATOR_NPUB>" groups list
```

### Send failures on some lines

Transient relay/network issues can happen. Re-run scenario or increase delay:

```bash
SEND_DELAY=0.05 ./run_marmot_scenario.sh <MOBILE_NPUB>
```

### `Error: MDK error: Does not exist` during group creation

This usually means at least one invited member does not yet have a retrievable valid key package.

- The script now publishes local key packages and waits for validity before creating the group.
- If it still appears, wait a little and rerun.
- You can manually verify:

```bash
./target/release/wn --json keys check "<BOB_NPUB>"
./target/release/wn --json keys check "<MOBILE_NPUB>"
```

### `Error: group not found for this account` while accepting invite

This usually means invite propagation is not complete yet on the other account.

- The script now waits for invite/group visibility and retries accept automatically.
- If it still fails, run manually:

```bash
./target/release/wn --account "<OTHER_NPUB>" groups invites
./target/release/wn --account "<OTHER_NPUB>" groups list
./target/release/wn --account "<OTHER_NPUB>" groups accept "<GID>"
```
