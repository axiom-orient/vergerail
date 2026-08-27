# 검증

모든 명령은 저장소 루트에서 실행합니다. 필요한 toolchain은 Rust `1.97.1`, rustfmt, Clippy와 `cargo-deny`입니다.

## 기본 gate

계정이나 실제 runtime 없이 canonical script를 실행합니다.

```bash
scripts/verify.sh
```

이 script는 fmt, offline locked build/test(guardian process fixture를 위해 test thread 2개로 bounded), doctest, Clippy, rustdoc, dependency policy, protocol hash와 package 검증을 실행합니다. GitHub Actions 경로, repository clutter 또는 폐기된 first-party symbol도 거부합니다. lockfile dependency가 Cargo cache에 없는 새 환경에서는 먼저 `cargo fetch --locked`가 필요합니다.

## 공식 runtime

`VERGERAIL_CODEX_PACKAGE`를 audited package root로 설정합니다. package는 `bin/codex`, `bin/codex-code-mode-host`, `codex-package.json`, `codex-path`, `codex-resources`를 포함해야 합니다.

`VERGERAIL_CODEX_PACKAGE`가 설정된 상태에서 `scripts/verify.sh`를 실행하면 공식 runtime과 IFSC signed-out test도 함께 실행합니다. process guardian 회귀는 기본 all-targets test에 포함되며, aarch64 macOS에서 embedded digest, owner-only extraction, TERM-무시 leader의 deterministic late-fork fixture, package에서 제외된 독립 test-only mutation fixture, descendant pipe teardown, immediate/delayed no-survivor structural guard, liveness teardown, startup/exec failure를 직접 실행합니다.

첫 ignored test는 package 전체 검증, 빈 임시 home의 handshake, signed-out 계정과 정상 종료·재사용을 검사합니다. 둘째 ignored test는 IFSC binary의 signed-out typed failure를 검사합니다.

## aarch64 Linux build와 offline process smoke

Linux에서는 macOS guardian process fixture를 실행하지 않습니다. 대신 target-neutral unit/integration test와 IFSC의 runtime 접근 전 typed failure 경로를 검증할 수 있습니다.

```bash
cargo build --locked
cargo test --locked --all-targets -- --test-threads=2
cargo test --locked --doc
cargo install --path . --root ./target/linux-install --locked
./target/linux-install/bin/ifsc_text_provider </dev/null
```

마지막 command의 기대 결과는 exit `2`, stdout의 `invalid-request` JSON 한 값, 빈 stderr입니다. 이 smoke는 Linux process가 실제로 설치·시작되고 stdin/stdout 계약을 지킨다는 증거일 뿐입니다. `runtime/pinned-macos-aarch64.json`과 guardian 지원 경계는 그대로이므로 Linux에서 Codex app-server 연결·command 실행이 가능하다는 증거가 아닙니다. 검증 후에는 `scripts/clean.sh`로 repository-local artifact를 제거합니다.

## 관리형 runtime

다음 test는 고정 archive를 격리 cache에 다운로드해 검증·설치·연결하고 `DownloadPolicy::Never` 재사용까지 확인합니다.

```bash
cargo test --locked --test managed_runtime -- --ignored --nocapture
```

## package와 consumer

기본 verify는 작업 checkout의 package 내용과 compile을 검사합니다. 목록에 credential, `target`, runtime binary나 임시 파일이 있으면 실패입니다. 개발 중에는 `scripts/verify.sh`가 호환성을 위해 `--allow-dirty` package 검사를 사용합니다. 배포 직전에는 `scripts/release-verify.sh`가 committed `HEAD`, clean tree, 지원 host, 전용 authenticated home과 audited package를 요구한 뒤 공식/managed/live external suite와 `cargo package --offline --locked`( `--allow-dirty` 없음)를 실행합니다.

별도 consumer crate는 상대경로로 public surface를 확인합니다.

