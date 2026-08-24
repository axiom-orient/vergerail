#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repository_root"

if [ -e .github/workflows ]; then
    echo "GitHub Actions are prohibited: remove .github/workflows" >&2
    exit 1
fi

if ! git rev-parse --verify HEAD >/dev/null 2>&1; then
    echo "release verification requires a committed HEAD" >&2
    exit 1
fi

if [ -n "$(git status --porcelain=v1)" ]; then
    echo "release verification requires a clean checkout" >&2
    git status --short >&2
    exit 1
fi

# A release candidate must be externally exercised on the supported host.  The
# package and dedicated authenticated home are supplied by the release owner;
# this entrypoint never reads or copies credentials.
if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
    echo "release external proof requires aarch64 macOS" >&2
    exit 1
fi
if [ -z "${VERGERAIL_CODEX_PACKAGE:-}" ] || [ ! -d "${VERGERAIL_CODEX_PACKAGE:-}" ]; then
    echo "release external proof requires VERGERAIL_CODEX_PACKAGE" >&2
    exit 1
fi
if [ -z "${VERGERAIL_CODEX_HOME:-}" ] || [ ! -d "${VERGERAIL_CODEX_HOME:-}" ]; then
    echo "release external proof requires an existing VERGERAIL_CODEX_HOME" >&2
    exit 1
fi
if [ -n "${HOME:-}" ] && case "$VERGERAIL_CODEX_HOME" in
    "$HOME/.codex"|"$HOME/.codex/"*) true ;;
    *) false ;;
esac
then
    echo "release external proof forbids the general ~/.codex home" >&2
    exit 1
fi
if [ -z "${VERGERAIL_HOME_OWNER:-}" ] || [ -z "${VERGERAIL_MODEL:-}" ]; then
    echo "release external proof requires VERGERAIL_HOME_OWNER and VERGERAIL_MODEL" >&2
    exit 1
fi
if [ -z "${VERGERAIL_WORKSPACE:-}" ] || [ ! -d "${VERGERAIL_WORKSPACE:-}" ]; then
    echo "release external proof requires an existing VERGERAIL_WORKSPACE" >&2
    exit 1
fi

scripts/verify.sh

# The canonical script covers the official package handshake (two tests) and
# IFSC signed-out path.  Release mode additionally proves managed download /
# reuse and the authenticated end-to-end flow against this exact checkout.
cargo test --offline --locked --test managed_runtime -- --ignored --nocapture
cargo run --offline --locked --example live_e2e

# This is intentionally a second package invocation without --allow-dirty.
# It proves the exact committed tree is the package input for a source release.
cargo package --offline --locked
