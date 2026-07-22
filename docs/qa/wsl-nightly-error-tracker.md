# Colay WSL/Windows Nightly Error Tracker

이 문서는 WSL Linux와 Windows에서 nightly Colay를 실제 사용하면서 발견한 오류와 개선
후보를 지속적으로 누적하는 메모다. 오류를 재현했다고 해서 수정 완료로 간주하지 않으며,
각 항목은 증거, 영향, 임시 우회, 제품 개선안, 검증 조건을 분리해서 기록한다. 기존
`WSL-*` ID는 이력 안정성을 위해 유지하되 Windows에서도 재현되면 공통 이슈로 표시한다.

## Tracking metadata

- 최초 작성: 2026-07-22 (Asia/Seoul)
- 마지막 갱신: 2026-07-22
- 대상 환경: WSL 2 Ubuntu 24.04 x86-64, Windows 11 Home 10.0.26100 x86-64
- 확인한 nightly: `0.1.1-nightly.20260721.8c7f638`
- Windows PATH 설치본: Cargo 설치 `colay 0.1.0` (nightly와 불일치)
- 기본 원칙: 실제 provider inference를 QA에서 호출하지 않는다.
- 상태 값: `open`, `workaround-confirmed`, `fix-in-progress`, `fixed`, `closed`

## Issue index

| ID | 심각도 | 상태 | 요약 |
| --- | --- | --- | --- |
| `WSL-001` | medium | fixed | NVM/Node 버전 및 비대화형 PATH 불일치 |
| `WSL-002` | high | fixed | daemon startup phase, bounded probe wait, exact child cleanup 적용 |
| `WSL-003` | high | fixed | WSL/Windows idle daemon의 반복 `BEGIN IMMEDIATE`로 direct writer starvation |
| `WSL-004` | high | fixed | WSL/Windows non-Git 위치에서 task 영속화 후 raw Git 128 오류 |
| `WSL-005` | high | fixed | WSL/Windows unborn HEAD에서 raw `Needed a single revision` 오류 |
| `WSL-006` | medium | workaround-confirmed | WSL Git이 `/mnt/c` Windows checkout 줄바꿈을 대량 변경으로 인식 |
| `WSL-007` | low | fixed | chat TUI reconnect 테스트의 고정 500ms 타이밍 플래이크 |
| `WSL-008` | high | fixed | provider 오류/실행 중단 후 장기 lease가 남아 `resume` 충돌 |
| `WIN-001` | medium | workaround-confirmed | Windows PATH가 npm nightly 대신 오래된 Cargo `0.1.0`을 선택 |
| `WIN-002` | medium | open | Windows nightly PE에 Authenticode 서명이 없어 OS 신뢰 체인이 없음 |
| `WIN-003` | low | open | Windows 전체 테스트에서 `icacls.exe` 접근 거부가 1회 발생한 플래이크 |

## WSL-001: NVM/Node 및 PATH 불일치

### 관찰

- 패키지는 Node.js 22 이상을 요구한다.
- 최초 설치는 NVM의 Node.js `20.19.6` 아래에 존재했다.
- interactive Bash에서는 `colay`가 NVM 경로에서 확인됐지만, 일반적인
  `wsl.exe ...` 비대화형 실행에서는 NVM이 로드되지 않아 `colay`를 찾지 못했다.
- 이후 Node.js `22.23.1` 아래에도 동일 nightly가 설치됐다.
- 한동안 Node 20으로 시작한 daemon과 Node 22로 시작한 TUI가 동시에 같은 DB를 사용했다.

### 영향

- 실행 방식에 따라 서로 다른 Node/Colay 설치가 선택된다.
- 업그레이드 후에도 기존 daemon은 이전 NVM 경로의 binary로 계속 실행될 수 있다.

### 현재 우회

```bash
nvm install 22
nvm alias default 22
nvm use 22
npm install --global @kimohy/colay@nightly
```

업그레이드 후에는 기존 TUI를 종료하고 daemon을 명시적으로 stop/restart한다.

### 제품 개선 후보

- launcher가 지원되지 않는 Node 버전을 명확한 오류로 거부한다.
- `doctor`가 launcher, native binary, daemon 각각의 실제 경로와 버전을 함께 보고한다.
- daemon 상태에 시작 executable 경로와 Colay build version을 포함한다.
- WSL 비대화형 실행과 NVM 사용법을 설치 문서에 명시한다.

### 수정 구현 및 재검증

- npm launcher가 native binary를 resolve/spawn하기 전에 실제 Node major version을 검사한다.
  Node 22 미만이면 `nvm install 22 && nvm use 22`를 포함한 명확한 오류로 종료한다.
- `doctor`에 `runtime` check를 추가해 실제 native executable 경로, Colay build version,
  target OS/architecture, invocation path를 보고한다. 이 check는 state를 만들지 않는다.
- WSL의 system Node `18.19.1`로 새 launcher를 실행하면 native를 시작하지 않고 지원 버전
  오류를 반환하며, NVM Node `22.23.1`에서는 launcher 8개 테스트가 모두 통과했다.
- 비대화형 shell이 NVM을 source하지 않아 명령 자체를 찾지 못하는 경우는 shell 환경
  설정이므로 PATH에 Node 22 NVM bin을 넣거나 실행 전에 `nvm use 22`를 수행해야 한다.
- 설치된 nightly root/native package는 모두
  `0.1.1-nightly.20260721.8c7f638`로 일치했고 Linux native는 static PIE x86-64였다.

## WSL-002: daemon start timeout과 orphan child

### 재현된 증상

```text
error: daemon did not publish a healthy heartbeat within five seconds
```

