# White Noise — FFI bindings for Flutter

**Auto-generated. Do not edit by hand.**

This package contains the Dart bindings, cargokit build glue, and a vendored
snapshot of the Rust wrapper crate that produces them. It is the consumable
surface of [`whitenoise-rs`][src] for Flutter apps.

## Source of truth

This package is regenerated and published from
[`marmot-protocol/whitenoise-rs`][src] on every codeowner push to `master`.

- Rust wrapper canonical source: `crates/whitenoise-frb/`
- Package template: `crates/whitenoise-frb/template/`
- Dart bindings: regenerated from the wrapper via
  [flutter_rust_bridge][frb] `2.11.1`

Each published tarball is named after the source SHA it was built from.

## Consuming this package

In your Flutter app's `pubspec.yaml`, pin to a specific published SHA:

```yaml
dependencies:
  whitenoise_frb:
    hosted: https://marmot-protocol.github.io/whitenoise-rs
    version: "0.0.0-dev+<short-sha>"
```

Available versions are listed at the [registry index][index].

[src]: https://github.com/marmot-protocol/whitenoise-rs
[frb]: https://github.com/fzyzcjy/flutter_rust_bridge
[index]: https://marmot-protocol.github.io/whitenoise-rs/api/packages/whitenoise_frb
