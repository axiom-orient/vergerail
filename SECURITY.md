# Security policy

## 지원 경계

Vergerail runtime 실행은 Apple silicon macOS와 저장소에 고정된 Codex `0.150.1` package를 대상으로 합니다. package manifest, entrypoint, bundled path와 checksum을 실행 전에 검증하고 spawn 직전 전체 locked file-set/hash/permission을 다시 확인합니다. 각 locked artifact에는 설치된 pinned package에서 관찰한 strict byte ceiling이 있으며, hash 전에 크기를 확인하고 chunk hash 중에도 초과를 거부합니다. managed cache는 기존 symlink component를 따라가지 않습니다. Linux build는 runtime을 실행하지 않는 정적 계약 검증만 의미합니다.

이미지 응답의 압축 해제 scanline raw data는 정확히 14 MiB를 넘을 수 없고
상한은 row allocation 전에 검사됩니다. billed image 요청이 dispatch된 뒤
caller가 취소하거나 결과를 잃으면 `OutcomeUnknown`으로 connection을 종료하며
재시도하지 않습니다. explicit HTTP 401만 같은 turn ID로 한 번 재인증합니다.
`timeoutMs`는 runtime 검증·connect·operation이 공유하는 하나의 monotonic
deadline입니다. deadline 만료 뒤 shutdown은 별도의 고정 2초 teardown budget으로
bounded하게 시도하며 이 budget은 `timeoutMs`에 포함되지 않습니다. 최종 재검증과 `exec` 사이 same-UID path replacement
race는 safe-Rust/path 실행만으로 절대 보장할 수 없는 잔여 범위입니다.

## 인증

Vergerail은 인증 파일이나 browser cookie/profile을 직접 읽지 않습니다. 일반
library와 UpAgent provider는 공식 app-server가 선택한 Codex account를 사용합니다.
별도의 Vergerail account 경로는 없고 upstream `CODEX_HOME`이 있으면 그대로
상속합니다. 선택된 account는 ChatGPT 앱 또는 `codex login`으로 이미 로그인되어
있어야 합니다.

이미지 요청에서 공식 app-server는 `getAuthStatus`의
`includeToken=true, refreshToken=true` 결과로 short-lived access token과 JWT의
bounded ChatGPT account claim을 신뢰된 로컬 Vergerail process에 의도적으로
export합니다. Vergerail은 이를 메모리에서 endpoint 요청에만 사용하고 파일·로그·
provider 응답에 저장하지 않습니다. 이 로컬 export는 credential 파일을 직접
복사하는 방식이 아닙니다.

인증 전달은 loopback MCP server나 MCP connector claim에 의존하지 않습니다.
현재 trusted-origin gating 때문에 loopback MCP 경로의 auth propagation은
사용할 수 없으며, 그것은 Vergerail 이미지 아키텍처의 경로가 아닙니다.

`Codex::login()`은 app-server의 공식 로그인 흐름만 시작합니다. URL 열기, 계정 선택, MFA는 host와 사용자가 수행합니다. `Codex::logout()`은 공유 표준 계정에 영향을 주므로 host가 명시적으로 요청한 경우에만 호출해야 합니다.

## 실행 권한

- 기본 `Codex::run()`은 ephemeral read-only, network-off, approval-deny입니다.
- workspace write는 명시적인 persistent session과 caller approval이 필요합니다.
- provider는 한 요청만 읽고 stdout JSON 한 값을 쓰며 credential을 출력하지 않습니다.
- guardian은 Vergerail이 만든 owner-only 임시 디렉터리에서 실행되고 종료 시 helper와 디렉터리를 제거합니다.
- 오류와 stderr는 credential 표식을 redaction하고 크기를 제한합니다.

보안 문제에는 재현 조건, 영향 범위, 사용한 runtime/package 버전을 포함하고 공개 issue 대신 저장소 보안 연락 경로를 사용하세요.