- 격리된 임시 repository에서 `daemon start`가 두 번 timeout 됐다.
- 같은 상태에서 `daemon restart`는 online이 됐다.
- timeout을 반환한 이전 child는 종료되지 않았고, 이후 별도로 daemon lease를 획득했다.
- 이미 stop이 성공한 뒤에도 느리게 초기화되던 이전 child가 lease를 획득해 daemon이 다시
  online이 되는 race가 관찰됐다.

### 근본 원인 방향

- `ensure_started`의 고정 timeout은 5초다.
- child는 provider probe를 마치기 전까지 heartbeat/lease를 게시하지 않는다.
- timeout 경로가 spawn한 child를 종료하거나 회수하지 않는다.
- child stdout/stderr가 null로 폐기돼 시작 실패 원인도 남지 않는다.

### 제품 개선 후보

- 최소 bootstrap lease/heartbeat를 provider probe보다 먼저 게시한다.
- startup phase를 `booting`, `probing`, `online`, `failed`로 구분한다.
- timeout 시 정확히 자신이 spawn한 child를 종료하고 종료 확인까지 수행한다.
- stderr 또는 redacted startup diagnostics를 repository state에 보존한다.
- 느린 fake provider probe를 사용하는 회귀 테스트를 추가한다.

### 수정 구현

- schema migration 9에서 daemon instance에 `booting`, `probing`, `online`, `failed` phase와
  redacted `startup_error`를 추가했다. schema 8의 기존 행은 `online`으로 보존된다.
- child는 provider probe 전에 bootstrap lease를 획득하고 별도 startup heartbeat로 lease를
  갱신한다. 서비스 구성이 끝난 뒤에만 `online`으로 전환하며 정상 daemon loop는 같은
  instance 소유권을 재획득하지 않고 이어받는다.
- 부모는 `booting`과 `probing`을 진행 중 상태로 처리하고, 활성 provider 수에 따른 bounded
  probe 예산을 사용한다. 6초 지연 fake Codex probe가 과거 5초 거짓 timeout 없이 Windows에서
  online이 되는 회귀 테스트를 추가했다.
- timeout과 조기 종료 경로는 부모가 보유한 정확한 child PID의 프로세스 트리를 종료하고
  child 종료를 확인한다. 같은 PID의 lease만 `failed`로 기록·해제하며 다른 owner는 건드리지
  않는 테스트를 추가했다.
- child setup 오류는 configured redactor를 거친 뒤 repository DB에 보존된다. detached child의
  장기 stderr pipe는 Windows parent 종료를 막을 수 있어 사용하지 않고 raw provider stderr도
  영속화하지 않는다.
- 수정 커밋 `f88d974`, `26a001d`, `de70216`, `96d8460`에서 Windows lifecycle 3개,
  전체 Rust 418개, npm 65개, fmt와 전체 Clippy `-D warnings`가 통과했다. 실제 provider
  inference는 호출하지 않았고 `WIN-003`의 `icacls.exe` 접근 거부도 재발하지 않았다.

### 완료 조건

- 느린 probe에서도 `start`가 거짓 실패를 반환하지 않는다.
- timeout을 강제해도 child와 lease가 나중에 다시 나타나지 않는다.
- 반복 start/restart/stop 후 관련 프로세스가 남지 않는다.

## WSL-003: SQLite writer starvation (WSL/Windows 공통)

### 재현된 증상

```text
error: SQLite operation failed: database is locked: database is locked: Error code 5
```

### 증거

- daemon과 TUI가 같은 WAL/SHM을 열고 있는 상태에서 발생했다.
- DB `PRAGMA integrity_check`는 `ok`였다. DB 손상은 아니었다.
- 실패한 `colay run hello`는 task 생성, 분석, routing, event reconciliation을 완료하고
  `planned` 상태까지 기록한 뒤 coordinator lease 획득 전에 실패했다.
- 해당 task에는 coordinator lease와 provider attempt가 없었다.
- 활성 daemon 상태에서 무변경 `BEGIN IMMEDIATE` 획득을 1,000회 시도했을 때
  671회가 즉시 `SQLITE_BUSY`였다. 다른 시점의 200회 표본에서는 2회로, 경쟁률은
  daemon 활동에 따라 크게 변했다.
- daemon 기본 command poll은 100ms다.
- idle poll도 session command claim, orchestration command claim, ready-task claim에서
  pending row 존재 여부를 확인하기 전에 `TransactionBehavior::Immediate`를 시작한다.
- Windows 11의 격리 repository에서도 fake-provider daemon이 online인 동안 SQLite
  `timeout=0`으로 `BEGIN IMMEDIATE`/rollback을 500회, 10ms 간격으로 시도했을 때
  81회(16.2%)가 `SQLITE_BUSY`였다. daemon stop 후 동일 표본은 500회 모두 성공했다.
- Windows 표본 전후의 `PRAGMA integrity_check`도 `ok`였으므로 플랫폼별 DB 손상이 아니라
  활성 writer 경쟁으로 보는 것이 타당하다.

### 현재 우회

- direct `colay run`을 사용할 때 TUI를 먼저 종료하고 repository daemon을 stop한다.
- 실패 후 새 task를 무작정 만들지 말고, 이미 `planned`로 남은 task와 lease/attempt 유무를
  먼저 확인한다.
- `orchestrator.db-wal` 또는 `orchestrator.db-shm`을 직접 삭제하지 않는다.

### 제품 개선 후보

- read-only precheck로 pending 후보가 있을 때만 immediate transaction에 진입한다.
- transaction 안에서 후보를 재검증해 TOCTOU 안전성은 유지한다.
- `SQLITE_BUSY`에 bounded retry, jitter, deadline, 구체적인 owner diagnostics를 추가한다.
- direct run이 daemon과 별도 writer로 경쟁하지 않고 durable command를 통해 daemon에
  제출되는 단일-writer 구조를 검토한다.
