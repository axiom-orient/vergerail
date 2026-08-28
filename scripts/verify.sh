#!/bin/sh
set -eu

mode=${1:-}
case "$mode" in
    "") release=0 ;;
    --release) release=1 ;;
    *) echo "usage: scripts/verify.sh [--release]" >&2; exit 2 ;;
esac

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repository_root"

if [ -e .github/workflows ]; then
    echo "GitHub Actions are prohibited: remove .github/workflows" >&2
    exit 1
fi

if find . -path ./.git -prune -o -name .DS_Store -type f -print | grep -q .; then
    echo "repository clutter detected: run scripts/clean.sh" >&2
    exit 1
fi

if grep -R -n -E 'VERGERAIL_CODEX_HOME|VERGERAIL_HOME_OWNER|with_home_owner|ManagedHome' \
    src examples tests docs README.md SECURITY.md; then
    echo "retired dedicated-home surface detected" >&2
    exit 1
fi

if grep -n -E 'VERGERAIL_GUARDIAN_LEGACY_MUTANT|survivor-probe|first-empty-scan' \
    src/native/vergerail_guardian.c; then
    echo "test mutation surface leaked into production guardian" >&2
    exit 1
fi

if [ "$release" -eq 1 ]; then
    if [ -n "${VERGERAIL_IMAGE_ONLY:-}" ]; then
        echo "release verification forbids VERGERAIL_IMAGE_ONLY" >&2
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
    if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
        echo "release external proof requires aarch64 macOS" >&2
        exit 1
    fi
    if [ -z "${VERGERAIL_CODEX_PACKAGE:-}" ] || [ ! -d "${VERGERAIL_CODEX_PACKAGE:-}" ]; then
        echo "release external proof requires VERGERAIL_CODEX_PACKAGE" >&2
        exit 1
    fi
    if [ -z "${VERGERAIL_MODEL:-}" ]; then
        echo "release external proof requires VERGERAIL_MODEL" >&2
        exit 1
    fi
    if [ -z "${VERGERAIL_WORKSPACE:-}" ] || [ ! -d "${VERGERAIL_WORKSPACE:-}" ]; then
        echo "release external proof requires an existing VERGERAIL_WORKSPACE" >&2
        exit 1
    fi
    if [ -z "${VERGERAIL_PERFECTPIXEL_BIN:-}" ] || [ ! -f "${VERGERAIL_PERFECTPIXEL_BIN:-}" ]; then
        echo "release external proof requires an existing VERGERAIL_PERFECTPIXEL_BIN" >&2
        exit 1
    fi
fi

cargo fmt --all -- --check
cargo check --offline --locked --all-targets
# Guardian-backed contract tests start real macOS process boundaries. Keep the
# test harness bounded so the 500 ms request-timeout fixtures measure protocol
# behavior rather than an unbounded process-start storm.
cargo test --offline --locked --all-targets -- --test-threads=2
cargo test --offline --locked --doc
cargo clippy --offline --locked --all-targets -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --offline --locked --no-deps
cargo deny check
(cd protocol/codex-0.150.1 && shasum -a 256 -c SHA256SUMS)
if [ "$release" -eq 1 ]; then
    cargo package --offline --locked
else
    cargo package --offline --locked --allow-dirty
fi

if [ -n "${VERGERAIL_CODEX_PACKAGE:-}" ]; then
    cargo test --offline --locked --test official_runtime -- --ignored --nocapture
fi

if [ "$release" -eq 1 ]; then
    cargo test --offline --locked --test managed_runtime -- --ignored --nocapture
    cargo run --offline --locked --example live_e2e
fi
