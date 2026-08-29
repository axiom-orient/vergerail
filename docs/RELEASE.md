# Release workflow

Vergerail releases source from `main` and publishes immutable platform binaries as
GitHub Release assets attached to a version tag. Binary artifacts are never committed
to a `release` branch: doing so permanently grows Git history, duplicates GitHub's
artifact store, and makes platform or checksum replacement ambiguous.

GitHub Actions and release automation are intentionally prohibited by the repository
rules. A release owner runs this checklist locally and records the resulting commit,
tag, checksums, and GitHub Release URL.

## Release contract

- `Cargo.toml` owns the release version; the tag is exactly `v<version>`.
- `main` is the only long-lived branch and must match `origin/main` before tagging.
- The tag points at a reviewed, committed, clean checkout.
- `scripts/verify.sh --release` is the mandatory gate on Apple silicon macOS.
- Release assets contain the two macOS arm64 provider binaries and `SHA256SUMS`.
- crates.io publication is out of scope because `publish = false`.

## Prepare and verify

Confirm that the intended release commit is the entire candidate and that no other
local or remote source branch is being used:

```bash
git switch main
git fetch --prune origin
git status --short --branch
git branch --format='%(refname:short)'
git branch -r --format='%(refname:short)'
git rev-list --left-right --count origin/main...main
```

Set the four explicit external inputs and run the release gate from the clean release
commit:

```bash
export VERGERAIL_CODEX_PACKAGE="/absolute/path/to/official-codex-0.150.1-package"
export VERGERAIL_MODEL="gpt-5.6-luna"
export VERGERAIL_WORKSPACE="/absolute/path/to/existing-workspace"
export VERGERAIL_PERFECTPIXEL_BIN="/absolute/path/to/perfectpixel"
scripts/verify.sh --release
```

The gate must end with the authenticated live marker and leave no owned provider,
guardian, app-server, PerfectPixel, Cargo, or rustc process behind. A timeout or an
unobserved billed image result is not retry authority.

## Build release assets

Build both binaries from the same clean commit. Stage them outside the repository so
generated artifacts cannot enter Git history:

```bash
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
tag="v${version}"
artifact_dir="$(mktemp -d "/private/tmp/vergerail-${tag}.XXXXXX")"
cargo build --offline --locked --release --bins
install -m 0755 target/release/vergerail-upagent-provider \
  "${artifact_dir}/vergerail-upagent-provider-${tag}-aarch64-apple-darwin"
install -m 0755 target/release/ifsc_text_provider \
  "${artifact_dir}/ifsc_text_provider-${tag}-aarch64-apple-darwin"
(cd "${artifact_dir}" && shasum -a 256 \
  vergerail-upagent-provider-* ifsc_text_provider-* > SHA256SUMS)
(cd "${artifact_dir}" && shasum -a 256 -c SHA256SUMS)
```

## Tag and publish

Create one annotated tag at the verified commit, push `main` and that exact tag, then
publish the staged files. Do not reuse or move an existing release tag.

```bash
git tag -a "${tag}" -m "Vergerail ${tag}"
git push origin main
git push origin "${tag}"
gh release create "${tag}" "${artifact_dir}"/* \
  --repo axiom-orient/vergerail \
  --title "Vergerail ${tag}" \
  --verify-tag \
  --notes-file "/absolute/path/to/release-notes.md"
```

After publication, verify that the remote tag resolves to the release commit, download
the assets into a new temporary directory, validate `SHA256SUMS`, and run a bounded
provider protocol smoke test. Record the URL and checksum results before cleaning local
build artifacts with `scripts/clean.sh`.

## Stop and rollback

Stop before tagging if the checkout is dirty, `main` differs from `origin/main`, the
version tag already exists, any release gate fails, an owned process survives, or an
artifact checksum differs.

If a published release is defective, do not force-push `main` or move the tag. Preserve
the evidence, remove the GitHub Release and its tag, revert the release commit on
`main`, and publish a new version only after the full gate passes. A billed image result
whose outcome is unknown must be resolved externally before any retry.