- task 영속화와 coordinator 확보 사이 실패를 명시적인 recoverable 상태로 기록한다.

### 수정 구현

- `codex/fix-sqlite-writer-starvation`에서 command queue와 scheduler의 idle claim 경로에
  read-only `SELECT EXISTS` 사전검사를 추가했다.
- 후보가 없으면 `BEGIN IMMEDIATE` 없이 `None`을 반환한다. 후보가 보이면 기존 immediate
  transaction에 진입해 같은 조건을 다시 조회하므로 concurrent claim의 단일 승자와
  TOCTOU 안전성은 유지된다.
- 별도 SQLite 연결이 `BEGIN IMMEDIATE`를 보유한 상태에서도 빈 general/session/
  orchestration command queue와 빈 scheduler poll이 `SQLITE_BUSY` 대신 `None`을 반환하는
  Windows 회귀 테스트 2개를 추가했다.
- 수정 커밋 `bf49188`, 전체 Rust 테스트 409개, npm 테스트 65개, fmt와 전체 Clippy
  `-D warnings` 통과로 수정 범위를 검증했다. 실제 provider inference는 호출하지 않았다.

### 완료 조건

- idle daemon/TUI와 direct run을 병행하는 stress test에서 `SQLITE_BUSY`가 발생하지 않는다.
- 실패 주입 후 task가 중복 생성되지 않고 동일 task를 안전하게 재개할 수 있다.
- DB integrity, append-only event chain, exact lease ownership이 유지된다.

## WSL-004: Git 저장소가 아닌 위치의 late failure (WSL/Windows 공통)

### 재현된 증상

```text
fatal: not a git repository (or any of the parent directories): .git
```

### 증거

- Colay/TUI가 `/home/kimohy`에서 시작됐다.
- `/home/kimohy`는 Git repository가 아니지만 `/home/kimohy/.colay` state가 생성됐다.
- `colay run`은 task를 `planned`까지 영속화한 뒤 worktree 생성 시 raw Git 128로 실패했다.
- Windows 11의 격리된 non-Git 디렉터리에서도 native nightly와 fake provider로 같은 raw
  오류를 재현했다. 실패 후 DB integrity는 정상이었지만 `planned` task 1건이 남았다.

### 제품 개선 후보

- writable `run`, `resume`, TUI approval/execution 전에 Git repository와 worktree 지원 여부를
  preflight한다.
- preflight는 DB/task/event/worktree mutation보다 먼저 실행한다.
- 사용자 오류는 `colay run must be executed inside a Git repository`처럼 제품 문맥으로
  변환하고 실행한 Git argv와 안전한 cwd를 진단 데이터로 남긴다.

### 수정 구현

- `codex/fix-git-readiness-preflight`에서 read-only Git readiness 검사를 추가했다.
- direct `colay run`은 `.colay` state와 task를 만들기 전에 repository root와
  `HEAD^{commit}`을 검사한다.
- non-Git 상태는 `direct task execution requires a Git repository`로 분류하며 raw Git 128을
  사용자 오류로 노출하지 않는다.
- Windows 호환 CLI 회귀 테스트가 실패 후 `.colay`가 존재하지 않음을 검증한다.
- 수정 커밋 `787ffdf`, `64583f7`과 Windows 전체 Rust 테스트 407개 통과로 완료 조건을
  확인했다.

### 사용자 정정: conversation-first plan mode

- 사용자가 의미하는 plan-only는 현재 CLI의 `run --plan-only`와 다르다. orchestrator가
  read-only provider를 선택해 사용자의 질의를 이해하고 답변하며, 후속 질문으로 요구를
  구체화하는 **task 이전의 대화 단계**다.
- 이 단계에서는 task row, coordinator/worker lease, provider attempt, worktree를 만들지
  않는다. 처음부터 모든 입력을 coding task로 영속화하는 현재 `run` 중심 흐름은
  바람직하지 않다.
- orchestrator가 대화 결과를 `answer_complete`, `more_information_needed`,
  `worktree_task_candidate` 같은 명시적인 outcome으로 판단한다.
- 단순 질의는 답변으로 끝나고 session 대화 기록만 남는다. 구현 또는 repository 변경이
  필요하다고 판단한 경우에만 실행 가능한 task graph를 제안하고, 전환 게이트를 통과한
  뒤 task와 격리 worktree를 생성한다.
- 사용자는 전환 게이트를 **인터뷰를 통한 요구 구체화 → 실행 가능성 및 결과 검증 →
  최종 승인** 순서로 확정했다. orchestrator의 필요성 판단만으로 task를 자동 시작하지
  않으며, 최종 승인 전에는 task materialization과 writable 실행이 모두 금지된다.
- 인터뷰 중 요구나 검증 결과가 바뀌면 계획을 새 revision으로 갱신하고 이전 승인 후보를
  무효화해야 한다. 최종 승인은 검증된 최신 revision에만 결합된다.
- Git repository/HEAD 검사는 대화 시작 조건이 아니라 task 승격 조건이다. Git이 없거나
  unborn HEAD라면 대화와 답변은 계속 가능해야 하며, task를 만들지 않은 채 준비 방법을
  안내하고 plan mode에 머문다.
- 이미 materialize된 task의 `resume`은 이 초기 대화 흐름과 별개이며, lease 및 Git
  안전성 검사를 그대로 적용한다.

### 구현 결과

