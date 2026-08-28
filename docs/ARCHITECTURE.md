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

## 시작 흐름

1. `RuntimeResolver`가 lock과 checksum에 맞는 package를 선택합니다.
2. `Codex::connect`가 package를 다시 검증합니다.
3. fresh owner-only 임시 디렉터리에 embedded guardian을 추출합니다.
4. guardian이 `codex app-server --listen stdio:// --strict-config`를 실행합니다.
5. child는 `CODEX_HOME` override 없이 표준 `~/.codex` 계정 상태를 사용합니다.
6. initialize handshake가 성공한 뒤에만 `Codex`가 반환됩니다.

Vergerail은 표준 Codex 설정이나 credential을 수정하지 않습니다. workspace는 symlink가 아닌 기존 directory로 canonicalize하며 session config로만 전달합니다.

## 세션과 종료

각 session은 sandbox, persistence, instruction, output schema, turn deadline과 retained-output 상한을 소유합니다. read-only text/image adapter는 허용하지 않은 command, file change, approval 또는 외부 surface를 관찰하면 실패합니다. persistent success는 durable audit과 대조합니다.

`Session::close()`와 `Codex::shutdown()`은 owned request/task/process를 닫고 guardian을 reap한 뒤 helper와 임시 디렉터리를 제거합니다. timeout, cancellation, panic과 partial spawn도 같은 cleanup 경계를 사용합니다. PID만 기억한 신호나 system-wide process 검색은 사용하지 않습니다.

## 고정 입력과 생성 산출물

`runtime/pinned-macos-aarch64.json`, `protocol/codex-0.150.1/` schema·provenance·checksum은 고정 입력입니다. generated build/package/coverage 파일은 source가 아니며 `scripts/clean.sh`로 제거합니다. 표준 `~/.codex`와 외부 runtime cache는 사용자 소유 외부 상태입니다.
