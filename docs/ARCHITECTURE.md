# 아키텍처

Vergerail은 검증된 Codex app-server package와 Rust caller 사이의 좁은 stdio JSONL client입니다. 설치, 계정 UI, provider retry 정책, 범용 daemon 역할을 소유하지 않습니다.

## 책임

| 영역 | 책임 |
|---|---|
| `runtime.rs` | 고정 runtime 발견·다운로드·checksum·package 검증 |
| `config.rs` | client timeout, frame 상한, image capability 설정 |
| `client.rs` | app-server handshake, account/model, session 생성과 shutdown |
| `session.rs` | sandbox, instruction, event, approval, terminal/audit 계약 |
| `private/process.rs` | stdio transport, task custody, bounded shutdown |
| `private/process_tree.rs` | owner-only guardian 추출과 process-tree 종료 |
| provider binaries | stdin JSON 검증, 단일 요청 실행, stdout typed JSON |

`Codex::generate_image()`는 session 경계와 별개인 비멱등 이미지 요청입니다.
공식 app-server의 `getAuthStatus(includeToken=true, refreshToken=true)` 결과에서
인증과 bounded ChatGPT account claim을 얻은 뒤, Vergerail의 이미지 adapter가
고정된 ChatGPT Images endpoint를 직접 호출합니다. 이 endpoint와 `gpt-image-2`
pixel model은 pinned Codex `0.150.1` runtime profile의 단일 소유 입력이며,
caller가 model을 바꿀 수 없습니다. 현재 public app-server v2에는 동등한 direct
image RPC가 없으므로 이 경계는 fallback이 아니라 명시된 compatibility adapter다.
이미지 요청은 모델 turn이나 tool selection을 만들지 않습니다. app-server가
export한 short-lived token은 신뢰된 로컬 Vergerail process의 메모리에서만
사용됩니다. endpoint가 HTTP 401을 반환할 때만 이를 권위 있는 인증 거부로
간주해 app-server 인증을 다시 요청하고 한 번 재시도하며, 그 외 상태 코드는
재시도하지 않습니다. 첫 요청과 이 단일 재시도는 같은 `x-codex-image-turn-id`
correlation value를 사용합니다. 이 헤더는 표준 idempotency 보장을 의미하지
않으며, Vergerail은 일반적인 이미지 재실행을 자동화하지 않습니다. endpoint
timeout은 provider request에서 시작한 하나의 monotonic deadline의 남은 시간을 사용합니다. 응답은 PNG 구조·CRC·zlib·크기와
정확히 한 장인지를 검증한 뒤 caller에 전달합니다. PNG decompressed scanline
raw data는 정확히 14 MiB를 넘을 수 없고, 이 상한은 압축 해제 row buffer를
만들기 전에 검사됩니다. dispatch 뒤 caller cancellation이나 deadline으로
결과를 잃으면 image turn ID를 보존한 `OutcomeUnknown`으로 connection을
fence하며 자동 재시도하지 않습니다.

인증 전달은 loopback MCP connector가 아닙니다. trusted-origin gating 때문에
loopback MCP auth propagation은 지원되지 않으며, 이 경로는 공식 app-server
auth RPC와 직접 endpoint 호출만 사용합니다.

## 시작 흐름

1. `RuntimeResolver`가 내장된 공식 runtime lock과 checksum에 맞는 package를
   선택합니다. provider는 사용자 제작 runtime variant나 별도 lock을 받지
   않습니다.
2. `Codex::connect`가 package를 다시 검증합니다.
3. fresh owner-only 임시 디렉터리에 embedded guardian을 추출합니다.
4. guardian이 공식 package의 `bin/codex --listen stdio:// --strict-config`를
   app-server 모드로 직접 실행합니다.
5. provider child는 별도의 Vergerail account 경로를 설정하지 않습니다. upstream
   `CODEX_HOME`이 있으면 상속하고, 공식 app-server가 Codex account를 해석합니다.
6. initialize handshake가 성공한 뒤에만 `Codex`가 반환됩니다.

Vergerail은 표준 Codex 설정이나 credential을 수정하지 않습니다. workspace는 symlink가 아닌 기존 directory로 canonicalize하며 session config로만 전달합니다.

## 세션과 종료

각 session은 sandbox, persistence, instruction, output schema, turn deadline과 retained-output 상한을 소유합니다. read-only text/image adapter는 허용하지 않은 command, file change, approval 또는 외부 surface를 관찰하면 실패합니다. persistent success는 durable audit과 대조합니다.

`Session::close()`와 `Codex::shutdown()`은 owned request/task/process를 닫고 guardian을 reap한 뒤 helper와 임시 디렉터리를 제거합니다. provider는 사용자 operation deadline의 남은 시간으로 shutdown을 시도하고, deadline이 이미 만료되면 별도의 고정 2초 teardown budget으로 bounded cleanup을 시도합니다. 이 teardown budget은 `timeoutMs`에 포함되지 않습니다. timeout, cancellation, panic과 partial spawn도 같은 cleanup 경계를 사용합니다. PID만 기억한 신호나 system-wide process 검색은 사용하지 않습니다.

실행 직전 locked file-set/hash/permission 재검증은 provider binary 안의 작은
검증 모드로 수행하고, embedded guardian이 그 process와 private process group을
소유합니다. parent는 remaining operation deadline으로 기다리며, 만료 시 guardian을
TERM/force/reap하고 helper와 임시 파일을 guardian의 별도 고정 1초 cleanup window 안에서
bounded cleanup 합니다. 이 verifier cleanup window는 provider shutdown의 별도 2초
teardown budget과 구분됩니다. 검증 helper가
없는 실행 파일에서는 fail-closed하며, in-process blocking worker를 deadline 뒤에
남겨두지 않습니다. 완료 뒤와 `exec` 직전에 deadline을 다시 확인해 만료된 작업은
spawn하지 않습니다. locked file-set/hash/permission 재검증과 `exec` 사이의
same-UID path replacement race는 safe-Rust/path API만으로 닫히지 않는 잔여
경계입니다.

## 고정 입력과 생성 산출물

`runtime/pinned-macos-aarch64.json`, `protocol/codex-0.150.1/` schema·provenance·checksum은 고정 입력입니다. generated build/package/coverage 파일은 source가 아니며 `scripts/clean.sh`로 제거합니다. managed runtime cache는 component별 symlink를 거부하고, package는 `tests/fixtures/guardian_survivor_mutant.c`를 canonical listing에 포함하는지 검증합니다. app-server가 관리하는 로그인 home과 외부 runtime cache는 사용자 소유 외부 상태입니다.

최종 locked file-set/hash/permission 재검증과 `exec` 사이의 same-UID path
replacement race는 현재 guardian과 safe-Rust path API만으로 닫을 수 없는 잔여
경계이며, release 보증에서 명시적으로 제외합니다.

이미지 옵션은 app-server 바이너리의 확장이 아니라 Vergerail의 공식 인증
adapter가 소유합니다. 따라서 이 저장소는 app-server를 빌드하거나 패치하지
않으며, runtime lock은 공식 package와 protocol schema의 검증에만 사용합니다.