- session-level 사용자 메시지는 task가 아니라 durable
  `request_conversation_turn`으로 이어진다. official CLI adapter는 read-only sandbox에서
  `answer_complete`, `more_information_needed`, `worktree_task_candidate`,
  `needs_attention` 중 하나의 엄격한 JSON outcome만 반환한다.
- 답변과 인터뷰는 conversation attempt와 timeline, 필요 시 immutable requirement revision만
  기록한다. task, task attempt, worktree, coordinator/worker lease는 만들지 않는다.
- 완전한 candidate만 read-only graph planning을 자동 queue한다. Git repository와 valid
  `HEAD`는 이 승격 단계에서 검사하며, 실패하면 session을 유지하고 `Initialize Git and
  create HEAD` 안내를 남긴 채 approvable hash를 만들지 않는다.
- 승인 카드는 requirement revision, validation hash, base commit, validation checks와
  proposal hash를 표시한다. 최신 사용자 메시지 또는 Git `HEAD`가 바뀌면 승인을
  숨기거나 거부한다. 정확한 typed 승인 이후에만 task를 atomic materialize한다.
- provider 실패는 redacted `needs_attention` 응답으로 종료하고 session과 사용자 메시지를
  보존한다. crash replay는 deterministic ID와 idempotency key로 중복 응답·계획을 막는다.
- 기존 `run --plan-only`는 provider를 호출하지 않는 static persisted assessment라는
  compatibility 의미를 유지해 conversation-first 흐름과 구분했다.

### 정정된 완료 조건 검증

- `fixed`: 단순 질의와 인터뷰에서 task/worktree/worker/coordinator lease가 모두 0건임을
  Windows fake-provider 통합 테스트로 검증했다.
- `fixed`: 구현 요청도 session 대화로 시작하고 완전한 candidate와 검증 전에는 task가
  없다.
- `fixed`: Git repository와 valid HEAD를 task graph 승인 후보 승격 직전에 검사하고,
  정확한 최종 승인 이후에만 task를 materialize한다. worktree는 scheduler가 이후 별도로
  생성한다.
- `fixed`: non-Git 및 unborn HEAD에서 session을 유지하고 task 없이 준비 안내를 반환한다.
- `fixed`: `run --plan-only`를 static compatibility command로 문서화하고 자동
  conversation-first TUI session과 분리했다.
- 승인된 접근법의 정식 설계는
  `docs/superpowers/specs/2026-07-22-conversation-first-plan-mode-design.md`에서 추적한다.

## WSL-005: unborn HEAD의 late failure (WSL/Windows 공통)

### 재현된 증상

```text
fatal: Needed a single revision
```

### 증거

- `/home/kimohy/workspace`는 `git init`은 됐지만 `No commits yet on main` 상태였다.
- 상위 workspace에는 `camfit`, `camping`, `quiz` 등 별도 Git repository가 중첩돼 있었다.
- 상위 workspace의 `.colay` DB에는 실패 후 `planned` task 1건이 남았다.
- `git rev-parse --verify HEAD`는 동일한 `Needed a single revision` 오류를 재현했다.
- 하위 `camping` repository는 clean 상태이고 유효한 HEAD가 있었다.
- Windows 11의 `git init`만 수행한 격리 repository에서도 native nightly와 fake provider로
  동일한 `fatal: Needed a single revision`을 재현했다. DB integrity는 정상이었지만
  `planned` task 1건이 남았다.

### 추가 재현: `~/workspace/test`

- 2026-07-22에 사용자가 `~/workspace/test`에서 `colay run hello`를 다시 실행했다.
- 해당 경로는 Git repository이지만 `## No commits yet on main`인 unborn `HEAD` 상태였다.
- state 이외의 project 파일은 없고 `.colay/`만 untracked로 존재했다.
- `git rev-parse --show-toplevel`은 `/home/kimohy/workspace/test`를 반환했지만,
  `git rev-parse --verify HEAD`는 `fatal: Needed a single revision`을 재현했다.
- DB integrity는 정상이고 `planned` task 1건이 남았다.
- active coordinator lease와 provider attempt는 모두 0건이므로 provider 실행 전 worktree
  base revision 확인 단계에서 실패한 것으로 확인됐다.

### 안전 주의

상위 workspace에서 문제를 해결하려고 `git add . && git commit`하면 `.env`,
`node_modules`, 중첩 repository 등을 잘못 포함할 수 있다. 실제 프로젝트 repository로
이동해야 한다.

### 제품 개선 후보

- writable 실행 전에 `git rev-parse --show-toplevel`과 `git rev-parse --verify HEAD^{commit}`을
  분리해서 검사한다.
- unborn HEAD를 `repository has no base commit; create an initial commit first`로 설명한다.
- 중첩 repository를 포함하는 상위 폴더에서 실행할 때 repository 선택 경고를 제공한다.
- 이 검사 역시 task 영속화보다 먼저 수행한다.

### 수정 구현

- Git root probe가 성공한 뒤 `git rev-parse --verify HEAD^{commit}`을 별도 실행한다.
- unborn `HEAD`는 `Git repository has no base commit; create an initial commit before task
  execution`으로 분류한다.
- Windows 호환 CLI 회귀 테스트가 실패 후 `.colay`와 `planned` task가 생성되지 않음을
  검증한다.
- 수정 커밋 `787ffdf`, `64583f7`과 Windows 전체 Rust 테스트 407개 통과로 완료 조건을
  확인했다.

## WSL-008: provider 오류 후 남은 장기 lease

### 재현된 증상

```text
error: lease conflict for task 019f86e9-e70b-7340-a119-20d230d0f8ff: another coordinator lease is active
```

### 증거

- 2026-07-22 08:09 KST의 이전 `resume`은 유효한 초기 commit
  `daf06377f772d3a68aa28b331508d6ee892e2b77`을 기준으로 격리 worktree를 만들었다.
