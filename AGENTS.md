# Repository rules

- Do not create, modify, or enable GitHub Actions or `.github/workflows`.
- Use `scripts/verify.sh` as the canonical local verification entrypoint.
- Use `scripts/clean.sh` to remove repository-local generated artifacts.
- Keep runtime locks and protocol provenance as immutable inputs; never edit generated schemas by hand.
- Keep one canonical API and state format. Do not add compatibility aliases, migration adapters, deprecated shims, or silent fallbacks.
- Do not commit `target/`, credentials, standard `~/.codex` state, logs, caches, or temporary files.

## Project profile

- Project kind: Rust protocol/runtime client with pinned protocol and host integration.
- Platform: host-native runtime; platform-specific clients must remain behind the declared adapter boundary.
- Primary gates: `scripts/verify.sh` and `scripts/clean.sh`; preserve protocol provenance and runtime locks.

## Shared workspace policy

이 프로젝트에도 parent workspace `AGENTS.md`를 적용한다. 브라우저 작업은 Aside CLI의 `aside`만 사용하고 새 Playwright/Puppeteer 자동화를 추가하지 않는다. GitHub Actions·`.github/workflows`·CI/CD·release automation은 생성·수정·복원하지 않으며 검증은 로컬 명령으로 한다.
