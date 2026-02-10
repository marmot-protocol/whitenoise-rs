# AGENTS.md

Coding conventions, development workflows, and project context for AI agents and contributors working on the White Noise codebase.

## Project Overview

White Noise is a **secure messenger** built on the [MLS](https://www.rfc-editor.org/rfc/rfc9420.html) (Messaging Layer Security) protocol and [Nostr](https://github.com/nostr-protocol/nostr). It implements the [Marmot protocol](https://github.com/marmot-protocol/marmot) to bring MLS group messaging to Nostr.

This crate (`whitenoise`) is a **library** (`cdylib` + `rlib`) that powers a [Flutter app](https://github.com/marmot-protocol/whitenoise). It is front-end agnostic and exposes a public API for any consumer.

- **Rust edition**: 2024
- **MSRV**: 1.90.0 (pinned in `rust-toolchain.toml`)
- **Async runtime**: Tokio (full features)

## Development Commands

**Use `just` recipes for ALL development tasks.** Run `just` with no arguments to see every available recipe.

### Why not run cargo directly?

The `just` recipes pass specific flags (`--all-features`, `--all-targets`, `-D warnings`, etc.) and set up environment variables. Running `cargo clippy`, `cargo fmt`, or `cargo test` directly will produce different results and miss feature-gated code.

### Core Workflow

| Task | Command |
|------|---------|
| **List all recipes** | `just` |
| Apply formatting | `just fmt` |
| Formatting check | `just check-fmt` |
| Clippy check | `just check-clippy` |
| Fix clippy issues | `just fix-clippy` |
| Run unit tests | `just test` |
| Run all checks (clippy, fmt, docs) | `just check` |
| Quick pre-commit (checks + unit tests) | `just precommit-quick` |
| Full pre-commit (checks + unit tests + integration) | `just precommit` |

### Quiet vs Verbose Commands

The precommit commands default to **quiet mode** which shows minimal pass/fail output per step. This is optimized for AI agents and CI where only failures need detail.

- `just precommit` — quiet mode, full checks including integration tests
- `just precommit-quick` — quiet mode, skip integration tests
- `just precommit-verbose` — verbose mode, full checks including integration tests
- `just precommit-quick-verbose` — verbose mode, skip integration tests

**Agents should use `just precommit-quick` (or `just precommit` if Docker is running).** On success the output looks like:

```text
fmt...                   ✓
docs...                  ✓
clippy...                ✓
tests...                 ✓
PRECOMMIT PASSED
```

On failure, the failing step prints its full output for diagnosis before exiting.

### Integration Tests (require Docker)

Docker Compose services **must** be running before running integration tests:

```sh
just docker-up      # Start Docker services (two Nostr relays + Blossom media server)
just int-test       # Run all integration test scenarios
just int-test basic-messaging   # Run a specific scenario
just docker-down    # Stop Docker services
just docker-logs    # View Docker service logs
```

### Performance Benchmarks (require Docker, not run in CI)

```sh
just benchmark                         # Run all benchmarks
just benchmark messaging-performance   # Run specific benchmark
just benchmark-save                    # Run and save results with timestamp
```

### Other Useful Recipes

| Task | Command |
|------|---------|
| Build release binary | `just build-release` |
| Generate & open docs | `just doc` |
| Code coverage (lcov) | `just coverage` |
| Code coverage (HTML) | `just coverage-html` |
| Security audit | `just audit` |
| Check outdated deps | `just outdated` |
| Update dependencies | `just update` |
| License/security check | `just deny-check` |
| Install dev tools | `just install-tools` |
| Clean build + data | `just clean` |
| Clear local dev data only | `just clear-dev-data` |

## Codebase Architecture

```
src/
  lib.rs                    # Crate root, re-exports public API, tracing init
  bin/
    integration_test.rs     # Integration test binary (feature-gated)
    benchmark_test.rs       # Benchmark binary (feature-gated)
  whitenoise/               # Core application logic
    mod.rs                  # Whitenoise struct (global singleton), initialization
    accounts.rs             # Account management (create, login, logout)
    accounts_groups.rs      # Account-group associations
    aggregated_message.rs   # Aggregated message types
    app_settings.rs         # Application settings (theme, language)
    chat_list.rs            # Chat list logic and sorting
    chat_list_streaming/    # Real-time chat list updates (broadcast channels)
    database/               # SQLite database layer (sqlx)
    error.rs                # Central WhitenoiseError enum
    event_processor/        # Nostr event processing pipeline
      event_handlers/       # Per-event-kind handlers (giftwrap, MLS, metadata, etc.)
    event_tracker.rs        # Event deduplication tracking
    follows.rs              # Follow/contact list management
    group_information.rs    # Group metadata
    groups.rs               # MLS group operations (create, join, add/remove members)
    key_packages.rs         # MLS key package management
    media_files.rs          # Media orchestration layer (coordinates DB + storage + Blossom)
    message_aggregator/     # Message aggregation, reactions, emoji processing
    message_streaming/      # Real-time message updates (broadcast channels)
    messages.rs             # Message handling (send, delete, reactions)
    notification_streaming/ # Real-time notification updates
    relays.rs               # Relay management and defaults
    scheduled_tasks/        # Background task scheduler (key package maintenance)
    secrets_store.rs        # Platform keychain integration for secret key material
    storage/                # Filesystem storage layer (media cache)
    users.rs                # User discovery and metadata
    utils.rs                # Shared utilities
  nostr_manager/            # Nostr protocol layer
    mod.rs                  # NostrManager struct, relay connections
    parser.rs               # Event content parsing (tokenization)
    publisher.rs            # Event publishing to relays
    query.rs                # Querying relays for events
    subscriptions.rs        # Relay subscription management
    utils.rs                # Nostr utilities
  integration_tests/        # Integration test framework (feature-gated)
    README.md               # Detailed test framework documentation
    core/                   # Test infrastructure (context, traits, retry)
    scenarios/              # High-level test workflows
    test_cases/             # Atomic reusable test operations
    benchmarks/             # Performance benchmarks

db_migrations/              # SQLite migrations (sequential numbering: 0001_, 0002_, ...)
docs/                       # Project documentation
  mls/                      # MLS protocol docs and RFCs
  DEVELOPMENT_TOOLS.md      # Dev tool installation and usage
  EXTERNAL-RESOURCES.md     # External links and references
  SECURITY.md               # Security audit notes and policy
  storage-architecture.md   # Media storage architecture
scripts/                    # Shell scripts used by just recipes
dev/                        # Docker configs and local dev data
```

### Key Design Patterns

- **Global singleton**: `Whitenoise` is initialized once via `Whitenoise::initialize_whitenoise()` and accessed via `Whitenoise::get_instance()`. It owns the database, Nostr client, storage, and all managers.
- **Event-driven processing**: Nostr events flow through `event_sender` channel to `event_processor`, which dispatches to event-specific handlers.
- **Streaming updates**: Chat list, messages, and notifications use `tokio::sync::broadcast` channels for real-time updates to consumers.
- **Feature flags**: `integration-tests` enables the integration test module and mock keyring. `benchmark-tests` extends that with performance benchmarks.

### Key Dependencies

| Crate | Role |
|-------|------|
| `nostr-sdk` | Nostr protocol client (events, relays, signing) |
| `mdk-core` | MLS protocol implementation (Marmot Development Kit) |
| `mdk-sqlite-storage` | MLS state persistence in SQLite |
| `sqlx` | Database (SQLite with migrations) |
| `nostr-blossom` | Blossom media server client |
| `tokio` | Async runtime |
| `thiserror` | Error type derivation |
| `tracing` | Structured logging |
| `keyring` | Platform-native secret storage |
| `chacha20poly1305` | Media file encryption |

## Rust Code Style

All Rust code style conventions are documented in [`STYLE.md`](STYLE.md). Read and follow that file when writing or reviewing Rust code. Key points:

- **Trait bounds**: Always use `where` clauses, never inline bounds
- **`Self`**: Use `Self` instead of the type name in impl blocks
- **Logging**: Always write `tracing::warn!(...)` with full path, never `use tracing::warn`
- **Logging targets**: Use `target: "whitenoise::module_name"` to match the module path
- **Imports**: Place at top of scope, never inside functions. Order: std, external crates, `mod` declarations, `crate::`/`super::`/`self::`
- **String conversion**: Prefer `.to_string()` or `.to_owned()` over `.into()` or `String::from()`
- **Submodules**: Use `mod x;` with a separate file, not `mod x { ... }` (except `tests` and `benches`)
- **`if let` vs `match`**: Use `match` when both arms have logic; use `if let` only when the else arm is empty
- **Line width**: 100 characters (configured in `rustfmt.toml`)

## Error Handling

Errors are centralized in `src/whitenoise/error.rs` as the `WhitenoiseError` enum using `thiserror`. When adding new error cases:

1. Add a new variant to `WhitenoiseError` in `error.rs`
2. Use `#[from]` for automatic conversion from library error types
3. Use descriptive `#[error("...")]` messages
4. The crate defines `pub type Result<T> = core::result::Result<T, WhitenoiseError>;`

## Database

- **Engine**: SQLite via `sqlx`
- **Migrations**: Located in `db_migrations/`, named with sequential prefix (`0001_`, `0002_`, ...). Migrations run automatically on database initialization.
- **Adding a migration**: Create a new `.sql` file with the next sequential number. The migration will run automatically the next time the database is opened.
- **NEVER modify an existing migration file**: SQLx computes a SHA-384 checksum of the entire file contents (including comments and whitespace) when a migration is first applied, and stores it in the `_sqlx_migrations` table. On every subsequent run, it compares the file's current checksum against the stored one. Any change — even fixing a typo in a comment or updating a URL — will cause a `VersionMismatch` error and break every database that already applied that migration. If you need to correct something, create a new migration instead.
- **Pattern**: Each domain has a corresponding file in `src/whitenoise/database/` (e.g., `database/accounts.rs`, `database/users.rs`) that contains all SQL operations for that domain.

## Integration Tests

There is a file at `src/bin/integration_test.rs` that performs a full integration test across multiple features. This integration test should **always** be run with `just int-test` and should not be run on its own with different log and data directories. The `int-test` recipe also enables the `integration-tests` Cargo feature.

**Prerequisites**: Docker services must be running (`just docker-up`).

The integration test framework uses a **Scenarios + TestCases** pattern:
- **Scenarios** (`src/integration_tests/scenarios/`): Orchestrate multiple test cases into complete workflows
- **TestCases** (`src/integration_tests/test_cases/`): Atomic, reusable test operations
- **Shared TestCases** (`src/integration_tests/test_cases/shared/`): Cross-scenario reusable operations (create accounts, create groups, etc.)

See `src/integration_tests/README.md` for the full framework documentation, including how to add new scenarios and test cases.

## CI Pipeline

CI runs on push to `master` and on all PRs. The pipeline includes:

1. **Format check** (`cargo fmt --check`)
2. **Clippy** (`cargo clippy --all-targets --all-features --no-deps -- -D warnings`)
3. **Documentation** (`cargo doc --no-deps --all-features` with `-D warnings`)
4. **Security audit** (via `rustsec/audit-check`)
5. **Tests** on Linux (with Docker for integration tests + code coverage), macOS-latest, and macOS-14
6. **Coverage regression check** (PRs that decrease coverage are blocked)
7. **Nightly Rust check** (informational only, allowed to fail)

Before submitting work, run at minimum `just precommit-quick` (or `just precommit` if Docker is running). Both commands use quiet mode by default; add `-verbose` suffix for full output.

## MLS Protocol Documentation and Resources

This project implements the MLS (Messaging Layer Security) protocol on top of Nostr via the Marmot protocol. Below are key resources and documentation to reference when working on MLS-related code.

### Official MLS Specifications

- **RFC 9420**: The Messaging Layer Security (MLS) Protocol
  - Official IETF specification: https://www.rfc-editor.org/rfc/rfc9420.html
  - Local copy: `docs/mls/rfc9420.txt`

- **RFC 9750**: MLS Architecture
  - Official Architecture Overview: https://www.rfc-editor.org/rfc/rfc9750.html
  - Local copy: `docs/mls/rfc9750.txt`

### Marmot Protocol Integration

- **Marmot Protocol Docs**: The current protocol spec describing how to integrate MLS messages with Nostr.
  - Official spec/repo: https://github.com/marmot-protocol/marmot
  - Local implementation notes: `docs/mls/marmot-implementation.md`

- **MDK (Marmot Development Kit)**: The Rust library implementing the Marmot protocol.
  - Repository: https://github.com/marmot-protocol/mdk
  - Integration notes: `docs/mls/mdk-integration.md`

### Security Considerations

When working with MLS code, always consider:
1. Forward secrecy guarantees
2. Post-compromise security
3. Group membership authentication
4. Message ordering and consistency
5. Key rotation procedures
