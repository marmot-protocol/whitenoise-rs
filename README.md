# White Noise

A secure, private, and decentralized chat app built on Nostr, using the MLS protocol under the hood.

## Overview

![Overview](https://blossom.primal.net/c47602a6ea11e092b43fd39482e66c3e3e961cb706a443b89e7396cee025875d.png)

White Noise aims to be the most secure private chat app on Nostr, with a focus on privacy and security. Under the hood, it uses the [Messaging Layer Security](https://www.rfc-editor.org/rfc/rfc9420.html) (MLS) protocol to manage group communications in a highly secure way. Nostr is used as the transport protocol and as the framework for the ongoing conversation in each chat.

This crate is the core library that powers our [Flutter app](https://github.com/marmot-protocol/whitenoise). It is front-end agnostic and will allow for CLI and other interfaces to operate groups in the future.

## Status

![Status](https://blossom.primal.net/8edb242c987646d02bb3723cd92e30d2ac2eca1779306e22dac1b9a6214de531.png)

![CI](https://github.com/marmot-protocol/whitenoise-rs/actions/workflows/ci.yml/badge.svg?event=push)

## The Spec

![The Spec](https://blossom.primal.net/6a3288895d4e67fd23f8e873056bf424f3e63c0f043467e2a406c09cff3e2535.png)

White Noise implements the [Marmot protocol](https://github.com/marmot-protocol/marmot), which brings MLS group messaging to Nostr.

## Releases

![Releases](https://blossom.primal.net/be38014386130840a38503b2876e88d186ca7ce40b9966d8e7f31ef16e842f3e.png)

Coming soon

## Building White Noise Yourself

![Building](https://blossom.primal.net/0ad7960c55f72c48f2609de66973960fafedb6c0fbef8fe1743186fcdcc95e68.png)

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

![Contributing](https://blossom.primal.net/9ee0c0d6fe86b85aab3dc9983155a83a894e42d8bdc292b0cf02cd17abfd5301.png)

To get started contributing you'll need to have the [Rust](https://www.rust-lang.org/tools/install) toolchain installed (version 1.90.0 or later) and [Docker](https://www.docker.com).

1. Clone the repo: `git clone https://github.com/marmot-protocol/whitenoise-rs.git` and `cd whitenoise-rs`.
1. Install recommended development tools: `just install-tools` (optional but recommended)
1. Start the development services (two Nostr relays; nostr-rs-relay and strfry, and a blossom server):
   ```bash
   docker compose up -d
   ```
   Or if using older Docker versions:
   ```bash
   docker-compose up -d
   ```
1. Now you can run the integration tests with `just int-test`.

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
docker compose up -d

# Generate coverage
just coverage        # Generate lcov.info report
just coverage-html   # Generate HTML report

# View HTML report
open target/llvm-cov/html/index.html
```

Coverage is automatically tracked in CI as part of the test suite. PRs that decrease coverage will be blocked.

## License

![License](https://blossom.primal.net/d6c4287303eb9024c83adc9214ff6c44ed705aed2f8370461b85cadf3dd00021.png)

White Noise is free and open source software, released under the Gnu AGPL v3 license. See the [LICENSE](LICENSE) file for details.
