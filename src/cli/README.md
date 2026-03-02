# Whitenoise CLI

A Unix-style CLI for Whitenoise, split into a long-running daemon (`wnd`) and a thin client (`wn`).

## Architecture

```txt
wn (client)                         wnd (daemon)
+-----------+                       +-------------------+
| clap args |  -- JSON over Unix    | Whitenoise        |
| account   |     domain socket --> | singleton         |
| resolve   |                       | (Nostr, MLS, DB)  |
| output    |  <-- JSON response -- | dispatch          |
+-----------+                       +-------------------+
```

**`wnd`** owns the `Whitenoise` singleton — database, Nostr client, MLS state, relay subscriptions. It listens on a Unix domain socket at `{data_dir}/dev/wnd.sock` (debug) or `{data_dir}/release/wnd.sock`.

**`wn`** is stateless. It connects to the socket, sends one JSON request, reads the response, and exits. For streaming commands it reads multiple response lines until `stream_end: true` or Ctrl+C.

**IPC protocol:** Newline-delimited JSON. Request is a serde-tagged enum (`{"method": "...", "params": {...}}`). Response has `result`, `error`, and `stream_end` fields.

## Commands

### Identity & Auth

| Command                 | Description                              |
| ----------------------- | ---------------------------------------- |
| `wn create-identity`    | Create a new Nostr keypair               |
| `wn login`              | Log in with an nsec (interactive prompt) |
| `wn logout <npub>`      | Log out an account                       |
| `wn whoami`             | Show logged-in accounts                  |
| `wn export-nsec <npub>` | Export the secret key                    |

### Groups

| Command                                    | Description          |
| ------------------------------------------ | -------------------- |
| `wn groups list`                           | List visible groups  |
| `wn groups create <name> [members...]`     | Create a group       |
| `wn groups show <group-id>`                | Show group details   |
| `wn groups members <group-id>`             | List members         |
| `wn groups admins <group-id>`              | List admins          |
| `wn groups add-members <id> <npubs...>`    | Add members          |
| `wn groups remove-members <id> <npubs...>` | Remove members       |
| `wn groups leave <group-id>`               | Leave a group        |
| `wn groups rename <id> <name>`             | Rename a group       |
| `wn groups invites`                        | List pending invites |
| `wn groups accept <group-id>`              | Accept an invite     |
| `wn groups decline <group-id>`             | Decline an invite    |

### Messages

| Command                              | Description              |
| ------------------------------------ | ------------------------ |
| `wn messages list <group-id>`        | List messages in a group |
| `wn messages send <group-id> <text>` | Send a message           |
| `wn messages subscribe <group-id>`   | Stream live messages     |

### Chats

| Command              | Description                          |
| -------------------- | ------------------------------------ |
| `wn chats list`      | List chats with last message preview |
| `wn chats subscribe` | Stream chat list updates             |

### Follows

| Command                    | Description         |
| -------------------------- | ------------------- |
| `wn follows list`          | List followed users |
| `wn follows add <npub>`    | Follow a user       |
| `wn follows remove <npub>` | Unfollow a user     |
| `wn follows check <npub>`  | Check if following  |

### Users

| Command                   | Description                                  |
| ------------------------- | -------------------------------------------- |
| `wn users show <npub>`    | Show a user's profile                        |
| `wn users search <query>` | Search users by name/description (streaming) |

### Profile, Relays, Settings

| Command                                        | Description                    |
| ---------------------------------------------- | ------------------------------ |
| `wn profile show`                              | Show account metadata          |
| `wn profile update [--name ...] [--about ...]` | Update profile fields          |
| `wn relays list`                               | Show relay connection statuses |
| `wn settings show`                             | Show current settings          |
| `wn settings theme <light\|dark\|system>`      | Set theme                      |
| `wn settings language <en\|es\|fr\|...>`       | Set language                   |

### Notifications & Daemon

| Command                      | Description                   |
| ---------------------------- | ----------------------------- |
| `wn notifications subscribe` | Stream notifications          |
| `wn daemon start`            | Start the daemon (foreground) |
| `wn daemon stop`             | Stop the daemon               |
| `wn daemon status`           | Check daemon status           |

### Global Flags

- `--json` — Machine-readable JSON output (all commands)
- `--account <npub>` — Specify account (or set `WN_ACCOUNT` env var)
- `--socket <path>` — Override daemon socket path
- `--version` — Print version

## File Structure

```
src/cli/
  mod.rs            Module root
  protocol.rs       Request/Response types (serde-tagged enum)
  config.rs         Platform-specific paths, config resolution
  server.rs         Unix socket listener, connection handling (wnd)
  client.rs         Unix socket client (wn)
  dispatch.rs       Request -> Whitenoise method routing
  output.rs         Human-readable + JSON formatting
  account.rs        Account resolution (--account / WN_ACCOUNT / auto)
  commands/
    mod.rs          Command module exports
    identity.rs     create-identity, login, logout, whoami, export-nsec
    daemon.rs       daemon start/stop/status
    accounts.rs     accounts list
    groups.rs       groups list/create/show/members/admins/...
    messages.rs     messages list/send/subscribe
    chats.rs        chats list/subscribe
    follows.rs      follows list/add/remove/check
    profile.rs      profile show/update
    users.rs        users show/search
    relays.rs       relays list
    settings.rs     settings show/theme/language
    notifications.rs  notifications subscribe

src/bin/
  wnd.rs            Daemon entry point
  wn.rs             CLI client entry point
```

## Adding a New Command

1. **Protocol** (`protocol.rs`): Add a `Request` variant with `#[serde(rename = "snake_case")]`. Add a serde roundtrip test.

2. **Dispatch** (`dispatch.rs`): Add a match arm in `dispatch()` (request-reply) or `dispatch_streaming()` (streaming). Handler pattern: `find_account` -> call Whitenoise method -> `to_response`.

3. **Command** (`commands/<domain>.rs`): Add a clap `Subcommand` variant. Handler: resolve account -> build `Request` -> `client::send` (or `client::stream`) -> `output::print_and_exit`.

4. **Wire up** (`src/bin/wn.rs` + `commands/mod.rs`): Add to `Cmd` enum and match arm.

## Tests

94 unit tests across the CLI modules:

| Module | Tests | Coverage |
|--------|------:|----------|
| `protocol.rs` | 45 | Serde roundtrip for every Request variant |
| `output.rs` | 28 | Human-readable formatting, field hiding, npub conversion |
| `server.rs` | 9 | Stale socket cleanup, PID file parsing |
| `account.rs` | 6 | Account resolution logic |
| `client.rs` | 3 | Client-server roundtrip on temp socket |
| `config.rs` | 3 | Platform defaults, CLI overrides, socket path |

E2E tests live in `tests/cli_e2e.rs` (requires `--features cli,integration-tests` + local relays).
