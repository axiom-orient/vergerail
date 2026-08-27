# Vergerail

> Verified runtimes. Explicit authority.

Vergerail은 Rust 애플리케이션에서 고정된 OpenAI Codex app-server를 로컬 자식 프로세스로 사용하는 라이브러리입니다. 검증된 runtime을 stdio JSONL로 연결하며, 범용 provider SDK·HTTP client·공개 daemon은 아닙니다. 저장소에는 정적 IFSC `ScreenProgram`을 생성하는 one-shot `ifsc_text_provider` binary도 있습니다.

현재 checkout의 Codex runtime 연결은 Apple silicon macOS, Codex `0.150.1`, Rust `1.97.1` 이상을 지원합니다. aarch64 Linux에서는 library와 `ifsc_text_provider`를 build·test·install하고 runtime에 접근하지 않는 typed input/error 경로를 실행할 수 있지만, 고정 runtime과 guardian은 macOS 전용이므로 `RuntimeResolver`/`Codex` 실행 지원을 뜻하지 않습니다. 이 프로젝트는 공개 GitHub source로 배포하며, `publish = false` 설정으로 crates.io에는 게시하지 않습니다. 상세 증거와 배포 조건은 [검증](docs/VERIFICATION.md)과 [배포](docs/RELEASE.md) 문서가 관리합니다.

## 저장소 계약

- 입력: `Cargo.toml`·`Cargo.lock`, `runtime/` lock, `protocol/` schema·provenance, library 또는 IFSC 요청
- 출력: Rust library API와 `ifsc_text_provider`의 stdout JSON 한 값
- 산출물: Cargo가 재생성하는 `target/`; source·credential·전용 `CODEX_HOME`과 분리

```bash
scripts/verify.sh
scripts/clean.sh
```

`verify.sh`가 개발 중 기본 검증 진입점이고 `release-verify.sh`가 committed HEAD, clean tree, 지원 host와 실제 external proof 환경을 요구하는 배포 직전 진입점입니다. `clean.sh`가 repository-local 산출물을 제거합니다. GitHub Actions와 `.github/workflows`는 이 저장소에서 금지하며 verify 단계도 해당 경로가 있으면 실패합니다.

## 설치

소비자 crate에서 이 checkout을 상대경로로 참조합니다.

```toml
[dependencies]
vergerail = { path = "../vergerail" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

`publish = false`는 crates.io 게시만 차단합니다. Git dependency는 canonical repository가 배포된 뒤 검증한 commit SHA로 고정해야 합니다.

공개 GitHub source를 사용할 때는 mutable branch나 tag 대신 검증한 full commit SHA를 고정하세요.

```toml
[dependencies]
vergerail = { git = "https://github.com/axiom-orient/vergerail.git", rev = "<verified-full-commit-sha>" }
```

canonical source: <https://github.com/axiom-orient/vergerail>

## 빠른 시작

Vergerail 앱마다 비어 있는 전용 `CODEX_HOME`과 안정적인 lowercase owner ID를 사용하세요. 일반 `~/.codex` 또는 복사한 `auth.json`은 허용되지 않습니다.

```rust,no_run
use vergerail::{Account, Codex, CodexConfig, RuntimeResolver};

#[tokio::main]
async fn main() -> vergerail::Result<()> {
    let runtime = RuntimeResolver::new().resolve().await?.into_package();
    let codex_home = std::env::var_os("VERGERAIL_CODEX_HOME")
        .expect("VERGERAIL_CODEX_HOME must be set");
    let codex = Codex::connect(
        CodexConfig::new(runtime, codex_home).with_home_owner("my-app"),
    )
    .await?;

    match codex.account().await? {
        Account::SignedOut { .. } => println!("로그인이 필요합니다"),
        Account::ChatGpt { email, plan } => println!("{email:?} / {plan}"),
    }

    codex.shutdown().await
}
```

`RuntimeResolver::resolve()`는 기본적으로 터미널의 `codex`나 ChatGPT 앱 번들을 사용하지 않고 VergeRail 관리 cache에서 정확한 고정 runtime을 재사용하거나 공식 archive를 설치합니다. 외부의 완전한 감사 package를 재사용하려는 호출자만 `with_system_discovery(true)`를 명시해야 합니다. `Codex::connect()`는 전달받은 package를 재검증하고 실행하지만 설치를 시작하지 않습니다.

## 로그인과 실행

```rust,no_run
use vergerail::{Codex, LoginMethod, SessionOptions};

