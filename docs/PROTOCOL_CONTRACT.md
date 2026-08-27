# 고정 프로토콜 계약

## 버전

| 항목 | 값 |
| --- | --- |
| Codex | `0.150.1` |
| tag | `rust-v0.150.1` |
| commit | `90854393966b21e9ebfd21b122334eb09a20c93d` |
| schema SHA-256 | `a0aac52c9ea4bfcc02a1a323d612fb460e92b35e493695f01e2dd9b6e5072d33` |
| transport | stdio JSONL |
| experimental API | 사용하지 않음 |

실제 wire field는 main branch 문서가 아니라 저장소에 고정한 schema와 fixture를 기준으로 합니다.

## 0.150.1 호환성 검토

이전 고정 schema와 공식 `rust-v0.150.1` tag의 schema를 구조적으로 비교했습니다.
Vergerail이 보내거나 해석하는 `initialize`, account login/read/logout, `model/list`,
thread start/resume/read/unsubscribe, turn start/interrupt와 typed notification의
method, request/response 참조, 필수 routing field는 변경되지 않았습니다. approval
reverse request는 기존처럼 앱 서버의 server-request 경계에서 typed parser로 검증합니다.

새 schema에는 Vergerail이 사용하지 않는 additive API와 field가 포함됩니다.
Vergerail은 이 값을 공개 raw JSON이나 낙관적 기본값으로 노출하지 않고, 기존에
소유한 routing과 완료 상태는 계속 엄격히 검증합니다. 이미지 생성의 시작 알림은
공식 runtime이 아직 상태를 확정하지 않은 payload를 보낼 수 있으므로 빈 `status`를
시작 단계에서만 허용하며, 완료 알림과 durable audit의 상태는 필수로 유지합니다.

## 보내는 요청

- `initialize`, `initialized`
- `account/read`, `account/login/start`, `account/login/cancel`, `account/logout`
- `model/list`
- `thread/start`, `thread/resume`, `thread/read`, `thread/unsubscribe`
- `turn/start`, `turn/interrupt`

`SessionOptions`의 base/developer instruction은 `thread/start`와 `thread/resume`의 `baseInstructions`, `developerInstructions`에 각각 전달합니다. `text_only()`는 같은 요청의 thread-local `config`에서 shell, web search, app, plugin, memory, hook, goal, multi-agent와 관련 external execution surface를 끄고 history persistence를 사용하지 않습니다. 이 config는 session 경계이며 provider 선택이나 workspace authority를 소유하지 않습니다. 이미지 생성은 `CodexConfig::with_image_generation(true)`로 전용 managed home에 명시적으로 opt-in하며 기본값은 false입니다. 각 session은 provider turn deadline과 누적 retained-output byte 상한을 소유합니다. retained output은 assistant text와 item ID별 최신 image-generation metadata/base64의 합계입니다. 기본값은 30분과 8 MiB이며 출력 상한은 1..=64 MiB만 허용합니다.

`ReasoningEffort::Low`는 고정 schema의 `turn/start.params.effort = "low"`로 인코딩합니다. image-generation E2E만 이 값을 명시적으로 선택하고, 다른 session은 기존 기본값 `medium`을 유지합니다.

## 해석하는 알림

- `turn/started`, `turn/completed`
- `item/agentMessage/delta`
- `item/commandExecution/outputDelta`
- `item/fileChange/patchUpdated`
- `item/started`, `item/completed`
- `thread/tokenUsage/updated`
- `error`, `warning`, `configWarning`
- `account/login/completed`

유효하지만 Vergerail이 해석하지 않는 additive run 알림은 `Event::Unknown`으로 전달합니다. 연결 수준 알림은 크기가 제한된 diagnostics에 보관합니다.

Vergerail이 위 목록에서 typed event로 해석하는 알림은 고정 schema의 필수 routing field와 공개 타입에 필요한 필드를 검증합니다.

