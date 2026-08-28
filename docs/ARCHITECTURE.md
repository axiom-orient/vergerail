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
고정된 ChatGPT Images endpoint를 직접 호출합니다. 이미지 요청은 모델 turn이나
tool selection을 만들지 않습니다. app-server가 export한 short-lived token은
신뢰된 로컬 Vergerail process의 메모리에서만 사용됩니다. endpoint가 HTTP 401을
반환할 때만 이를 권위 있는 인증 거부로 간주해 app-server 인증을 다시 요청하고
한 번 재시도하며, 그 외 상태 코드는 재시도하지 않습니다. 첫 요청과 이 단일
재시도는 같은 `x-codex-image-turn-id` correlation value를 사용합니다. 이
헤더는 표준 idempotency 보장을 의미하지 않으며, Vergerail은 일반적인 이미지
재실행을 자동화하지 않습니다. endpoint timeout은 operation의 남은 deadline을
사용합니다. 응답은 PNG 구조·CRC·zlib·크기와 정확히 한 장인지를 검증한 뒤
caller에 전달합니다.

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
5. provider child는 명시된 managed home을 `CODEX_HOME`으로 받습니다. 이 디렉터리는
   호출자가 미리 로그인해야 하며 Vergerail은 credential을 복사하지 않습니다.
6. initialize handshake가 성공한 뒤에만 `Codex`가 반환됩니다.

Vergerail은 표준 Codex 설정이나 credential을 수정하지 않습니다. workspace는 symlink가 아닌 기존 directory로 canonicalize하며 session config로만 전달합니다.

## 세션과 종료

각 session은 sandbox, persistence, instruction, output schema, turn deadline과 retained-output 상한을 소유합니다. read-only text/image adapter는 허용하지 않은 command, file change, approval 또는 외부 surface를 관찰하면 실패합니다. persistent success는 durable audit과 대조합니다.

`Session::close()`와 `Codex::shutdown()`은 owned request/task/process를 닫고 guardian을 reap한 뒤 helper와 임시 디렉터리를 제거합니다. timeout, cancellation, panic과 partial spawn도 같은 cleanup 경계를 사용합니다. PID만 기억한 신호나 system-wide process 검색은 사용하지 않습니다.

## 고정 입력과 생성 산출물

`runtime/pinned-macos-aarch64.json`, `protocol/codex-0.150.1/` schema·provenance·checksum은 고정 입력입니다. generated build/package/coverage 파일은 source가 아니며 `scripts/clean.sh`로 제거합니다. app-server가 관리하는 로그인 home과 외부 runtime cache는 사용자 소유 외부 상태입니다.

이미지 옵션은 app-server 바이너리의 확장이 아니라 Vergerail의 공식 인증
adapter가 소유합니다. 따라서 이 저장소는 app-server를 빌드하거나 패치하지
않으며, runtime lock은 공식 package와 protocol schema의 검증에만 사용합니다.
