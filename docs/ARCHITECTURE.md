# Vergerail 아키텍처

## 정체성

Vergerail은 Codex app-server 하나를 Rust API로 감싼 라이브러리입니다. model provider 추상화나 직접 HTTP client가 아닙니다.

```text
공개 Codex/session/event API
  → Codex 프로토콜 변환
  → JSON-RPC router와 JSONL transport
  → 검증된 로컬 Codex app-server 자식 프로세스
```

app-server는 인터넷에 공개하는 daemon이 아닙니다. Vergerail이 프로세스를 소유하고 stdin/stdout으로만 통신합니다.

`src/bin/ifsc_text_provider.rs`는 library 위에 놓인 bounded adapter입니다. stdin JSON을 검증한 뒤 text-only read-only session 하나를 실행하고, live event와 durable audit을 모두 검사한 정적 `ScreenProgram`만 stdout JSON으로 반환합니다. runtime·home·process·session 상태를 별도로 소유하지 않고 아래 library owner를 사용합니다.

## 누가 무엇을 소유하는가

Codex app-server가 소유하는 것:

- ChatGPT 로그인, token 저장과 갱신
- 전용 `CODEX_HOME`의 인증 상태
- 모델 사용 권한
- thread와 turn
- 도구 실행과 sandbox
- 승인 요청 생성

Vergerail이 소유하는 것:

- runtime과 protocol 버전 고정
- runtime 파일과 hash 검증
- 소비자 owner가 포함된 전용 `CODEX_HOME` v2 marker, 설정, client lifetime 동안의 배타적 소유권
- 자식 프로세스 생명주기
- JSON-RPC 요청과 응답 연결
- session과 run event 분배
- 승인 실패 시 안전한 거부
- 공개 오류와 event 의미

Vergerail은 `auth.json`, Chrome cookie, 브라우저 profile, access token을 읽지 않습니다. 기존 인증 파일을 복사하는 흐름도 지원하지 않습니다. 전용 홈은 app-server의 로그인 절차로 독립적으로 인증해야 합니다.

### 내부 상태 소유권

| 경계 | 소유 상태와 책임 |
|---|---|
| `runtime/lock.rs` | embedded JSON에서 runtime identity·download metadata를 파싱하고 identifier·path·hash 불변식 검증 |
| `runtime.rs` | package layout·권한·artifact hash·manifest·schema·실행 version 검증 |
| `runtime/manager.rs` | system/cache 후보 탐색, bounded download, allowlist extraction, atomic install과 cache 재사용 |
| `client.rs`의 `ClientInner` | outbound use case와 shutdown 순서를 조정하는 coordinator. 원시 상태 collection을 직접 소유하지 않음 |
| `client/router.rs` | inbound JSON-RPC 분류, notification/reverse-request adapter, protocol 위반 격리 |
| `private/request.rs` | pending request id, response handoff, timeout/cancellation과 비멱등 결과 소유권 |
| `account.rs`의 `LoginRegistry` | login ID별 terminal 결과와 waiter 수명, bounded early completion |
| `session.rs`의 `SessionRegistry` | loaded thread ID와 create/resume/read/unsubscribe/shutdown lifecycle |
| `session/run_state.rs` | thread별 단일 active turn, pre-ack replay, event/terminal 전이, interrupt single-flight |
| `event.rs`의 `DiagnosticBuffer` | run 밖의 bounded diagnostic queue와 overflow 정책 |
| `private/connection.rs` | graceful closing admission, 최초 causal disconnect와 후속 stderr 보강 |
| `private/process.rs` | owned macOS guardian child, stdin writer, stdout/stderr task, bounded stderr capture |
| `config/home.rs` | managed-home marker/lock, project set, atomic config/state commit |
| `approval.rs` / `approval/protocol.rs` / `approval/respond.rs` | 공개 decision, pinned provider JSON decoding, fail-closed response I/O |
| `bin/ifsc_text_provider.rs` | IFSC request/output validation, one-shot orchestration, typed JSON process boundary |

