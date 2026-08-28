# 검증

검증 진입점은 두 스크립트뿐입니다.

```bash
scripts/verify.sh
scripts/clean.sh
```

`scripts/verify.sh`는 다음을 순서대로 검사합니다.

- GitHub Actions와 `.github/workflows` 금지
- repository clutter와 제거된 별도 인증-home API 재유입 금지
- `cargo fmt`, `cargo check`, 전체 test와 doctest
- `cargo clippy -D warnings`, rustdoc warning
- license/advisory/source policy, protocol checksum과 package 내용

배포 후보는 같은 진입점에 `--release`를 사용합니다. clean committed HEAD와 Apple silicon macOS를 요구하고, 아래 입력으로 공식 runtime 계정 연결, runtime 재사용, managed runtime, live text/image E2E, strict package를 추가 검증합니다.

```bash
export VERGERAIL_CODEX_PACKAGE="/absolute/path/to/audited/package"
export VERGERAIL_MODEL="gpt-5.6-luna"
export VERGERAIL_WORKSPACE="$PWD"
export VERGERAIL_PERFECTPIXEL_BIN="/absolute/path/to/perfectpixel"
scripts/verify.sh --release
```

인증 입력은 별도 환경 변수가 아닙니다. app-server가 표준 `~/.codex`를 사용하므로 ChatGPT 앱 또는 `codex login`으로 로그인된 계정을 재사용합니다. signed-out 상태에서는 live 검증이 즉시 실패하며 검증 중 브라우저나 OAuth 흐름을 자동 시작하지 않습니다.

## 산출물 정리

```bash
scripts/clean.sh
```

이 명령은 repository root를 확인한 뒤 `cargo clean`, `package-check/`, coverage 결과와 `.DS_Store`만 제거합니다. 표준 `~/.codex`, 사용자 credential, 외부 runtime cache, 다른 저장소는 건드리지 않습니다. 제거한 build 결과는 `cargo build --locked` 또는 `scripts/verify.sh`로 복원할 수 있습니다.

## 프로세스 종료 증거

Guardian contract tests는 정상·오류·panic·timeout·취소 경로에서 Vergerail이 직접 소유한 process tree의 종료와 helper 임시 디렉터리 제거를 검증합니다. `tests/fixtures/guardian_survivor_mutant.c`는 과거 호환물이 아니라 종료 누락을 재현하는 결함 fixture이므로 회귀 검증 입력으로 유지합니다.