async fn login_and_run(codex: &Codex) -> vergerail::Result<String> {
    if matches!(codex.account().await?, vergerail::Account::SignedOut { .. }) {
        let login = codex.login(LoginMethod::Browser).await?;
        println!("{}", login.auth_url().expect("browser login URL"));
        login.wait().await?;
    }

    let result = codex
        .run("이 저장소의 테스트 실패 원인을 설명해 줘.", SessionOptions::read_only("."))
        .await?;
    Ok(result.text)
}
```

URL 열기, 계정 선택, OAuth 승인과 MFA는 host 애플리케이션과 사용자가 처리합니다. `Codex::run()`은 임시 read-only session에서 network를 끄고 approval을 자동 거부합니다. 파일 쓰기는 `SessionOptions::workspace_write()` persistent session에서만 요청할 수 있으며 caller가 event approval에 직접 응답해야 합니다.

session을 마치면 `Session::close()`, client 전체는 `Codex::shutdown()`으로 종료합니다. 완료된 persistent turn의 durable effect는 session을 닫기 전에 `Session::audit_turn()`으로 검사합니다.

## 이미지 생성

이미지 생성은 기본적으로 꺼져 있습니다. 전용 home을 연결할 때 명시적으로 활성화하면 `Event::ImageGeneration`으로 수명주기를 관찰하고, terminal `RunResult::image_generations`에서 item ID별 최신 상태와 base64 결과를 받을 수 있습니다. persistent session의 `TurnAudit::image_generations`와 대조하면 live 결과가 durable history와 일치하는지도 확인할 수 있습니다.

```rust,no_run
use vergerail::{Codex, CodexConfig, RuntimePackage, SessionOptions};

async fn generate(
    runtime: RuntimePackage,
    codex_home: impl Into<std::path::PathBuf>,
) -> vergerail::Result<()> {
    let codex = Codex::connect(
        CodexConfig::new(runtime, codex_home)
            .with_home_owner("image-app")
            .with_image_generation(true),
    )
    .await?;
    let result = codex
        .run(
            "Generate one square PNG of a green circle on a navy background.",
            SessionOptions::read_only(".").with_maximum_output_bytes(32 * 1024 * 1024),
        )
        .await?;
    assert_eq!(result.image_generations.len(), 1);
    codex.shutdown().await
}
```

세션의 retained-output 제한은 assistant text와 image item metadata/base64의 합계에 적용됩니다. 실계정 E2E는 생성 bytes를 임시 파일로만 디코딩하고 별도 `perfectpixel inspect` 실행으로 PNG/JPEG/WebP 디코딩, 크기와 foreground 존재를 검증합니다.

## Text-only adapter

text adapter는 `SessionOptions::read_only(...).text_only()`와 전용 base/developer instruction을 사용해야 합니다. 이 설정은 실행·외부 context surface를 끄지만 sandbox를 대체하지 않으므로 live event와 durable audit을 함께 확인해야 합니다.

`ifsc_text_provider`의 환경, stdin/stdout schema와 실패 의미는 [IFSC provider 계약](docs/IFSC_TEXT_PROVIDER.md)을 따릅니다.

## 주요 API

- 연결·실행: `Codex`, `CodexConfig`, `Session`, `Run`, `SessionOptions`, `Sandbox`
- 계정·모델: `Account`, `Login`, `LoginMethod`, `Model`
- 이벤트·결과: `Event`, `RunResult`, `ImageGeneration`, `ImageGenerationFailure`, `TurnStatus`, `TurnAudit`, `Usage`, `Diagnostic`
- runtime: `RuntimeResolver`, `RuntimePackage`, `ResolvedRuntime`, `RuntimeOrigin`, `DownloadPolicy`
- 오류: `Error`, `ErrorKind`, `Result`; 비멱등 결과가 불명확하면 `OutcomeUnknown`

전체 public surface는 [src/lib.rs](src/lib.rs)가 기준입니다.

## 문서

- [아키텍처](docs/ARCHITECTURE.md): 책임, 상태 owner, 시작·종료 흐름
- [프로토콜 계약](docs/PROTOCOL_CONTRACT.md): wire, sandbox, 재시도와 terminal 의미
- [IFSC provider 계약](docs/IFSC_TEXT_PROVIDER.md): command 환경, schema와 실패 의미
- [검증](docs/VERIFICATION.md): 로컬·runtime·실계정 검증 명령과 현재 증거
- [배포](docs/RELEASE.md): GitHub와 crates.io gate
- [보안](SECURITY.md): 지원 경계와 credential 취급

라이선스는 Apache-2.0입니다.