```toml
[dependencies]
vergerail = { path = "../vergerail" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

처음에는 `cargo check`로 consumer lockfile을 만들고, 이후 `cargo check --locked`를 사용합니다. 공개 GitHub consumer는 검증한 remote의 full commit SHA로 고정해 두 검사를 수행합니다. 이후 release에서도 같은 검사를 새 full SHA로 반복합니다.

## 실제 계정 E2E

일반 `~/.codex`나 복사한 `auth.json`이 아닌 새 전용 home을 사용합니다.

```bash
export VERGERAIL_CODEX_HOME="$HOME/.local/share/vergerail-live-e2e"
export VERGERAIL_HOME_OWNER=vergerail
export VERGERAIL_MODEL=gpt-5.6-luna
export VERGERAIL_WORKSPACE="$PWD"
export VERGERAIL_PERFECTPIXEL_BIN=/absolute/path/to/perfectpixel/target/debug/perfectpixel
cargo run --locked --example live_e2e
```

이미지 경로만 빠르게 재검증할 때는 같은 환경에서 `VERGERAIL_IMAGE_ONLY=1`을 추가합니다. 이 모드는 account/model 확인, 실제 이미지 생성, durable audit 대조와 PerfectPixel 검사만 수행하고 `VERGERAIL_IMAGE_ONLY_OK`를 출력합니다. release gate는 이 변수가 존재하면 실패하므로 전체 E2E 표식으로 오인할 수 없습니다.

`VERGERAIL_CODEX_PACKAGE`를 설정하면 그 package만 사용합니다. 생략하면 기본 resolver는 터미널이나 ChatGPT 앱의 Codex를 탐색하지 않고 VergeRail 관리 cache를 검증한 뒤 필요한 경우 고정 archive를 설치합니다. 완전한 외부 package 재사용은 호출자가 `with_system_discovery(true)`로 명시적으로 허용해야 합니다.

첫 실행은 일회용 OAuth URL을 출력합니다. 사용자가 외부 브라우저에서 계정 선택, OAuth 승인과 MFA를 마치면 harness가 다음을 검사합니다.

- exact visible model과 one-shot 실행
- persistent session 재개와 interrupt
- text-only host-read 차단
- read-only command 대조군과 write·network 차단
- workspace-write와 root confinement
- live event와 completed full durable audit 일치
- 실제 image-generation 결과와 durable image item 일치
- 생성된 PNG/JPEG/WebP가 `perfectpixel inspect`에서 디코딩되고 non-empty raster로 판정됨
- 생성 래스터를 `perfectpixel convert`의 Lanczos3 filter로 512×512 PNG로 변환하고 수정본도 다시 inspect함

성공 표식:

```text
VERGERAIL_IMAGE_E2E_OK id=<image-item-id> width=<width> height=<height> foregroundPixels=<count> modifiedWidth=512 modifiedHeight=512 modifiedForegroundPixels=<count>
VERGERAIL_LIVE_E2E_FULL_OK model=<selected-model> owner=<consumer-owner>
```

E2E는 sandbox 검증용 임시 root와 생성 래스터용 임시 directory만 변경하며 래스터를 저장소에 남기지 않습니다. 판정은 모델 설명이나 approval 유무가 아니라 live item과 `Session::audit_turn()`의 같은 item ID·status, base64 bytes, PerfectPixel JSON 검사를 대조합니다. 어느 단계가 실패해도 app-server shutdown을 기다리고 cleanup 오류를 함께 보고합니다.

## 현재 실행 증거

2026-08-27 Apple silicon macOS, Rust `1.97.1`, Codex `0.150.1` 공식
`aarch64-apple-darwin` package에서 다음을 확인했습니다.

- source identity: receipt 수집 시점의 canonical 공개 repository와
  `/Users/ax/repoGithub/vergerail` clean working tree입니다. 이 문서는 commit
  SHA를 self-reference하지 않으며, `scripts/release-verify.sh`가 clean
  committed `HEAD`에서 같은 production inputs와 official/managed/live
  external suites를 실행해 release evidence를 해당 commit에 bind합니다.
  공개 GitHub remote consumer도 별도 full commit SHA 검증을 통과했습니다.

- protocol source archive와 raw/canonical schema의 byte 수와 SHA-256를 독립적으로
  다시 계산했고 provenance checksum 검사를 통과했습니다.
- runtime focused test 22개 통과. 이전 독립 all-target run에서 guardian legacy
  fixture의 PID 값이 비어 한 차례 실패했지만 exact 재실행과 이후 canonical
  all-target run 두 번은 통과했습니다. 현재 `scripts/verify.sh`는 library 161개,
  IFSC unit 5개, provider unit 4개, IFSC protocol 5개와 live harness unit 22개,
  Clippy, rustdoc, cargo-deny, protocol checksum, package verification을 모두 마치고
  exit `0`입니다. 간헐 fixture 관찰 이력은 release 판단에서 계속 추적합니다.
- 공식 runtime ignored test 2개와 IFSC signed-out test 1개 통과
- managed runtime download/install/reuse ignored test 1개 통과
- isolated local path consumer의 최초 `cargo check`와 후속 `cargo check --locked` 통과
- authenticated dedicated-home live E2E는 0.150.1 전용 home, model과
  PerfectPixel 입력이 제공되지 않아 이번 upgrade에서 실행하지 않았습니다.
  signed-out handshake/account/shutdown과 managed download/reuse만 현재 증거입니다.
- package 검증에는 protocol provenance, target-neutral guardian stub와
  third-party notice가 포함되고 credential, `target`, runtime binary는
  제외됨
- official package URL의 archive `117535191` bytes / SHA-256
  `e5008a5b8d7a22e2ad2922a74783de8f7202b7b886a181e6ddbec7633609c885`를 다시
  확인했으며, managed download/reuse ignored test도 `1 passed`입니다.
- production guardian C source에는 legacy mutant/acknowledgement 문자열과
  compile flag가 없으며, 해당 결함을 재현하는 독립
  `tests/fixtures/legacy_guardian_mutant.c`를 Rust test가 고정 clang으로
  임시 빌드합니다. fixture는 Cargo package 입력에서 제외됩니다. production
  guardian artifact는 `69440` bytes / SHA-256
  `e3d54de78017ab65558fc99d49564280be3aaa11699b34d0f61cddd15f8765ad`이며,
  deterministic UUID `56455247-4552-4149-4C2D-475541524431`, codesign verify,
  정상 argv 실행을 확인했습니다. normal/CFLAGS/CPPFLAGS/CC(extra args) 4개
  isolated build가 모두 같은 bytes/SHA를 냈고, production helper의 legacy
  문자열·심볼은 모두 부재했습니다.
- current candidate에서 공식 runtime 2개, IFSC signed-out 1개, managed
  download/reuse 1개가 0.150.1 package로 exit `0`입니다. release 전체 gate는
  clean committed checkout과 authenticated live E2E가 준비된 뒤 다시 실행합니다.

이 증거는 receipt 수집 시점의 local source, 공개 GitHub source와 고정 runtime
실사용을 지지합니다. crates.io package 배포 증거는 아니며, 각 release commit의
최종 증거는 `scripts/release-verify.sh` 재실행 결과와 matching remote
full-SHA consumer 검증으로 판단합니다.

## 산출물 정리

```bash
scripts/clean.sh
```

Cargo `target/`, coverage/package-check 산출물과 Finder metadata만 제거합니다. 전역 Cargo cache, runtime cache, credential과 전용 `CODEX_HOME`은 제거하지 않습니다. 모든 build 산출물은 `scripts/verify.sh` 또는 `cargo build --locked`로 재생성할 수 있습니다.
