# White Noise

A secure, private, and decentralized chat app built on Nostr, using the MLS protocol under the hood.

## Overview

White Noise aims to be the most secure private chat app on Nostr, with a focus on privacy and security. Under the hood, it uses the [Messaging Layer Security](https://www.rfc-editor.org/rfc/rfc9420.html) (MLS) protocol to manage group communications in a highly secure way. Nostr is used as the transport protocol and as the framework for the ongoing conversation in each chat.

This crate is the core library that powers our [Flutter app](https://github.com/marmot-protocol/whitenoise). It is front-end agnostic and will allow for CLI and other interfaces to operate groups in the future.

## Status

![CI](https://github.com/marmot-protocol/whitenoise-rs/actions/workflows/ci.yml/badge.svg?event=push)

## The Spec

White Noise implements the [Marmot protocol](https://github.com/marmot-protocol/marmot), which brings MLS group messaging to Nostr.

## Releases

Coming soon

## Building White Noise Yourself

White Noise is a standard rust crate. Check it out and use it like you would any other crate.

1. Clone the repo: `git clone https://github.com/marmot-protocol/whitenoise-rs.git` and `cd whitenoise-rs`.

In addition, there are extensive integration tests in the codebase that run through much of the API and functions. Run it with the following

```sh
just int-test
```

Check formatting, clippy, and docs with the following:

```sh
just check
```

Check all those things and run tests with `precommit`

```sh
just precommit
```

## Contributing

To get started contributing you'll need to have the [Rust](https://www.rust-lang.org/tools/install) toolchain installed (version 1.90.0 or later) and [Docker](https://www.docker.com).

1. Clone the repo: `git clone https://github.com/marmot-protocol/whitenoise-rs.git` and `cd whitenoise-rs`.
1. Install recommended development tools: `just install-tools` (optional but recommended)
1. Start the development services (two Nostr relays, a Blossom server, and a local Transponder instance):
   ```bash
   just docker-up
   ```
1. Now you can run the integration tests with `just int-test`.

`just docker-up` will auto-generate `dev/transponder/.env` if it is missing, wait for the local
stack to become ready, and put a local-only test key there for the Transponder container. The file
is gitignored and should not be committed.

### Development Tools

We recommend installing additional cargo tools for enhanced development experience:

```sh
just install-tools
```

This installs:
- **cargo-nextest**: Faster parallel test runner
- **cargo-audit**: Security vulnerability scanner
- **cargo-outdated**: Check for outdated dependencies
- **cargo-llvm-cov**: Code coverage reporting
- **cargo-msrv**: Verify minimum Rust version
- **cargo-deny**: License and dependency checker
- **cargo-expand**: Macro expansion tool

See [docs/DEVELOPMENT_TOOLS.md](docs/DEVELOPMENT_TOOLS.md) for detailed usage instructions.

### Formatting, Linting, and Tests

Before submitting PRs, please run the `precommit` command:

```sh
just precommit
```

Or for a quicker check (without integration tests):

```sh
just precommit-quick
```

Available development commands:

```sh
just check           # Run format, clippy, and doc checks
just fmt             # Format code
just test            # Run all tests (uses nextest if available)
just test-cargo      # Force cargo test (slower)
just coverage        # Generate coverage report
just audit           # Security audit
just outdated        # Check for outdated dependencies
just deny-check      # Check licenses and dependencies
```

### Test Coverage

**Prerequisites**: Docker must be running for coverage tests.

```bash
# Start required services
just docker-up

# Generate coverage
just coverage        # Generate lcov.info report
just coverage-html   # Generate HTML report

# View HTML report
open target/llvm-cov/html/index.html
```

Coverage is automatically tracked in CI as part of the test suite. PRs that decrease coverage will be blocked.

## License

White Noise is free and open source software, released under the Gnu AGPL v3 license. See the [LICENSE](LICENSE) file for details.
