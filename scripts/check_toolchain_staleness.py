#!/usr/bin/env python3
"""Check whether rust-toolchain.toml is behind latest stable Rust."""

from __future__ import annotations

import argparse
import os
import re
import sys
import tomllib
import urllib.request
from pathlib import Path


STABLE_MANIFEST_URL = "https://static.rust-lang.org/dist/channel-rust-stable.toml"
SEMVER_PREFIX = re.compile(r"^(\d+)\.(\d+)\.(\d+)")


def parse_semver_prefix(value: str) -> tuple[int, int, int] | None:
    match = SEMVER_PREFIX.match(value.strip())
    if match is None:
        return None

    return tuple(int(part) for part in match.groups())


def format_semver(version: tuple[int, int, int]) -> str:
    return ".".join(str(part) for part in version)


def read_pinned_toolchain(path: Path) -> tuple[str, tuple[int, int, int] | None]:
    with path.open("rb") as toolchain_file:
        toolchain = tomllib.load(toolchain_file)

    channel = toolchain.get("toolchain", {}).get("channel")
    if not isinstance(channel, str):
        raise ValueError(f"{path} must define toolchain.channel")

    parsed = parse_semver_prefix(channel)
    if parsed is None:
        return channel, None

    return format_semver(parsed), parsed


def read_latest_stable(manifest_url: str) -> tuple[str, tuple[int, int, int]]:
    with urllib.request.urlopen(manifest_url, timeout=30) as response:
        manifest = tomllib.loads(response.read().decode("utf-8"))

    version = manifest["pkg"]["rust"]["version"]
    parsed = parse_semver_prefix(version)
    if parsed is None:
        raise ValueError(f"stable manifest returned an unparseable Rust version: {version}")

    return format_semver(parsed), parsed


def write_github_outputs(path: str, outputs: dict[str, str]) -> None:
    with open(path, "a", encoding="utf-8") as output_file:
        for key, value in outputs.items():
            output_file.write(f"{key}={value}\n")


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Check whether rust-toolchain.toml lags latest stable Rust."
    )
    parser.add_argument(
        "--toolchain-file",
        default="rust-toolchain.toml",
        type=Path,
        help="Path to rust-toolchain.toml",
    )
    parser.add_argument(
        "--manifest-url",
        default=STABLE_MANIFEST_URL,
        help="Rust stable channel manifest URL",
    )
    parser.add_argument(
        "--github-output",
        default=os.environ.get("GITHUB_OUTPUT"),
        help="Optional GitHub Actions output file",
    )
    args = parser.parse_args()

    pinned_label, pinned_version = read_pinned_toolchain(args.toolchain_file)
    latest_label, latest_version = read_latest_stable(args.manifest_url)
    stale = pinned_version is not None and pinned_version < latest_version

    outputs = {
        "pinned": pinned_label,
        "latest": latest_label,
        "stale": str(stale).lower(),
    }

    if args.github_output:
        write_github_outputs(args.github_output, outputs)

    if pinned_version is None:
        print(
            f"rust-toolchain.toml uses channel {pinned_label!r}; "
            f"latest stable is {latest_label}. Treating this as not stale."
        )
    elif stale:
        print(f"Rust toolchain is stale: pinned {pinned_label}, latest stable {latest_label}.")
    else:
        print(f"Rust toolchain is current: pinned {pinned_label}, latest stable {latest_label}.")

    return 0


if __name__ == "__main__":
    sys.exit(main())