- `threadId`, `turnId` 또는 typed payload의 필수 필드가 잘못되면 runtime 연결을 종료하고 활성 run을 `Protocol` 오류로 종료합니다.
- command/file item의 필수 `id`, `status`, `cwd`, `changes[].path`를 기본값으로 만들거나 일부만 버리지 않습니다.
- malformed notification만 버리고 route를 해제하면 실제 runtime turn이 계속 실행될 수 있으므로 session 재사용으로 복구하지 않습니다. 새 연결에서만 재시작합니다.
- 알 수 없는 additive method나 지원하지 않는 item type은 provider JSON을 공개하지 않고 opaque event 또는 diagnostic으로 축약합니다.
- `imageGeneration` item은 `Event::ImageGeneration`으로 전달하며 item ID별 최신 lifecycle을 terminal `RunResult::image_generations`에 보존합니다. item 수는 event capacity, text를 포함한 retained bytes는 session output limit로 제한합니다.

## 완료 turn 감사

실시간 item 알림만으로 실행 증거가 부족할 때 persistent `Session::audit_turn`이 안정 `thread/read`를 한 번 호출합니다. `includeTurns: true` 응답에서 요청한 thread ID와 turn ID가 정확히 일치하고 target status가 `completed`이며 `itemsView`가 생략되었거나(고정 schema의 기본값 `full`) 명시적으로 `full`일 때만 command/file-change/image-generation을 공개 타입으로 반환합니다. target turn 부재·중복, missing/non-completed status, turn 안의 duplicate item ID, partial view, malformed known item은 실패하며 모델 설명이나 marker 상태로 대체하지 않습니다.

이 경로는 ephemeral session과 active run에서는 거부됩니다. 긴 thread의 전체 history를 매 terminal마다 다시 읽어 누적 O(n²) I/O를 만들지 않도록 일반 event router가 자동 호출하지 않으며, 감사 증거가 필요한 caller만 명시적으로 사용합니다. 사용자 메시지와 reasoning 원문은 반환하지 않고 non-command/file item의 type만 보존합니다.

## 역방향 요청

다음 네 종류만 typed API로 처리합니다.

- command 실행 승인
- 파일 변경 승인
- permission 요청 확인과 거부
- 사용자 입력 요청

그 밖의 method는 JSON-RPC `-32601`, 잘못된 params는 `-32602`로 답합니다. 처리하지 않은 요청을 승인으로 보거나 pending 상태로 남기지 않습니다.

`PermissionApproval`은 typed 요청 내용을 보여 주지만 `deny()`만 노출합니다. upstream filesystem 배열과 structured entry는 하나의 `PermissionGrant::entries` 목록으로 정규화해 caller가 권한 일부를 놓치지 않게 합니다. 원래 read-only/network-disabled 또는 exact workspace sandbox를 확대하는 permission 응답은 Vergerail이 대신 승인하지 않습니다. command/file 승인은 별도 명시적 decision API를 유지합니다.

사용자 입력 요청의 `isBlocking`은 고정 schema의 필수 boolean입니다. 누락되거나 다른 형식이면 `-32602`로 거부하고, 유효한 값은 `UserInputRequest::is_blocking()`으로 전달합니다.

## sandbox 변환

| Vergerail 옵션 | thread sandbox | approval policy | turn 정책 |
| --- | --- | --- | --- |
| `Sandbox::ReadOnly` | `read-only` | `never` | read-only, network 차단 |
| `Sandbox::WorkspaceWrite` | `workspace-write` | `on-request` | 정확한 root만 쓰기, network 차단 |

text-only는 sandbox의 대체물이 아닙니다. read-only sandbox와 함께 사용하고, terminal 뒤 persistent `thread/read` 감사로 command/file-change 및 다른 effect item 부재를 확인해야 합니다. 고정 안정 schema에는 per-turn maximum output token field가 없으므로 Vergerail은 native token generation cap을 주장하지 않습니다. 대신 누적 text byte 상한을 delta append 전에 집행하고 초과 시 `ResourceLimit`으로 interrupt합니다.

## frame과 재시도

