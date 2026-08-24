# 보안 정책

Vergerail이 실행하는 Codex app-server는 파일을 읽고 명령을 실행할 수 있습니다. 명시적으로 workspace 쓰기를 허용하면 파일 변경도 요청할 수 있습니다. runtime, `CODEX_HOME`, prompt, model 출력, 승인 요청을 신뢰 경계로 다루세요.

## 지원 범위

- Codex `0.149.1`
- `aarch64-apple-darwin`
- upstream commit `ff29a44391deccde0aba0f8390337d7f3c319ea4`

다른 버전과 target은 낙관적으로 실행하지 않고 거부합니다.

## 지키는 규칙

- 고정한 공식 runtime과 hash가 같은 regular file만 실행합니다.
- 다운로드 archive의 길이와 SHA-256을 확인하고 허용한 파일만 풉니다. runtime cache root와 lock은 non-symlink identity를 확인하고, stale cleanup은 Vergerail 전용 temporary prefix만 제거합니다.
- aarch64 macOS 실행은 build-packaged guardian이 private session/pgrp를 만들고 Codex leader를 unreaped anchor로 유지한 뒤 libproc scan이 끝난 후에만 reap합니다. Rust에는 guardian Child가 이미 reaped된 뒤 숫자 PGID를 신호하는 경로가 없습니다.
- `RuntimeResolver::resolve()`만 다운로드할 수 있습니다.
- 실행 전에 파일 종류, 권한, hash, manifest, target, version, schema를 검사합니다.
- 빈 홈 또는 configured consumer owner와 정확히 일치하는 `.vergerail-managed-home` v2 marker가 있는 홈만 전용 `CODEX_HOME`으로 허용합니다. 다른 owner, owner 필드가 없는 marker, marker 없는 config/state/auth/database 홈은 mutation 전에 거부합니다. project config transaction은 caller 취소와 무관하게 전체 managed-home capability와 배타적 file lock을 commit 종료까지 유지합니다.
- 일회성 실행은 read-only이고 network를 막습니다.
- workspace 쓰기는 명시적 옵션과 승인 처리가 필요합니다.
- text-only session은 execution/external-context 기능을 끄고 cwd를 project trust에 영구 등록하지 않지만, caller의 live/durable effect 감사 책임을 대체하지 않습니다.
- 추가 permission 요청은 typed evidence로 공개하되 Vergerail API에서 승인할 수 없고 거부만 할 수 있습니다.
- 알 수 없는 역방향 요청은 거부합니다.
- frame, queue, 누적 assistant output, 진단, stderr 보관 크기와 provider turn lifetime을 제한합니다.
- 제한을 넘는 단일 stderr line은 원문을 분할 보관하지 않고 폐기하여 chunk 경계가 redaction을 우회하지 못하게 합니다.
- 흔한 authorization, token, cookie, password 형식은 보관 전에 가립니다.
- 보낸 비멱등 요청의 결과가 불확실하면 재시도하지 않고 `OutcomeUnknown`을 반환합니다. 성공 표식과 pending 소유권 해제를 한 임계구역에서 확정해 caller cancellation·timeout·response handoff가 경쟁해도 원격 side effect를 owner 없이 남기지 않습니다.
- 비멱등 create 응답이나 typed notification이 고정 schema를 위반해 원격 상태 소유권을 잃으면 연결을 종료합니다.
- durable turn audit은 정확한 completed turn의 full history만 받아들이며 missing/non-completed status, partial history와 malformed known item을 거부합니다.

## credential 경계

ChatGPT 로그인, token 저장과 갱신은 app-server가 맡습니다. Vergerail은 `auth.json`, access token, refresh token, Chrome cookie, 브라우저 profile을 읽거나 반환하지 않습니다.

Vergerail 앱마다 고유한 `CodexConfig::with_home_owner`를 정하고 새 전용 `CODEX_HOME`에서 로그인하세요. 다음 방식은 지원하지 않습니다.

- 일반 `~/.codex`를 Vergerail 홈으로 사용
- 기존 `auth.json`을 전용 홈으로 복사
- 같은 홈을 여러 machine에서 공유하거나 여러 process/client가 동시에 소유
- 한 Vergerail 소비자의 홈을 다른 owner ID로 재사용

refresh token을 복사해도 서버에서는 독립된 session이 아닙니다. 한 복사본의 갱신이나 logout이 다른 복사본을 무효화할 수 있습니다.

## 신고

비공개 보안 신고는 GitHub Security Advisories의 private vulnerability
reporting을 사용하세요:

<https://github.com/axiom-orient/vergerail/security/advisories/new>

취약점 세부 정보, credential, OAuth URL, device code, prompt secret,
`CODEX_HOME`과 token은 public issue나 source tree에 남기지 마세요. 신고
페이지를 사용할 수 없는 경우에도 민감한 내용을 public issue에 게시하지
말고, repository owner가 제공하는 비공개 보안 채널을 사용하세요.

## 지원하지 않는 보안 구성

수정된 runtime, symlink package 파일, 불안전한 권한, 검증하지 않은 운영체제, `danger-full-access`, 임의 writable root, 자동 network 허용, plugin, MCP server, hook, experimental app-server 기능에는 보안 보장을 제공하지 않습니다. Codex descendant가 의도적으로 private session을 벗어나면 Vergerail은 그 외부 process group의 containment를 주장하지 않으며 guardian cleanup failure/unknown으로 처리합니다.