`runtime/pinned-macos-aarch64.json`이 실행 경로가 읽는 runtime identity·download metadata의 단일 source of truth입니다. managed cache 경로와 installation lock 이름은 그 lock의 version·target에서 함께 유도합니다. package manifest의 `resourcesDir`와 `pathDir`는 실제 process 경계가 사용하는 `codex-resources`와 `codex-path`에 정확히 일치해야 합니다.

`ClientInner`는 이 owner들을 조정하지만 각 owner 내부 collection이나 상태 전이를 직접 변경하지 않습니다. 이 경계가 파일 크기보다 거대 객체 위험을 판단하는 기준입니다.

## 시작과 종료

```text
설정 검증
→ host와 schema 확인
→ runtime 파일·권한·hash 확인
→ 제한 시간 안에 codex --version 확인
→ 전용 홈 준비
→ codex app-server --listen stdio:// --strict-config 실행
→ initialize / initialized
→ 계정·session·run 작업
→ 활성 작업 중단
→ session 구독 해제
→ stdin 닫기
→ 제한 시간 대기
→ owned guardian에 TERM을 보내 guardian이 private session/pgrp를 scan·teardown
→ 자식과 task 회수
```

runtime 다운로드는 이 흐름 앞의 `RuntimeResolver::resolve()`에서만 일어납니다. 연결과 요청 API는 다운로드하지 않습니다. runtime cache root는 real directory handle로 identity를 재확인한 뒤 owner-only 권한을 적용하고, stale cleanup은 Vergerail 전용 prefix에만 한정합니다.

## 동시성

- stdout reader 하나와 직렬화된 stdin writer 하나
- JSON-RPC id로 찾는 pending request registry 하나
- run마다 크기가 제한된 event channel과 terminal watch channel
- session 하나당 활성 run 하나
- app-server 하나에서 여러 session 동시 실행 가능

pending request, run route, session ID, login, diagnostics, connection failure, task handle처럼 **메모리 상태만** 보호하는 경계는 `std::sync::Mutex`를 사용하며 guard가 I/O나 `.await`를 가로지르지 않습니다. router와 process task handle도 lock 안에서 꺼낸 뒤 join합니다. pending RPC는 `dispatched`, `cancelled`, `successful_response`를 공유해 caller cancellation, timeout, router response handoff의 선후를 판정합니다. 성공 표식과 registry 소유권 해제는 같은 mutex 임계구역에서 일어나므로 timeout이 그 사이의 불완전한 상태를 관찰하지 않습니다. 비멱등 성공 응답이 local caller에 귀속되지 못하면 해당 호출을 `OutcomeUnknown`으로 종료하고 connection을 재사용하지 않습니다.

다음 async mutex만 실제 I/O·소유권 전이를 직렬화하기 위해 유지합니다.

- app-server 전체 session lifecycle: create/resume, 비멱등 `turn/start` ownership handshake, unsubscribe, shutdown
- session별 lifecycle: run start와 close의 교차 방지. lock 순서는 session lifecycle 다음 app-server lifecycle
- managed project commit: `lock_owned()`로 admission을 먼저 얻고 그 owned guard와 `ManagedHome` 전체 소유권을 blocking transaction에 넘겨 `config.toml`과 `vergerail-projects.json`의 lost update·caller cancellation 순서 역전을 방지
- child process lifecycle: Rust는 guardian Child 하나만 wait/TERM/drop 대상으로 소유하고, guardian은 Codex leader를 unreaped anchor로 유지한 뒤 `proc_listpgrppids` scan 후 reap