- task는 `planned`에서 `running`으로 전환됐고 Claude writable worker attempt 및 lease가
  생성됐다.
- worker event에는 `Credit balance is too low` 메시지와 `claude_result` 오류가 기록됐지만,
  attempt의 `ended_at`과 `outcome`은 계속 `NULL`이고 task도 `running` 상태로 남았다.
- 조사 시점에는 Colay, Claude, Codex, Gemini 관련 실행 프로세스가 없었으며 worktree에는
  변경 사항도 없었다.
- target coordinator lease는 2026-07-22 08:09:25 KST에 획득됐고
  12:49:25 KST까지, worker lease는 08:40:34 KST까지 유효하게 저장됐다.
- 기본 설정에서는 coordinator TTL이
  `(default_timeout_minutes * (max_retries + 8)) + 600초`로 계산된다. 기본값 30분과
  retry 1회를 적용하면 한 번 획득한 lease가 4시간 40분 유지된다.
- 08:11 KST에 생성된 다른 `hello` task
  `019f86f2-ea08-7bb0-b7b6-0f24822b5eac`도 동일한 Claude credit 오류 후 `running`,
  open attempt, unreleased coordinator/worker lease 상태로 남아 동일 패턴이 반복됐다.
- DB `PRAGMA integrity_check`는 `ok`였으므로 database 손상이 원인은 아니다.

### 근본 원인 방향

- `resume`의 lease 획득 로직은 `released_at IS NULL`인 coordinator를 충돌로 처리하고,
  현재 시각이 저장된 `expires_at`에 도달했을 때만 stale row를 원자적으로 만료시킨다.
- coordinator lease에는 짧은 heartbeat나 owner process liveness 정보가 없고, 정상적인
  함수 반환 경로에서만 release된다. provider 오류 뒤 CLI가 중단되거나 비정상 종료되면
  긴 TTL 전체가 복구 지연 시간이 된다.
- provider가 fatal credit 오류를 event로 보낸 뒤 attempt/task/lease finalization까지
  도달하지 못한 흐름도 함께 조사해야 한다. 현재 증거만으로 CLI가 스스로 비정상
  종료됐는지 사용자가 대기 중 중단했는지는 구분할 수 없지만, 어느 경우든 다음
  `resume`이 수 시간 막히는 복구 설계 문제가 확인됐다.

### 현재의 안전한 복구

- 실행 중인 owner/provider 프로세스가 정말 없는지 먼저 확인한다.
- DB, `orchestrator.db-wal`, `orchestrator.db-shm`, worktree를 삭제하거나 lease row를
  수동 SQL로 변경하지 않는다. append-only audit와 소유권 경계를 훼손할 수 있다.
- 현재 공개된 안전한 takeover 경로는 coordinator 만료 후 같은 task를 다시
  `resume`하는 것이다. target task의 확인된 만료 시각은
  **2026-07-22 12:49:25 KST**다.
- 재개 전에는 Claude의 `Credit balance is too low` 원인을 해소하거나 다른 eligible
  provider로 routing되도록 정상적인 설정/상태를 준비해야 같은 실패가 반복되지 않는다.
- 별도 running task `019f86f2-ea08-7bb0-b7b6-0f24822b5eac`의 coordinator 만료 시각은
  **2026-07-22 12:51:44 KST**이므로 두 task를 혼동하지 않는다.

### 제품 개선 후보

- coordinator를 짧은 TTL과 주기적 renewal로 바꾸고, heartbeat가 끊긴 owner를 짧은
  grace period 뒤 안전하게 takeover할 수 있게 한다.
- Ctrl-C, signal, panic, provider fatal 오류 경로에서 attempt 결과, task 상태,
  worker lease, coordinator lease를 순서대로 finalization하는 공통 cleanup guard를 둔다.
- provider의 terminal 오류 event가 도착하면 stream 종료를 무기한 기다리지 않고 bounded
  wait/cancel 후 attempt를 실패 또는 blocked로 확정한다.
- `resume`의 lease 충돌 메시지에 owner 상태, 획득/갱신/만료 시각, child worker,
  안전한 다음 조치를 표시한다.
- credit/quota 계열의 terminal 오류를 provider health/routing evidence에 반영해 다음
  attempt가 같은 provider로 즉시 반복되지 않게 한다. 확인되지 않은 quota 수치는 계속
  unknown으로 유지한다.
- 명시적인 감사 이벤트와 승인 조건을 가진 `recover stale-lease` 관리 경로를 검토한다.
  owner liveness가 불명확하면 fail-closed하고 worktree와 attempt evidence를 보존한다.

### 수정 구현

- provider `WorkerEvent::Error`는 redacted audit event를 먼저 기록한 뒤 즉시 cancel을
  요청하고 기존 process-tree termination 확인 경로로 진입한다. 확인된 종료는 attempt의
  `ended_at`과 `outcome`을 확정하며, 확인되지 않은 종료만 기존대로 lease를 보존한다.
- direct coordinator lease는 30초, child worker lease는 20초 TTL로 줄이고 활성 owner가
  5초마다 갱신한다. owner가 사라지면 기존 원자적 expiry/takeover가 최대 30초 경계에서
  authority를 회수하며, 살아 있는 owner는 계속 갱신되어 takeover되지 않는다.
- 충돌 오류에 coordinator owner, `renewed_at`, `expires_at`, active worker 수, 안전한 재시도
  시각을 포함한다.
- fake Claude terminal credit 오류, active coordinator/worker renewal, bounded TTL, 충돌
  diagnostics, 기존 atomic expiry/takeover 회귀가 Windows에서 통과했다.
