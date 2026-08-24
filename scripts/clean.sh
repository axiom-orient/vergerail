#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repository_root"

if [ ! -f Cargo.toml ] || ! grep -q '^name = "vergerail"$' Cargo.toml; then
    echo "refusing to clean outside the Vergerail repository" >&2
    exit 1
fi

cargo clean
rm -rf -- package-check coverage
rm -f -- tarpaulin-report.html lcov.info
find . -path ./.git -prune -o -name .DS_Store -type f -exec rm -f -- {} +