`ProcessInner`가 guardian Child와 reader/writer/stderr task를 소유하고 `ClientInner`는 process handle과 inbound router task를 소유합니다. aarch64 macOS에서 build-packaged C guardian은 `setsid` private session을 만들고 Codex를 별도 pgrp로 직접 `exec`하며, kqueue parent watcher와 CLOEXEC liveness handle을 먼저 세웁니다. guardian은 Codex leader를 `waitid(..., WNOWAIT)`로 unreaped 상태에 두고 TERM→bounded grace→KILL 뒤 immediate/delayed `proc_listpgrppids` scan이 비어 있을 때만 leader를 reap합니다. Rust에는 reap 뒤 숫자 PGID 신호 경로가 없습니다. shutdown은 handle을 lock 밖으로 이전한 뒤 모두 회수합니다. 공개 `Codex::shutdown`이 한 번 poll되면 cleanup은 별도 owned task가 소유하므로 caller future 취소가 `closing` 상태와 child process를 남기지 않습니다. 시작 응답보다 먼저 도착한 run 알림도 동일한 bounded event capacity를 사용하므로 별도 무제한 queue가 없습니다. queue 포화, 누적 output 초과, active turn deadline은 router를 막지 않는 cleanup task에 interrupt를 맡깁니다. deadline task는 provider terminal watch와 경쟁하므로 정상 terminal 뒤 남지 않습니다.

이 경계가 관찰하는 것은 private session/pgrp입니다. Codex descendant가 의도적으로 `setsid`로 탈출하면 이 helper가 그 외부 pgrp를 소유하거나 종료한다고 주장하지 않습니다. 그런 탈출은 typed guardian failure/unknown cleanup으로 기록되어 caller가 연결을 재사용하지 않도록 합니다.

고정 protocol의 typed notification이나 비멱등 create 응답에서 소유권 식별자가 깨지면 connection-fatal로 처리합니다. run route만 제거하면 실제 runtime turn/thread가 계속 살아 있을 수 있으므로 해당 상태를 session 재사용으로 복구하지 않습니다.

모든 값을 actor로 만들지는 않습니다. 실제 공유 가변 상태, 취소, I/O, 실패 경계만 격리합니다.

- process stdin은 bounded `mpsc`로 한 writer task에 직렬화합니다.
- stdout과 stderr는 별도 task에서 읽고 process shutdown이 회수합니다.
- 승인 응답은 한 task가 request id와 fallback을 소유합니다. 명시 응답과 deadline 중 하나만 전송하며, 전송 실패 시 runtime을 종료합니다.
- provider JSON을 공개 타입으로 바꾸는 함수는 I/O 없이 `Result`를 반환합니다.

## 실행 상태와 이벤트

run route는 boolean 조합이 아니라 다음 상태로 전이합니다.

```text
Starting { bounded deferred events }
  → Replaying { turn_id, bounded deferred events }
  → Active { turn_id }
  → provider turn/completed
  → route 제거

Starting
  → provider turn/completed
  → TerminalBeforeStart { turn_id }
  → turn/start 응답과 id 일치 확인
  → route 제거
```

queue 포화, consumer drop, 누적 output 초과, turn deadline은 route를 즉시 제거하지 않습니다. `pending_failure`에 원래 로컬 실패를 보존하고, 공유 `RunControl`이 사용자 interrupt, run drop, route cleanup, shutdown 사이의 중단 요청을 single-flight로 만듭니다. `Codex::run` caller가 취소되면 one-shot owner guard가 interrupt → provider terminal → unsubscribe를 같은 bounded 경로로 완료합니다. app-server의 `turn/completed`가 원격 terminal의 권위 있는 근거이며, 그때 active flag와 route 소유권을 해제합니다. terminal을 제한 시간 안에 확인하지 못하면 session만 재사용하지 않고 연결을 종료합니다.

외부에서 보이는 event는 `Event` enum, 종료 결과는 `Result<RunResult>`, 내부 수명주기는 `RunPhase` enum으로 분리합니다. atomics는 `Run` handle과 router가 공유해야 하는 active/abandoned 및 interrupt/terminal 신호에만 사용합니다.

실시간 lifecycle 알림이 감사에 충분하지 않은 경우 persistent session은 terminal 뒤 `Session::audit_turn`으로 정확한 한 turn의 durable command/file-change evidence를 읽을 수 있습니다. 이 read는 session lifecycle과 app-server session lifecycle을 같은 순서로 잠가 start/close/shutdown과 교차하지 않습니다. router 내부에서 `thread/read` 응답을 기다리는 교착을 만들지 않고, 모든 terminal에 전체 history를 다시 읽는 누적 O(n²) 경로도 만들지 않습니다.

