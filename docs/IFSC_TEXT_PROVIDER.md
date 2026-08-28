# IFSC ScreenProgram text provider

`ifsc_text_provider`는 Vergerail의 고정 Codex app-server를 IFSC의 교체 가능한 text-provider command로 연결하는 one-shot 실행 파일입니다. 모델은 PNG나 HTML을 만들지 않습니다. 정적인 `ScreenProgram` JSON만 제안하며, IFSC가 이를 다시 검증하고 자체 compiler와 잠긴 Chromium으로 렌더링합니다.

## 빌드와 실행 조건

```bash
cargo build --locked --release --bin ifsc_text_provider
```

필수 환경 변수:

- `VERGERAIL_WORKSPACE`: 존재하는 읽기 전용 작업 디렉터리
- `VERGERAIL_MODEL`: model catalog에서 정확히 일치하는 visible model

선택 환경 변수:

- `VERGERAIL_CODEX_PACKAGE`: 명시한 고정 runtime package만 사용
- `VERGERAIL_IFSC_RUNTIME_DOWNLOAD`: `never`(기본값) 또는 `if-missing`
- `VERGERAIL_IFSC_TURN_TIMEOUT_MS`: 5,000..=1,800,000, 기본값 600,000

기본값은 실행 중 예기치 않은 runtime 다운로드를 하지 않습니다. 관리 cache가 없으면 검증된 package를 `VERGERAIL_CODEX_PACKAGE`로 주거나 설치를 명시적으로 `if-missing`으로 허용해야 합니다.

provider는 표준 `~/.codex`의 ChatGPT 앱 또는 `codex login` 인증을 재사용합니다. signed-out이면 브라우저를 임의로 열지 않고 `authentication-required`로 실패합니다.

```bash
export VERGERAIL_MODEL=gpt-5.6-luna
export VERGERAIL_WORKSPACE="$PWD"
cargo run --locked --example live_e2e
```

일회용 OAuth URL, 계정 선택, MFA는 사용자가 직접 처리합니다. URL이나 credential을 request, 로그, 저장소에 보관하지 않습니다.

## stdin 요청

stdin은 최대 512 KiB인 JSON 값 하나입니다. unknown field는 허용하지 않습니다.

```json
{
  "schemaVersion": 1,
  "operation": "screen-program-proposal",
  "idempotencyKey": "screen-home-default-0001",
  "prompt": "Create a calm product screen.",
  "promptAst": {
    "screenId": "screen.home",
    "stateId": "default",
    "viewport": {
      "width": 1440,
      "height": 1024,
      "deviceScaleFactor": 1
    },
    "imageProfile": {
      "requiredSections": [
        { "elementId": "hero", "kind": "landmark" },
        { "elementId": "hero-title", "kind": "heading" },
        { "elementId": "start-project", "kind": "button" }
      ],
      "copyContract": {
        "mode": "no-readable-copy",
        "exactText": []
      }
    }
  },
  "output": { "width": 1440, "height": 1024, "format": "png" },
  "constraints": {
    "schemaVersion": 1,
    "staticOnly": true,
    "maximumNodes": 128,
    "maximumProgramBytes": 262144,
    "exactViewport": true,
    "externalResources": false
  }
}
```

주요 상한:

- `idempotencyKey`: control character 없는 8..=256 bytes
- `prompt`: 1..=393216 bytes
- viewport: 각 축 1..=8192, 전체 33,554,432 pixels 이하, PNG만 허용
- required section: 1..=64개
- node: 1..=128개
- program: 1024..=262144 bytes 범위에서 caller가 지정

`copyContract.mode`는 `no-readable-copy` 또는 `contract-copy-only`입니다. 후자는 `exactText`에 `{ "elementId", "text" }`를 최대 64개 넣고 model output이 그 문구만 사용하도록 제한합니다.

출력은 정확히 stdout의 JSON 값 하나입니다. 성공 시 exit code는 0입니다.

```json
{
  "schemaVersion": 1,
  "requestId": "screen-home-default-0001",
  "screenProgram": {},
  "usage": {
    "inputTokens": 0,
    "cachedInputTokens": 0,
    "outputTokens": 0,
    "reasoningOutputTokens": 0,
    "totalTokens": 0,
    "providerAttempts": 1
  }
}
```

실패 시 exit code는 1(실행·인증·모델·응답 실패) 또는 2(잘못된 입력·설정)이며, stdout은 동일하게 typed JSON 하나입니다. stderr는 프로토콜 출력으로 사용하지 않습니다.

```json
{
  "schemaVersion": 1,
  "error": {
    "code": "authentication-required",
    "message": "the standard Codex account is signed out",
    "retryable": false,
    "requestId": "screen-home-default-0001"
  }
}
```

stdout write 자체가 실패해도 exit code는 1입니다.

## 안전 경계

- persistent `read_only().text_only()` session만 사용합니다.
- command, file change, approval, warning, 지원하지 않는 event를 관찰하면 요청을 실패시킵니다. approval은 항상 거부합니다.
- terminal 뒤 `audit_turn()`으로 command/file effect가 durable history에도 없음을 확인합니다.
- 모델 응답은 256 KiB, 128 node, 단일 root, 정확한 viewport, parent containment, 허용 tag/style, 정적 behavior, 필수 semantic element, copy contract를 다시 검사합니다.
- 첫 모델 응답이 JSON 또는 ScreenProgram 검증에 실패하면 같은 read-only text-only session에 내부 검증 오류만 전달해 한 번 교정합니다. 두 번째 응답도 실패하면 그대로 typed failure로 종료하며 계약을 느슨하게 하거나 결과를 저장하지 않습니다.
- HTML, JavaScript, URL, image bytes, external resource, CSS escape는 출력할 수 없습니다.
- provider 종료·timeout은 실패이며, 성공으로 대체하지 않습니다.

## 검증

```bash
cargo test --locked --bin ifsc_text_provider
cargo test --locked --test ifsc_text_provider_protocol
cargo clippy --locked --bin ifsc_text_provider -- -D warnings
```

이 검사는 JSON 경계, 크기 제한, unknown field, 약화된 constraint, effect/CSS escape, invented copy, bounds escape를 runtime이나 mock 모델 없이 검증합니다. 실제 모델 품질과 인증 상태는 `scripts/verify.sh --release`의 live E2E로 검증합니다.

빈 입력 process smoke:

```bash
cargo run --locked --bin ifsc_text_provider </dev/null
```

기대 결과는 exit `2`와 `error.code = "invalid-request"`인 JSON 한 줄입니다.