- 수정 커밋 `5f09ecd`와 전체 Rust 테스트 411개, npm 테스트 65개, fmt 및 전체 Clippy
  `-D warnings` 통과로 검증했다. 실제 provider inference는 호출하지 않았다.

### 완료 조건

- fake provider가 terminal credit 오류를 반환하는 회귀 테스트에서 bounded 시간 내
  attempt가 종료되고 task 상태와 두 lease가 일관되게 finalization된다.
- worker 실행 중 parent CLI를 강제 종료하는 테스트에서 짧은 grace period 후 안전하게
  takeover할 수 있으며, 동시에 살아 있는 owner의 lease는 빼앗지 않는다.
- 충돌 출력만으로 사용자가 예상 만료 시각과 안전한 복구 방법을 확인할 수 있다.
- DB integrity, append-only event chain, worktree 격리, exact lease ownership이 유지된다.

## WSL-006: `/mnt/c` checkout과 줄바꿈 불일치

### 증거

- Windows Git에서는 대상 repository가 clean이었다.
- Windows Git의 system `core.autocrlf`는 `true`였다.
- WSL Git에서는 `core.autocrlf`가 unset이었다.
- 동일한 `/mnt/c` checkout을 WSL Git으로 조회하면 거의 모든 파일이 수정된 것으로
  표시됐다.

### 현재 우회

- Linux Colay는 WSL ext4 내부의 clone에서 사용한다.
- Windows checkout은 Windows Colay/Windows Git과 함께 사용한다.
- 한 checkout을 Windows Git과 WSL Git이 번갈아 관리하지 않는다.

### 제품 개선 후보

- WSL에서 repository가 `/mnt/*` 아래에 있으면 성능과 줄바꿈 위험을 진단한다.
- Git status가 전체 파일의 줄바꿈 변경으로 보이는 패턴을 감지해 writable 실행을
  fail-closed하거나 명시적 승인을 요구한다.

## WIN-001: Windows PATH가 오래된 Cargo 설치본을 선택

### 재현된 증상

- Windows PowerShell에서 `Get-Command colay`와 `where.exe colay`는
  `C:\Users\kimoh\.cargo\bin\colay.exe`를 선택했다.
- 이 binary는 `colay 0.1.0`이며, 검증 대상 nightly
  `0.1.1-nightly.20260721.8c7f638`과 다르다.
- Windows 전역 npm에는 `@kimohy/colay`가 설치돼 있지 않았다. 반면 격리된
  `npm exec --package=@kimohy/colay@nightly`와 임시 npm 설치는 같은 nightly를 정상
  선택했다.

### 영향

- 사용자가 nightly를 검증한다고 생각해도 Windows native 명령은 과거 Cargo build를
  실행할 수 있다.
- WSL과 Windows에서 같은 `colay` 명령이 서로 다른 기능, schema 기대치, 오류 처리를
  제공해 재현 결과가 혼재할 수 있다.

### 현재 우회

- 실행 전에 `Get-Command colay -All`, `where.exe colay`, `colay --version`을 함께 확인한다.
- 검증 시에는 버전이 고정된 `npm exec --yes --package=@kimohy/colay@nightly -- colay ...`
  또는 확인된 npm shim/native 경로를 사용한다.
- 오래된 Cargo 설치 제거 또는 PATH 순서 변경은 사용자 환경을 바꾸므로 자동 수행하지
  않는다.

### 제품 개선 후보

- `doctor`가 launcher 경로/버전, native 경로/버전, daemon 경로/버전을 한 화면에 표시한다.
- npm launcher가 자신의 package version과 native binary version 불일치를 거부한다.
- Windows 설치 문서에 Cargo/npm 명령 충돌 확인 절차를 포함한다.

### 수정 구현 및 재검증

- 새 `doctor.runtime` check가 현재 실행 중인 native binary 경로와 build version을 함께
  반환하므로, PATH가 Cargo `0.1.0` 또는 npm nightly 중 무엇을 골랐는지 JSON에서 바로
  확인할 수 있다.
- Windows source build에서 `runtime.status=pass`, `version=0.1.0`, 실제 격리 worktree의
  `target/debug/colay.exe`, `windows/x86_64`가 보고됐고 `.colay` state는 생성되지 않았다.
- PATH 우선순위 변경이나 오래된 Cargo binary 제거는 사용자 환경 변경이므로 자동화하지
  않으며, `Get-Command colay -All`과 `where.exe colay` 확인 절차를 유지한다.

## WIN-002: Windows nightly PE의 Authenticode 부재

### 증거

- npm의 `@kimohy/colay-win32-x64` native binary는 정상 AMD64 PE(`0x8664`)였고
  `--version`도 nightly와 일치했다.
- SHA-256은
  `1BAD6DDC441320165AFBD0B8E214BEF11B02CA806B6A7F97E882C5C8F23EB5BC`였다.
- `npm audit signatures --include-attestations`는 registry 서명과 GitHub Actions 기반
  SLSA provenance를 정상 검증했다.
- 그러나 Windows `Get-AuthenticodeSignature` 결과는 `NotSigned`였다. npm 공급망
  provenance가 유효하다는 사실과 Windows OS 수준의 code-signing 신뢰는 별개다.

### 영향

- 이번 QA 환경에서는 실행 차단이 발생하지 않았으므로 현재 오류의 직접 원인은 아니다.
- SmartScreen 평판, AppLocker/WDAC 또는 기업용 allowlisting 정책에서는 서명되지 않은
  nightly 실행 파일이 경고 또는 차단될 수 있다.

### 제품 개선 후보

- Windows release binary에 신뢰 가능한 Authenticode 서명을 추가하고 서명 검증 절차를
  배포 문서에 포함한다.
