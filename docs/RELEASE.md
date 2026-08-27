# 배포

Vergerail `0.2.0`은 Codex `0.150.1`과 Apple silicon macOS만 지원합니다. GitHub source와 crates.io 배포는 별도 gate입니다.

## 현재 상태

| 경로 | 판정 | 남은 조건 |
|---|---|---|
| local path dependency | `GO` | 없음 |
| GitHub git dependency | `NOT RELEASED` | local candidate review, push 후 consumer full commit SHA 검증 |
| crates.io | `OUT OF SCOPE` | `publish = false`; 별도 게시 결정과 gate가 필요 |

현재 canonical source는 공개 저장소
`https://github.com/axiom-orient/vergerail`입니다. 0.150.1/image candidate는
local `main`에서만 준비 중이고 `origin/main`은 `0601e9e`로 유지됩니다.
remote release branch도 아직 삭제하지 않았습니다. clean committed `HEAD`에서
non-authenticated gate, dedicated-home authenticated image/full E2E와
`scripts/release-verify.sh`가 모두 성공했습니다. 따라서 local path candidate는
GO이며, push와 별도 consumer의 검증된 full commit SHA 고정은 independent review
뒤에 진행합니다.

`Cargo.toml`의 repository, homepage와 documentation URL은 canonical GitHub
source를 가리킵니다. 현재는 crates.io 게시를 하지 않으며, tag와 release도
생성하지 않습니다.

GitHub Actions와 `.github/workflows`는 배포 수단으로 사용하지 않습니다. release 검증은 repository-owned `scripts/verify.sh`를 로컬에서 실행한 결과만 사용합니다.

## 공통 gate

[검증 문서](VERIFICATION.md)의 기본, 공식 runtime, 관리형 runtime, 실제 계정 E2E와 package 검사를 clean checkout에서 통과해야 합니다. source, runtime lock, protocol provenance, tests와 문서를 포함하고 `target`, credential, 전용 `CODEX_HOME`, runtime binary는 포함하지 않습니다.

개발 중 검증은 `scripts/verify.sh`를 사용합니다. 배포 직전에는
`scripts/release-verify.sh`를 사용해야 합니다. 후자는 먼저 `HEAD`가 존재하고
working tree와 index가 모두 깨끗한지 확인한 뒤, aarch64 macOS에서
`VERGERAIL_CODEX_PACKAGE`, 기존 전용 `VERGERAIL_CODEX_HOME`,
`VERGERAIL_HOME_OWNER`, visible `VERGERAIL_MODEL`, `VERGERAIL_WORKSPACE`,
검증할 `VERGERAIL_PERFECTPIXEL_BIN`을
요구합니다. 그 환경으로 개발 gate(공식 runtime 2개와 IFSC signed-out 1개),
managed runtime download/reuse, authenticated live E2E를 직접 실행하고 마지막으로
`cargo package --offline --locked`를 `--allow-dirty` 없이 실행합니다. credential은
읽거나 복사하지 않습니다. committed `HEAD`가 없거나 working tree가 dirty인
checkout에서는 이 release entrypoint가 전제조건 오류로 종료하며, clean committed
`HEAD`에서는 위 external suite와 final package 검증을 수행합니다.

현재 release 결정은 local candidate에 대해 `GO`, GitHub 배포에 대해
`NEEDS_REVIEW`입니다. remote push, release branch 삭제와 배포는 independent
review 및 full-SHA consumer 검증 전에는 수행하지 않습니다.

## GitHub source

1. canonical owner, repository 이름과 공개 범위를 확인합니다.
2. `Cargo.toml.repository`와 문서 URL이 실제 remote에 맞는지 확인합니다.
3. clean commit에서 공통 gate를 다시 실행합니다.
4. commit을 push한 뒤 별도 consumer가 full commit SHA를 고정해 `cargo check --locked`를 통과해야 합니다.

branch나 mutable tag만 고정한 결과는 재현 가능한 배포 증거가 아닙니다. release tag를 만들었다면 이동하거나 force-push하지 않습니다. 문제가 생기면 consumer가 마지막 검증 SHA로 되돌립니다.

## crates.io

GitHub gate 외에 다음이 필요합니다.

- 공개 보안 신고 경로와 security contact
- clean canonical commit과 annotated version tag
- crate 이름 소유권
- `publish = false` 제거

그 뒤에만 실행합니다.

```bash
cargo publish --locked --dry-run
cargo publish --locked
```

게시된 package는 삭제할 수 없습니다. 문제가 있으면 새 설치를 막고 수정 버전을 배포합니다.

```bash
cargo yank --version <published-version>
```

원인이 해결되고 배포 소유자가 승인한 경우에만 `cargo yank --undo --version <published-version>`를 사용합니다.
