# Vergerail

> Verified runtimes. Explicit authority.

Vergerail은 Rust 애플리케이션과 고정된 OpenAI Codex app-server 사이의 좁은
stdio JSONL 경계다. library와 두 개의 one-shot provider binary를 제공하며,
범용 HTTP SDK, daemon, credential store, 또는 app-server 배포판은 아니다.

현재 지원 계약은 Apple silicon macOS, Codex `0.150.1`, Rust `1.97.1` 이상이다.
`publish = false`이므로 crates.io 배포 대상이 아니다. Linux에서는 runtime을
실행하지 않는 library와 정적 provider 경계만 검증할 수 있다.

## 저장소 지도

| 경로 | 책임 |
| --- | --- |
| `src/` | typed client, session, runtime, image adapter |
| `src/bin/vergerail_upagent_provider.rs` | UpAgent `vergerail.upagent/1` one-shot provider |
| `src/bin/ifsc_text_provider.rs` | IFSC `ScreenProgram` one-shot text provider |
| `runtime/`, `protocol/` | 공식 `0.150.1` runtime lock과 app-server schema |
| `tests/`, `examples/` | protocol·runtime 계약과 human-controlled live E2E |
| `docs/` | public contract, architecture, provider, verification |
| `scripts/` | canonical verification과 재생성 가능한 산출물 정리 |

저장소의 wire와 runtime truth source는 [고정 프로토콜 계약](docs/PROTOCOL_CONTRACT.md),
public provider 입력은 [provider 계약](docs/PROVIDER_CONTRACT.md), 실행 경계는
[아키텍처](docs/ARCHITECTURE.md)다.

## 빌드와 검증

```bash
cargo build --locked --workspace
scripts/verify.sh
scripts/clean.sh
```

`verify.sh`는 format, locked build/test, Clippy, rustdoc, dependency policy,
protocol checksum, package 검사를 수행한다. 배포 후보의 Apple silicon macOS와
실제 계정·runtime·image·PerfectPixel 경계는 `scripts/verify.sh --release`가
별도로 요구한다. 필요한 환경 변수와 실패 의미는 [검증](docs/VERIFICATION.md)에
있다.

## Library 사용

```toml
[dependencies]
vergerail = { git = "https://github.com/axiom-orient/vergerail.git", rev = "<verified-full-commit-sha>" }
```

```rust,no_run
use vergerail::{Codex, CodexConfig, RuntimeResolver};

#[tokio::main]
async fn main() -> vergerail::Result<()> {
    let runtime = RuntimeResolver::new().resolve().await?.into_package();
    let codex = Codex::connect(CodexConfig::new(runtime)).await?;
    codex.shutdown().await
}
```

기본 library와 provider 설정은 Codex가 선택한 account를 그대로 사용한다. 별도의
Vergerail account 경로는 없으며, upstream `CODEX_HOME`이 설정돼 있으면 공식
app-server가 그 값을 해석한다. Vergerail은 credential을 복사하거나 생성하지 않는다.

## Provider

`vergerail-upagent-provider`는 다음 세 환경 변수를 모두 명시해야 한다.

```text
VERGERAIL_CODEX_PACKAGE  # 공식 0.150.1 package
VERGERAIL_MODEL          # text/tool-planning model
VERGERAIL_WORKSPACE      # 기존 read-only directory
```

stdin과 stdout은 각각 bounded JSON 값 하나이며, operation은 `model_turn` 또는
`image_generate`다. 이미지 입력의 `background`, `size`, `quality`만 caller가
선택할 수 있고 pixel model은 공식 `0.150.1` image adapter의 `gpt-image-2`로
고정된다. PNG decompressed scanline raw data는 정확히 14 MiB로 제한되며,
dispatch 뒤 결과를 잃은 billed image 요청은 `OutcomeUnknown`으로 반환되고
자동 재시도되지 않는다. `timeoutMs`는 검증·connect·operation에 공유되는 하나의
monotonic deadline이다. 이 deadline이 만료된 뒤에는 별도의 고정 2초 teardown
budget으로 cleanup을 bounded하게 시도하며, teardown budget은 사용자 operation
deadline에 포함되지 않는다. 자세한 field, 오류, timeout, cancellation은 [provider 계약](docs/PROVIDER_CONTRACT.md)을
참조한다.

`ifsc_text_provider`는 `VERGERAIL_WORKSPACE`와 `VERGERAIL_MODEL`이 필요하고,
`VERGERAIL_CODEX_PACKAGE` 및 명시적 runtime download 정책을 선택한다. 이 binary의
입력과 `ScreenProgram` 검증은 [IFSC provider](docs/IFSC_TEXT_PROVIDER.md)에 있다.

## 문서

- [Architecture](docs/ARCHITECTURE.md): 책임, 소유권, process lifecycle
- [Protocol contract](docs/PROTOCOL_CONTRACT.md): 고정 app-server schema와 typed event
- [Provider contract](docs/PROVIDER_CONTRACT.md): UpAgent JSONL wire와 image adapter
- [IFSC text provider](docs/IFSC_TEXT_PROVIDER.md): ScreenProgram 입력·출력
- [Verification](docs/VERIFICATION.md): static gate, authenticated live flow, cleanup
- [Release workflow](docs/RELEASE.md): tag, local release gate, binary assets, rollback
- [Security](SECURITY.md): 인증, 권한, credential과 process 경계

라이선스는 Apache-2.0이다.