- 서명 도입 전에는 npm integrity/provenance와 공개 checksum 검증 방법을 명시한다.
- code signing이 초기 release 범위 밖이라는 기존 결정을 유지한다면 Windows enterprise
  지원 제한으로 명확히 문서화한다.

## WSL-007: chat TUI reconnect 500ms 플래이크

### 증거

- 첫 `cargo test --workspace --all-features`에서
  `chat_tui_help_and_durable_reconnect_keep_daemon_alive`가
  `daemon did not create session within 500ms`로 실패했다.
- 해당 테스트만 3회 재실행했을 때 모두 통과했다.
- 전체 suite 재실행에서는 Rust 테스트 387개가 모두 통과했다.
- Windows 11 전체 suite에서는 같은 테스트를 포함한 Rust 테스트 402개가 한 번에 모두
  통과했다. 따라서 Windows에서 추가 재현되지는 않았으며 기존 WSL 타이밍 플래이크
  분류를 유지한다.

### 제품 개선 후보

- 고정 500ms 제한을 상태 기반 wait와 환경에 맞는 상한으로 교체한다.
- timeout 시 daemon heartbeat, command state, claimed/completed timestamps를 출력한다.
- CI 부하 상태에서 반복 실행하는 플래이크 검증을 추가한다.

### 수정 구현

- session/message projection의 고정 500ms 제한을 10ms 간격, 최대 5초의 상태 기반 bounded
  wait로 교체했다. 성공 시 즉시 반환하므로 정상 경로를 불필요하게 지연하지 않는다.
- 수정 커밋 `fe23303` 이후 해당 Windows 테스트를 단독 3회 연속 통과시켰고 전체
  workspace suite도 통과했다.

## WIN-003: Windows `icacls.exe` 접근 거부 테스트 플래이크

### 증거

- Git readiness 수정 후 첫 전체 suite에서 기존
  `rollback_relative_codex_target_matches_persisted_writable_worker_worktree` 테스트가
  `C:\Windows\System32\icacls.exe: Access is denied (os error 5)`로 한 번 실패했다.
- 동일 테스트만 연속 3회 실행했을 때 모두 통과했다.
- 다음 전체 workspace suite에서도 통과했으므로 현재 Git 변경과의 인과관계는 확인되지
  않았고 간헐적 Windows 권한/프로세스 실행 플래이크로 분류한다.

### 다음 조사 조건

- 동일 오류가 다시 발생하면 executable resolution evidence, 현재 identity, ACL, antivirus
  또는 endpoint policy, 동시 실행 중인 `icacls` 프로세스를 실패 시점에 수집한다.
- 원인이 확인되기 전에는 무조건적인 retry나 권한 완화를 추가하지 않는다.

## Confirmed healthy controls

- 설치된 Linux native binary는 x86-64 static PIE였고 `--version`이 정상 동작했다.
- npm nightly dist-tag와 설치된 build version이 일치했다.
- 격리 repository에서 `init`, migration schema v8, DB integrity, event log integrity가
  정상 동작했다.
- 기존 사용자 DB의 integrity와 append-only event hash chain은 정상으로 확인됐다.
- pseudo-terminal에서 TUI를 열고 `q`로 종료하는 흐름은 정상 동작했다.
- Windows npm root package와 `win32-x64` optional package의 nightly version이 일치했고,
  native binary는 정상 AMD64 PE였다.
- Windows 격리 repository에서 `init`, `doctor`, `providers`, `compatibility`, `status`,
  `run --plan-only`, daemon start/status/stop이 fake provider로 정상 동작했다.
- Windows `doctor`의 6개 check가 모두 pass였고 schema v8, DB integrity, foreign key
  integrity가 정상이었다.
- Windows에서 `cargo fmt --all -- --check`, 전체 clippy `-D warnings`, npm 테스트 65개가
  통과했다. 최초 nightly QA에서는 Rust 402개, Git readiness 수정 후에는 신규 회귀를
  포함한 Rust 407개, SQLite 수정 후 409개, provider lease 수정 후 411개가 통과했다.
- QA 과정에서 실제 Codex, Claude, Gemini inference는 호출하지 않았다.

## Prioritized improvement queue

1. `완료`: 모든 session 입력을 task로 시작하지 않는 conversation-first plan mode를
   기본 TUI 진입점으로 두고, Git preflight는 승인 후보 승격 직전에만 수행한다.
2. `완료`: provider terminal 오류를 finalization하고 장기 고정 lease를 짧은 renewable
   lease로 교체했다 (`WSL-008`).
3. `완료`: idle daemon의 불필요한 immediate transaction을 제거하고 writer starvation을
   회귀 테스트로 고정했다 (`WSL-003`).
4. `P1`: daemon startup timeout 시 child 정리와 phase diagnostics를 보장한다.
5. `P1`: 실패 후 `planned` task를 중복 없이 재개하는 명시적 UX를 제공한다.
6. `P1`: WSL/NVM과 Windows Cargo/npm 충돌을 포함한 실제 실행 binary 경로를
   `doctor`에 노출한다.
7. `P2`: Windows release Authenticode 서명 또는 명시적인 enterprise 지원 제한을
   제공한다.
8. `P2`: `/mnt/c` mixed-Git 환경 경고와 WSL native clone 문서를 추가한다.
9. `완료`: 500ms reconnect 테스트를 condition-based wait로 바꿨다 (`WSL-007`).

## Update log

### 2026-07-22

- 최초 WSL nightly 설치 및 전체 QA 결과를 정리했다.
- daemon startup orphan race와 SQLite writer starvation을 기록했다.
- `not a git repository`와 unborn `HEAD` 오류가 task 영속화 이후에 발생하는 preflight
  결함임을 기록했다.
