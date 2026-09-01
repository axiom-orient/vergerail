# 검증과 운영

## 역할

이 문서는 Vergerail의 canonical static gate와 사람이 직접 실행하는 authenticated
live 경계를 구분한다. source와 runtime 계약의 기준은 [고정 프로토콜 계약](PROTOCOL_CONTRACT.md)과
[provider 계약](PROVIDER_CONTRACT.md)이다.

## Static gate

저장소 root에서 실행한다.

```bash
scripts/verify.sh
```

이 명령은 workflow 금지, retired surface 검사, format, locked check/test/doc,
Clippy `-D warnings`, dependency policy, protocol checksum, canonical package
listing(`tests/fixtures/guardian_survivor_mutant.c` 포함)을 검증한다.
기본 검증은 network account나 실제 외부 provider 성공을 주장하지 않는다.
지원 host에 pinned package가 설치되어 있으면 `official_runtime`의 helper-deadline
회귀가 실제 package hash/size 검증을 guardian process로 실행하고 만료 후 helper
artifact가 남지 않는지 확인한다. 이 회귀는 authenticated 또는 billable 호출을
하지 않는다.

좁은 회귀 확인은 다음처럼 실행한다.

```bash
cargo test --offline --locked --all-targets -- --test-threads=1
cargo test --offline --locked --doc
cargo test --offline --locked --test vergerail_upagent_provider_protocol
cargo test --offline --locked --test ifsc_text_provider_protocol
```

## Authenticated live gate

`examples/live_e2e.rs`는 실행 host와 사용자가 소유한 이미 인증된 account를
검증한다. credential을 읽거나 복사하지 않으며, OAuth·MFA를 자동 시작하지 않는다.
Apple silicon macOS에서 고정 `0.150.1` package와 Codex가 선택한 인증 account를
사용하고 다음 네 값을 명시한다. upstream `CODEX_HOME`이 있으면 Codex 설정으로
그대로 적용되며 Vergerail 전용 account 변수는 없다.

```bash
export VERGERAIL_CODEX_PACKAGE="/absolute/path/to/official-codex-0.150.1-package"
export VERGERAIL_MODEL="gpt-5.6-luna"
export VERGERAIL_WORKSPACE="/absolute/path/to/existing-workspace"
export VERGERAIL_PERFECTPIXEL_BIN="/absolute/path/to/perfectpixel"
scripts/verify.sh --release
```

`--release`는 committed clean checkout, 지원 host, 세 가지 runtime 입력
(`VERGERAIL_CODEX_PACKAGE`, `VERGERAIL_MODEL`, `VERGERAIL_WORKSPACE`)과 별도의
PerfectPixel 파일을 요구한 뒤 static gate,
package, official runtime, managed runtime, live E2E를 실행한다. standalone
live example을 실행할 때도 같은 세 runtime 변수와 PerfectPixel 변수를 모두
유지한다.

live 흐름은 account가 ChatGPT인지 확인하고, visible model을 조회한 뒤 다음을
검증한다.

- one-shot text, persistent session, resume, interruption
- text-only/read-only/workspace-write 권한 경계와 root confinement
- direct image generation의 실제 PNG와 `gpt-image-2` adapter 경계
- PerfectPixel inspect, chroma plan, image conversion, PSD export
- diagnostics 부재, provider/PerfectPixel process cleanup, delayed survivor 부재

성공 출력은 전체 흐름에서 `VERGERAIL_LIVE_E2E_FULL_OK`이며 image-only 흐름은
`VERGERAIL_IMAGE_ONLY_OK`다. 최근 실사용 검증은 provider text/image와 UpAgent
creator/creator-image를 포함해 이미지 receipt 한 건을 생성하고 PerfectPixel의
alpha·PSD 증거까지 확인했다. 이 기록은 재현 가능한 절차의 근거이지 모든 host에서
성공한다는 보장은 아니다.

## 실패와 복구

- signed-out account 또는 hidden model은 live gate 실패다.
- timeout, cancellation, transport disconnect, malformed response는 성공으로
  바꾸지 않는 typed failure다.
- 완료를 관찰할 수 없는 비멱등 image operation은 자동 재시도하지 않으며 caller가
  외부 결과를 확인하고 복구해야 한다.
- `timeoutMs`는 runtime verify/connect과 operation에 공유되는 하나의 monotonic
  deadline이다. deadline 만료 뒤 shutdown은 별도의 고정 2초 teardown budget으로
  bounded하게 시도하며 이 budget은 `timeoutMs`에 포함되지 않는다. dispatch 뒤 billed image timeout/cancellation은
  `resolution_required`/`OutcomeUnknown`이며, dispatch 전에는 timeout/cancel이다.
- 작업과 cleanup이 모두 실패하면 원래 operation/cause를 유지하고 cleanup 오류를
  함께 보고한다.

## 산출물 정리

```bash
scripts/clean.sh
```

Vergerail에는 별도 cleanup-inventory script가 없다. 삭제 전에는 root에서 Git 상태와
script가 다루는 후보를 직접 확인한다.

`scripts/verify.sh`의 dedicated-home guard는 제거된 전용 home 표면만 검사하며
upstream `CODEX_HOME`은 허용한다. guardian survivor negative fixture는 production
guardian에 유출되지 않고 canonical package에 포함된다.

```bash
git status --short
find . -path ./.git -prune -o \( \
  -name target -o -name package-check -o -name coverage -o \
  -name tarpaulin-report.html -o -name lcov.info -o -name .DS_Store \
\) -print
```

`scripts/clean.sh`는 root Cargo `target/`을 `cargo clean`으로 지우고 root
`package-check/`·`coverage/`, `tarpaulin-report.html`, `lcov.info`와 모든
`.DS_Store`를 제거한다. 재생성은 `cargo build --locked --workspace` 또는
`scripts/verify.sh`로 수행한다. 로그인 home, credential, 외부 runtime cache와
다른 저장소는 건드리지 않는다.

정리 후에는 `find . -path ./.git -prune -o -name .DS_Store -type f -print`가
비어 있고 `git status --short`로 source/documentation 변경만 남는지 확인한다.

## Process custody

Guardian과 app-server, live PerfectPixel child는 Vergerail이 소유한다. timeout,
cancellation, panic, partial spawn과 정상 종료 모두 process group을 종료하고
reap한 뒤 임시 directory를 제거한다. 확인할 수 없는 survivor는 성공으로 간주하지
않는다. launch 직전 filesystem verification job도 cancellation token과
JoinHandle을 소유하며, 취소 후 worker acknowledgement 전에는 반환하지 않는다.
