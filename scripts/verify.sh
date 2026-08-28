#!/bin/sh
set -eu

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

if grep -R -n -E 'codex_0_147_0_macos_aarch64|GPT_5_6_LUNA|read_paths|write_paths' \
    src examples tests; then
    echo "retired first-party surface detected" >&2
    exit 1
fi

if grep -n -E 'VERGERAIL_GUARDIAN_LEGACY_MUTANT|legacy-ack|first-empty-scan' \
    src/native/vergerail_guardian.c; then
    echo "test mutation surface leaked into production guardian" >&2
    exit 1
fi

if ! VERGERAIL_IMAGE_ONLY=1 scripts/release-verify.sh 2>&1 \
    | grep -q '^release verification forbids VERGERAIL_IMAGE_ONLY$'; then
    echo "release verification image-only bypass guard is missing" >&2
    exit 1
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
cargo package --offline --locked --allow-dirty

if [ -n "${VERGERAIL_CODEX_PACKAGE:-}" ]; then
    cargo test --offline --locked --test official_runtime -- --ignored --nocapture
    cargo test --offline --locked --test ifsc_text_provider_protocol \
        official_runtime_signed_out_path_is_typed_and_clean -- --ignored --nocapture
    cargo test --offline --locked --test vergerail_provider_protocol \
        official_runtime_signed_out_path_is_typed_and_clean -- --ignored --nocapture
fi