- `~/workspace/test`의 빈 Git repository에서 unborn `HEAD` 오류가 동일하게 재현됐고,
  `WSL-005`의 두 번째 발생 사례로 추가했다.
- 처음에는 현재 `run --plan-only`로 강등하는 방향을 기록했으나 사용자 피드백에 따라
  폐기했다. 원하는 plan mode는 provider 기반 질의응답과 요구 명확화를 수행하는
  task 이전 session이며, orchestrator가 worktree 작업 필요성을 판단한 뒤에만 Git
  preflight와 task materialization을 수행하는 conversation-first 흐름으로 정정했다.
- task 승격은 orchestrator 단독 판단으로 자동 실행하지 않고, 인터뷰·구체화·검증을 거친
  최신 계획에 사용자가 최종 승인한 경우에만 허용한다는 결정을 추가했다.
- 기존 TUI session/graph/approval 구조를 확장하는 접근법과 상태 모델을 사용자가 승인해
  별도 conversation-first plan mode 설계 문서로 구체화했다.
- `resume`이 Claude의 terminal credit 오류 뒤 open attempt와 장기 coordinator/worker
  lease를 남겨 다음 `resume`이 충돌하는 현상을 `WSL-008`로 기록했다. 동일 패턴이 두
  task에서 반복됐으며 target lease의 안전한 만료 시각도 함께 남겼다.
- Windows checkout과 WSL Git 줄바꿈 설정 차이를 기록했다.
- Windows 11 native QA에서 npm nightly package/native PE/provenance, safe CLI, daemon,
  SQLite 경쟁, Git edge case, 전체 fake-provider test suite를 검증했다.
- `WSL-003`, `WSL-004`, `WSL-005`가 Windows에서도 재현되어 WSL 전용이 아닌 공통
  이슈로 재분류했다.
- Windows PATH가 nightly 대신 Cargo `0.1.0`을 선택하는 `WIN-001`과, npm provenance는
  유효하지만 native PE에 Authenticode가 없는 `WIN-002`를 추가했다.
- Windows daemon start의 일부 PowerShell output-capture 지연은 제품이 아니라 QA harness
  제약으로 판별해 이슈로 등록하지 않았다. 실제 lifecycle과 repository 테스트는
  정상 통과했다.
- Git readiness 수정 작업을 시작했다. typed repository/base-commit 검사를 worktree
  엔진과 direct `run`의 state mutation 이전에 적용했으며, non-Git/unborn 회귀 테스트를
  Windows에서 추가했다.
- Git readiness 수정의 fmt, 전체 clippy, npm 65개, Rust 407개 검증이 통과해
  `WSL-004`와 `WSL-005`를 `fixed`로 전환했다.
- 전체 suite 첫 시도에서 기존 rollback 테스트의 `icacls.exe` 접근 거부가 한 번
  발생했지만 단독 3회와 전체 재실행에서는 통과했다. 이를 `WIN-003`으로 추가했다.
- idle command/scheduler poll에 read-only candidate precheck를 추가했다. Windows에서
  별도 writer가 활성인 회귀 테스트 2개와 fmt, 전체 Clippy, npm 65개, Rust 409개가
  통과해 `WSL-003`을 `fixed`로 전환했다. 전체 재검증에서 `WIN-003`은 재발하지 않았다.
- provider terminal 오류를 즉시 cancel/confirmed-wait 경로로 연결하고 coordinator 30초,
  worker 20초, renewal 5초의 bounded authority로 변경했다. 상세 lease 충돌 진단과 fake
  terminal credit 회귀를 추가하고 Rust 411개 전체 suite를 통과해 `WSL-008`을 `fixed`로
  전환했다.
- 전체 suite에서 `WSL-007`의 500ms reconnect 플래이크가 다시 재현돼 상태 기반 최대 5초
  대기로 교체했다. 단독 3회와 전체 suite 통과 후 `fixed`로 전환했다.
- daemon startup phase와 bootstrap heartbeat를 schema 9에 추가하고, provider별 bounded wait,
  exact process-tree 종료/reap, PID 일치 lease 실패·해제, redacted durable 진단을 적용했다.
  Windows slow fake probe와 전체 Rust 418개, npm 65개, fmt/Clippy 검증이 통과해 `WSL-002`를
  `fixed`로 전환했다. 이 전체 실행에서도 `WIN-003`은 재발하지 않았다.
- conversation-first domain/engine/provider/state/daemon/TUI 경계를 schema 10으로 구현했다.
  Windows에서 자동 답변, 인터뷰, provider 실패 redaction, non-Git 차단, 정확 승인 1회
  materialization, 승인 전 writable table 0건, Git HEAD drift 거부를 fake-provider와 임시
  Git repository로 검증했다. 승인 카드는 requirement/validation/base-commit authority를
  표시하며 새 사용자 메시지는 stale card를 숨긴다.
- WSL 재검증에서 비대화형 shell은 system Node 18을 선택하고 NVM `colay`를 찾지 못하는
  반면, 명시적 Node 22 PATH에서는 설치된 nightly root/native version과 Linux ELF가
  정상임을 확인했다. launcher에 Node 22 fail-fast를 추가하고 Windows/Linux에서 테스트했으며,
  `doctor.runtime`에 현재 native path/build/target 진단을 추가해 `WSL-001`을 `fixed`로
  전환했다. release schema 기대값도 v10으로 갱신해 npm 66개 테스트가 통과했다.
- 향후 대화에서 새 오류가 확인되면 새 ID를 추가하거나 기존 항목의 상태, 증거,
  완료 조건, update log를 갱신한다.
