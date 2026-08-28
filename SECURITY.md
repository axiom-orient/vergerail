# Security policy

## 지원 경계

Vergerail runtime 실행은 Apple silicon macOS와 저장소에 고정된 Codex `0.150.1` package를 대상으로 합니다. package manifest, entrypoint, bundled path와 checksum을 실행 전에 검증합니다. Linux build는 runtime을 실행하지 않는 정적 계약 검증만 의미합니다.

## 인증

Vergerail은 app-server를 표준 Codex 상태에서 실행합니다. 따라서 ChatGPT 앱 또는 `codex login`이 만든 `~/.codex` 로그인을 재사용합니다. `auth.json`, browser cookie/profile, access token을 직접 읽거나 복사하거나 저장소에 기록하지 않습니다. 별도 인증-home, owner marker, 인증 마이그레이션 경로는 지원하지 않습니다.

`Codex::login()`은 app-server의 공식 로그인 흐름만 시작합니다. URL 열기, 계정 선택, MFA는 host와 사용자가 수행합니다. `Codex::logout()`은 공유 표준 계정에 영향을 주므로 host가 명시적으로 요청한 경우에만 호출해야 합니다.

## 실행 권한

- 기본 `Codex::run()`은 ephemeral read-only, network-off, approval-deny입니다.
- workspace write는 명시적인 persistent session과 caller approval이 필요합니다.
- provider는 한 요청만 읽고 stdout JSON 한 값을 쓰며 credential을 출력하지 않습니다.
- guardian은 Vergerail이 만든 owner-only 임시 디렉터리에서 실행되고 종료 시 helper와 디렉터리를 제거합니다.
- 오류와 stderr는 credential 표식을 redaction하고 크기를 제한합니다.

보안 문제에는 재현 조건, 영향 범위, 사용한 runtime/package 버전을 포함하고 공개 issue 대신 저장소 보안 연락 경로를 사용하세요.