- 한 줄에 UTF-8 JSON 값 하나
- CRLF 허용, 빈 줄 거부
- 기본 frame 제한 16MiB
- queue에 넣기 전에 encode와 크기 검증
- `turn/start` 응답 전 알림도 run event capacity 안에서만 보관
- assistant text와 item ID별 최신 image-generation payload는 frame별 크기와 별도로 합산 누적 byte 상한을 적용
- 부분 write나 flush 실패 뒤 결과를 알 수 없으면 `OutcomeUnknown`
- login/thread/turn 생성처럼 원격 상태를 새로 만드는 비멱등 요청은 자동 재시도하지 않음. `thread/unsubscribe`는 pinned schema의 `notLoaded`·`notSubscribed`·`unsubscribed`를 모두 성공으로 취급해 bounded cleanup에서 안전하게 재호출할 수 있음
- caller cancellation과 timeout은 pending entry 제거와 response handoff 양쪽에서 같은 상태로 판정함. 성공 표식과 pending entry 제거를 하나의 원자적 registry transition으로 공개하며, 요청이 이미 dispatch됐거나 성공 응답이 caller에게 귀속되지 못하면 `OutcomeUnknown`으로 connection을 종료함
- 실패 응답이 이미 확정된 경우에는 원격 성공을 잃은 것이 아니므로 cancellation만으로 `OutcomeUnknown`을 만들지 않음

## turn 중단과 terminal 소유권

한 provider turn에는 최대 한 개의 `turn/interrupt` 요청만 보냅니다. 사용자 호출, `Run` drop, event queue/output limit/turn deadline failure, shutdown이 경쟁해도 같은 interrupt 결과와 terminal 신호를 공유합니다.

- `turn/interrupt` 응답은 중단 요청의 접수 결과이며 원격 turn 종료 자체의 근거가 아닙니다.
- `turn/completed`를 provider terminal의 권위 있는 근거로 사용합니다.
- terminal이 interrupt 응답보다 먼저 오면 완료된 turn에 두 번째 interrupt를 보내지 않고 terminal을 채택합니다.
- queue failure 뒤에도 route와 thread/turn 소유권을 terminal까지 유지합니다.
- terminal을 shutdown deadline 안에 확인하지 못하면 해당 session을 재사용하지 않고 연결을 종료합니다.
- router task는 자신이 처리해야 할 `turn/interrupt` 응답을 기다리지 않습니다. cleanup은 별도 task에서 수행합니다. 취소된 `Codex::run`도 provider terminal 확인 후 ephemeral thread를 unsubscribe하며, 이 복구가 실패하면 connection을 재사용하지 않습니다.

## login terminal 상태

`account/login/completed`, 명시적 cancel 응답, 연결 종료가 경쟁할 수 있습니다. Vergerail은 login id별 첫 terminal 결과만 확정합니다.

- 성공 뒤 늦게 온 cancel은 성공을 덮어쓰지 않습니다.
- cancel 뒤 늦게 온 성공도 cancel 결과를 덮어쓰지 않습니다.
- login start 응답보다 completion 알림이 먼저 도착하는 wire race는 제한된 early-completion buffer로 연결합니다.
- Vergerail이 시작한 ChatGPT login completion의 `loginId` 또는 `success`가 없으면 결과를 안전하게 연결할 수 없으므로 connection-fatal protocol 오류로 처리합니다.
- handle이 살아 있는 동안 terminal 결과를 보존하고, handle drop 시 관련 상태와 waiter를 제거합니다. 개별 `Login::wait` future 취소도 해당 waiter를 동기적으로 제거합니다.
- start 응답 직후 process가 끊겨도 login timeout까지 기다리지 않고 연결 종료를 terminal 실패로 관찰합니다.

## 비멱등 create 응답

`account/login/start`, `thread/start`, `thread/resume`, `turn/start`는 성공 응답 뒤 Vergerail이 소유권 식별자를 확정해야 합니다. 성공 응답이 필수 `loginId`, `thread.id`, `turn.id` 또는 flow 필드를 잃으면 생성된 원격 상태를 추적할 수 없으므로 연결을 종료합니다. 임의 식별자 생성, silent fallback, 같은 요청 재시도는 하지 않습니다.

## cleanup 오류

본 작업과 cleanup이 모두 실패하면 본 작업의 `ErrorKind`, operation, RPC code를 유지합니다. cleanup 실패는 message와 필요 시 stderr에 관련 오류로 추가합니다. cleanup 실패 때문에 더 중요한 원래 실패가 사라지지 않습니다.
