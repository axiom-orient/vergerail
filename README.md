# Vergerail

> Verified runtimes. Explicit authority.

Vergerail은 Rust 애플리케이션에서 고정된 OpenAI Codex app-server를 로컬 자식 프로세스로 사용하는 라이브러리입니다. 검증된 runtime을 stdio JSONL로 연결하며 범용 provider SDK, HTTP client, 공개 daemon은 아닙니다. 저장소에는 IFSC `ScreenProgram`용 `ifsc_text_provider`와 UpAgent용 `vergerail-upagent-provider` one-shot binary도 포함됩니다.

현재 지원 조합은 Apple silicon macOS, Codex `0.150.1`, Rust `1.97.1` 이상입니다. Linux에서는 runtime을 실행하지 않는 library와 provider의 정적 경로만 build·test할 수 있습니다. `publish = false`이므로 crates.io에는 게시하지 않습니다.

## 저장소 계약

- 입력: `Cargo.toml`·`Cargo.lock`, `runtime/` lock, `protocol/` schema·provenance, provider stdin JSON, caller가 지정한 workspace
- 출력: Rust API의 typed event/result와 provider stdout의 JSON 값 하나
- 산출물: `target/`, `package-check/`, coverage 파일과 OS 임시 파일; 모두 `scripts/clean.sh`로 재생성 가능하게 제거
- 외부 사용자 상태: app-server가 관리하는 로그인 home과 Vergerail runtime cache; 저장소 산출물이 아니며 clean/package 대상이 아님

저장소 스크립트는 두 개만 유지합니다.

```bash
scripts/verify.sh
scripts/verify.sh --release
scripts/clean.sh
```

`verify.sh`는 로컬 정적·테스트·package gate입니다. `--release`는 clean committed HEAD, 지원 host, 공식 runtime, 표준 ChatGPT 로그인과 live E2E를 추가로 요구합니다. GitHub Actions, `.github/workflows`, CI/CD 설정은 이 저장소에서 금지하며 verify도 해당 경로가 있으면 실패합니다.

## 설치

```toml
[dependencies]
vergerail = { path = "../vergerail" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Git source는 mutable branch나 tag 대신 검증한 full commit SHA로 고정하세요.

```toml
[dependencies]
vergerail = { git = "https://github.com/axiom-orient/vergerail.git", rev = "<verified-full-commit-sha>" }
```

## 빠른 시작

Vergerail은 별도 인증 저장소를 만들지 않습니다. 기본 `CodexConfig`는 상속된
`CODEX_HOME`을 제거하며, provider 경계는 반드시 명시한 managed home을
`CODEX_HOME`으로 전달합니다. 그 디렉터리는 ChatGPT 앱 또는 `codex login`으로
먼저 인증되어 있어야 하며 Vergerail은 credential을 복사하거나 생성하지 않습니다.

```rust,no_run
use vergerail::{Account, Codex, CodexConfig, RuntimeResolver};

#[tokio::main]
async fn main() -> vergerail::Result<()> {
    let runtime = RuntimeResolver::new().resolve().await?.into_package();
    let codex = Codex::connect(CodexConfig::new(runtime)).await?;

    match codex.account().await? {
        Account::SignedOut { .. } => println!("ChatGPT 또는 codex login으로 로그인하세요"),
        Account::ChatGpt { email, plan } => println!("{email:?} / {plan}"),
    }

    codex.shutdown().await
}
```

`RuntimeResolver::resolve()`는 정확한 고정 runtime을 관리 cache에서 재사용하거나 공식 archive를 설치합니다. 외부의 완전한 감사 package를 재사용할 때만 `with_system_discovery(true)`를 명시합니다. `Codex::connect()`는 package를 재검증하고 실행하지만 설치를 시작하지 않습니다.

로그인이 필요하면 `Codex::login()`을 사용할 수 있습니다. OAuth URL 열기, 계정 선택, 승인과 MFA는 host와 사용자가 처리합니다. `Codex::logout()`은 공유하는 표준 Codex 계정을 로그아웃하므로 호출자는 그 영향을 명시적으로 소유해야 합니다.

`Codex::run()`은 임시 read-only session에서 network를 끄고 approval을 자동 거부합니다. 파일 쓰기는 `SessionOptions::workspace_write()` persistent session에서만 요청할 수 있으며 caller가 event approval에 직접 응답해야 합니다. session은 `Session::close()`, client는 `Codex::shutdown()`으로 종료합니다.

## 이미지와 provider

이미지 생성은 기본적으로 꺼져 있습니다. `CodexConfig::new(runtime).with_image_generation(true)`로 명시적으로 활성화하며 `Event::ImageGeneration`, `RunResult::image_generations`, `Session::audit_turn()`을 함께 검증합니다.

`ifsc_text_provider`는 read-only text-only session에서 정적 `ScreenProgram` JSON을 반환합니다. `vergerail-upagent-provider`는 고정 공식 runtime package, 명시적 managed home, model, read-only workspace를 환경으로 받아 `model_turn` 또는 `image_generate` 한 번을 처리합니다. `image_generate`는 먼저 공식 app-server의 `getAuthStatus`로 갱신 가능한 인증을 받아 ChatGPT Images endpoint를 직접 호출하므로 모델의 도구 선택에 의존하지 않습니다. HTTP 401일 때만 인증을 한 번 갱신하고 한 번 재시도합니다. 유효한 PNG 한 장이면 실제 크기와 PNG의 alpha 가능 여부를 포함해 성공으로 반환합니다. 두 provider 모두 managed home의 기존 Codex 로그인을 재사용하며 credential 파일을 읽거나 복사하지 않습니다.

provider 실행에는 `VERGERAIL_CODEX_HOME`도 필요합니다. 이 값은 기존 로그인 상태가
있는 명시적 managed home이어야 합니다.

이미지 배경·크기·품질은 `image_generate.imageOptions`로 지정할 수 있습니다.
provider가 공식 app-server에서 인증만 내보내고 이미지 요청은 고정된 Images
endpoint로 보내므로 app-server를 별도로 빌드하거나 패치할 필요가 없습니다.
`VERGERAIL_CODEX_LOCK` 같은 사용자 제작 runtime lock은 지원하지 않습니다.

## 주요 API와 문서

- 연결·실행: `Codex`, `CodexConfig`, `Session`, `Run`, `SessionOptions`, `Sandbox`
- 계정·모델: `Account`, `Login`, `LoginMethod`, `Model`
- 이벤트·결과: `Event`, `RunResult`, `ImageGeneration`, `TurnAudit`, `Usage`, `Diagnostic`
- runtime: `RuntimeResolver`, `RuntimePackage`, `ResolvedRuntime`, `RuntimeOrigin`, `DownloadPolicy`
- 오류: `Error`, `ErrorKind`, `Result`; 비멱등 결과가 불명확하면 `OutcomeUnknown`

전체 public surface는 [src/lib.rs](src/lib.rs)가 기준입니다. 세부 경계는 [아키텍처](docs/ARCHITECTURE.md), [프로토콜](docs/PROTOCOL_CONTRACT.md), [IFSC provider](docs/IFSC_TEXT_PROVIDER.md), [UpAgent provider](docs/PROVIDER_CONTRACT.md), [검증](docs/VERIFICATION.md), [보안](SECURITY.md)을 따릅니다.

라이선스는 Apache-2.0입니다.