## 입력·출력과 변환 경계

```text
stdin API 값
→ protocol JSON 생성
→ 크기 검증된 JSONL frame
→ bounded writer channel
→ app-server
→ stdout JSONL frame
→ JSON-RPC 분류
→ protocol JSON 해석
→ typed Event / Result / Diagnostic
```

JSONL reader는 소비 위치를 전진시키고 unread tail만 다음 read 전에 한 번 compact합니다. stderr도 read batch마다 소비한 prefix를 한 번만 compact합니다. 64KiB를 넘는 논리적 stderr line은 원문 조각을 보관하지 않고 고정 placeholder 하나로 대체한 뒤 다음 개행까지 버립니다. 따라서 여러 작은 frame이나 line이 한 buffer에 들어와도 앞부분 `drain` 반복으로 O(n²)가 되지 않고, 잘린 secret prefix·suffix가 따로 redaction을 우회하지 않습니다.

runtime archive 추출은 artifact path를 `HashMap`으로 한 번 색인합니다. 반대로 artifact 5개, diagnostics 128개처럼 상한이 작고 명시된 탐색은 단순 O(n)을 유지합니다. 그 영역에 tree나 actor를 추가하면 실제 비용보다 상태와 오류 경로가 늘어납니다.

## 상태 확정

로컬 상태는 app-server가 성공을 확인한 뒤에만 바꿉니다. 예를 들어 `thread/unsubscribe`가 실패하면 session은 닫힌 것으로 표시하지 않아 다시 시도할 수 있습니다.

전용 홈은 `VERGERAIL-MANAGED-HOME v2`와 consumer owner가 들어간 정확한 `.vergerail-managed-home` marker, `.vergerail-home.lock`의 배타적 advisory lock으로 소유권을 고정합니다. 빈 홈만 marker로 승격하며, 다른 owner·owner 필드가 없는 marker·marker 없는 non-empty 홈은 권한 변경·lock·workdir·state 생성 전에 거부합니다. 충돌 후 남은 빈 lock 파일만 안전하게 회수할 수 있습니다. 같은 홈의 동시 owner도 허용하지 않습니다.

project 설정 transaction은 admission guard와 전체 `ManagedHome`을 blocking worker가 끝까지 보유합니다. 따라서 caller가 취소돼도 file lock이 먼저 풀리거나 뒤 transaction이 추월하지 않으며, 마지막 파일 쓰기까지 성공한 뒤 메모리 상태를 바꿉니다. marker가 있는 홈을 다시 열 때는 `vergerail-projects.json`을 기준으로 Vergerail이 관리한 `config.toml`을 복구하고 사라진 real-directory project만 config/state에서 함께 제거합니다. filesystem 검사 자체가 실패한 entry는 추측해 삭제하지 않습니다. text-only session의 cwd는 canonical validation만 하고 project trust set에 추가하지 않습니다. lock과 임시 파일 권한은 path 재조회가 아니라 열린 file handle에 적용하고 path identity를 다시 확인합니다.

## 안전한 기본값

- 안정 protocol만 사용
- 일회성 실행은 read-only, network 차단
- text-only session은 전용 base/developer instruction field를 보존하고 execution/external-context 기능을 끔
- workspace 쓰기는 명시적 session과 정확한 root 아래에서만 허용
- `danger-full-access` 공개 API 없음
- 알 수 없는 역방향 요청 거부
- 추가 permission 요청은 deny-only; 승인 drop과 timeout도 거부로 처리
- frame, queue, 누적 output, 진단, stderr 보관 크기와 turn lifetime 제한
- 흔한 secret 형식 가림

## 넣지 않은 것

- 범용 `Provider` trait과 plugin registry
- WebSocket과 원격 daemon
- 연결 중 숨은 다운로드
- 임의 runtime lock, 느슨한 단일 바이너리, 검증하지 않은 실행 파일
- 직접 OpenAI/ChatGPT HTTP 호출
- 다른 제공자 adapter
- ROA 호환 계층
