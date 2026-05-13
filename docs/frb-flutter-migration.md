# Flutter Rust Bridge — Generation moved to whitenoise-rs

## What changed

Until recently, the Flutter app
([marmot-protocol/whitenoise](https://github.com/marmot-protocol/whitenoise))
owned both the FFI wrapper Rust crate and the `flutter_rust_bridge_codegen`
runs that produced the Dart bindings. Every Rust API change forced the Flutter
team to re-run codegen and bump the `whitenoise` git rev.

Now `whitenoise-rs` owns the wrapper and the codegen. Master is **Rust-only**;
the Flutter package shape and generated bindings live exclusively on a
dedicated `flutter-package` orphan branch (no shared history with master). CI
regenerates and updates that branch on every codeowner push to `master`. The
Flutter app consumes the orphan branch as a normal git dependency.

```
┌──────────────────────────────────┐  push to master   ┌─────────────────────┐
│ whitenoise-rs/master             │ ────────────────► │ frb-codegen.yml     │
│  crates/whitenoise-frb/  (wrap)  │                   │  - clone orphan     │
│  flutter_rust_bridge.yaml        │                   │  - codegen → orphan │
│  scripts/, workflow              │                   │  - vendor wrapper   │
│  (no flutter-package/)           │                   │  - push orphan      │
└──────────────────────────────────┘                   └─────────────────────┘
                                                                  │
                                                                  ▼
                                                  ┌──────────────────────────┐
                                                  │ whitenoise-rs            │
                                                  │  flutter-package (orphan)│
                                                  │   pubspec, cargokit,     │
                                                  │   lib/src/rust/, rust/   │
                                                  └──────────────────────────┘
                                                                  │
                                                                  ▼
                                                  ┌──────────────────────────┐
                                                  │ marmot-protocol/whitenoise│
                                                  │  pubspec.yaml git dep on │
                                                  │  flutter-package branch  │
                                                  └──────────────────────────┘
```

## Where things live

### On `master` (Rust-only)
| Concern | Path |
|---|---|
| FRB wrapper crate | `crates/whitenoise-frb/` |
| FRB config | `flutter_rust_bridge.yaml` |
| Codegen scripts | `scripts/frb-{stage,generate,check}.sh` |
| CI workflow | `.github/workflows/frb-codegen.yml` |
| Local staging worktree | `.frb-staging/` (gitignored) |

### On `flutter-package` (orphan, no shared history)
| Concern | Path |
|---|---|
| Pubspec | `pubspec.yaml` |
| Cargokit / platform glue | `cargokit/`, `android/`, `ios/`, `linux/`, `macos/`, `windows/` |
| Generated Dart bindings | `lib/src/rust/` |
| Vendored Rust wrapper | `rust/` (with `whitenoise` dep rewritten to a git rev) |
| Provenance marker | `REGENERATED.txt` (writes source SHA, timestamp, FRB version) |

## Local development

```sh
# 1. Set up a worktree of the flutter-package branch as .frb-staging/
just frb-stage

# 2. Run codegen (writes into .frb-staging/lib/src/rust/)
just frb-generate

# 3. Inspect changes
git -C .frb-staging diff
```

`just frb-stage` is safe to re-run; it refreshes `.frb-staging/` from origin
if a worktree already exists. The codegen tool must match the
`flutter_rust_bridge` crate version pinned in `crates/whitenoise-frb/Cargo.toml`
(`2.11.1`):

```sh
cargo install flutter_rust_bridge_codegen --version 2.11.1 --locked
```

To lint the wrapper crate (clippy under `-D warnings`):

```sh
just frb-check
```

## CI workflow

`.github/workflows/frb-codegen.yml`:

1. **Gate job** — verifies `github.actor` is in the codeowner allowlist
   (hardcoded; mirror `.github/CODEOWNERS`). Non-codeowner pushes skip cleanly.
2. **Codegen job**:
   - Checks out master at the pushed SHA into `source/`.
   - Checks out the `flutter-package` orphan branch into `source/.frb-staging/`.
   - Installs Rust 1.90.0, `flutter_rust_bridge_codegen 2.11.1`, Flutter SDK.
   - Runs `bash scripts/frb-generate.sh` (writes Dart into staging).
   - Runs `bash scripts/frb-check.sh` (lints wrapper).
   - Vendors `crates/whitenoise-frb/` into `staging/rust/` with the
     `whitenoise` dep rewritten from `path = "../.."` to a git rev pinning
     the source commit.
   - Refreshes `REGENERATED.txt` with provenance.
   - Commits + pushes the orphan branch only if there are real changes.

Cross-repo authentication uses `secrets.FRB_PR_TOKEN` if set, else
`secrets.GITHUB_TOKEN`. Same-repo orphan-branch pushes work with the default
token.

## One-time bootstrap

The `flutter-package` orphan branch must exist on `origin` before the
workflow can update it. Bootstrap once:

```sh
# 1. Create an empty orphan branch
git worktree add --orphan -b flutter-package /tmp/fp-bootstrap

# 2. Seed it with the package shape (pubspec, cargokit, platform dirs,
#    LICENSE, README, .gitignore, analysis_options) — see the layout under
#    "On flutter-package" above. The simplest source is to copy the files
#    that whitenoise (Flutter app) currently has under rust_builder/, plus a
#    fresh `lib/src/rust/` produced by running codegen.

# 3. Vendor the wrapper crate
cp -r crates/whitenoise-frb /tmp/fp-bootstrap/rust
SHA=$(git rev-parse HEAD)
sed -i "s#whitenoise = { path = \"../..\" }#whitenoise = { git = \"https://github.com/marmot-protocol/whitenoise-rs\", rev = \"${SHA}\" }#" \
    /tmp/fp-bootstrap/rust/Cargo.toml

# 4. Run codegen against the bootstrap
ln -s /tmp/fp-bootstrap .frb-staging
just frb-generate
rm .frb-staging

# 5. Commit and push
git -C /tmp/fp-bootstrap add -A
git -C /tmp/fp-bootstrap -c user.email=frb-codegen-bot@marmot-protocol.org \
    -c user.name="frb-codegen-bot" \
    commit -m "frb: bootstrap flutter-package from ${SHA}"
git -C /tmp/fp-bootstrap push origin flutter-package
```

After step 5, the workflow can take over.

## Rolling out the Flutter side

Once the orphan branch exists, the Flutter team needs a separate PR on
`marmot-protocol/whitenoise` that:

1. **Removes** the in-repo wrapper and its build glue:
   - `rust/` (entire wrapper crate)
   - `rust_builder/` (cargokit + plugin shape)
   - `flutter_rust_bridge.yaml`
   - `lib/src/rust/` (all auto-generated Dart bindings)

2. **Updates `pubspec.yaml`** to consume the orphan branch:

   ```yaml
   dependencies:
     rust_lib_whitenoise:
       git:
         url: https://github.com/marmot-protocol/whitenoise-rs
         ref: flutter-package
   # flutter_rust_bridge no longer needed at top level — comes transitively
   ```

3. **Updates Dart imports** across the app. Two equivalent approaches:

   - **Mass rewrite (recommended for clean end state):**
     ```sh
     find lib test -name "*.dart" -exec \
       sed -i 's#package:whitenoise/src/rust#package:rust_lib_whitenoise/src/rust#g' {} +
     ```
   - **Forwarding shims (smaller diff, leaves indirection):** keep
     `lib/src/rust/*.dart` files but replace each body with
     `export 'package:rust_lib_whitenoise/src/rust/<same-relative-path>';`.

4. **Removes generation recipes from `justfile`**: `generate`, `regenerate`,
   `clean-bridge`, and any `lint-rust` / `test-rust` recipes that operated on
   the deleted `rust/`.

5. **Updates `analysis_options.yaml`** — drop the `rust_builder/**` exclude.

6. **Updates docs** (`AGENTS.md`, `CONTRIBUTING.md`) — remove instructions for
   running `just generate` locally.

## Rollout order (important)

1. Land the whitenoise-rs PR introducing the wrapper + workflow.
2. Bootstrap the orphan branch (one-time, see above).
3. Run the workflow once via `workflow_dispatch` to verify CI updates the
   orphan branch correctly.
4. *Then* land the Flutter cleanup PR. If the Flutter PR lands first,
   `flutter pub get` will fail because the git ref does not resolve.

## Codeowner gating

The CI workflow only regenerates when `github.actor` is in the allowlist
hardcoded in `.github/workflows/frb-codegen.yml`. Keep that list in sync with
`.github/CODEOWNERS`. Non-codeowner pushes to master skip the codegen job
silently — no orphan-branch update.
