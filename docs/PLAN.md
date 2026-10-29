# agentlens 구현 계획

## 목표

여러 코딩 에이전트 프로그램(Orca, Codex, Claude Code, opencode, zcode …)이 각자 띄우는
node/MCP/REPL 프로세스와 SSH 연결을 **에이전트 탭 단위로 묶어** 보여 주고, 안 쓰는 것을 직접 정리한다.

## 결정 사항

| 항목 | 결정 | 이유 |
|---|---|---|
| 언어 | Rust | 요청 |
| UI | TUI (ratatui + crossterm) | GUI(egui) 대비 메모리·CPU 부담이 작고 Orca 탭 안에서 바로 실행 가능 |
| 에이전트 감지 | Orca 카탈로그 45개 + PATH 설치 스캔 + 사용자 규칙 | 실행 파일 이름만으로는 `node.exe`로 뜨는 에이전트를 구분할 수 없음 |
| 탭 묶음 근거 | `ORCA_TAB_ID`, `ORCA_WORKTREE_ID` 환경변수, 그 외 호스트 직계 자식 | 실측으로 확인 |
| 메모리 기준 | Private Bytes | Working Set은 공유 메모리 중복 |
| 수집 API | `NtQuerySystemInformation` + PID별 `NtQueryInformationProcess`, cwd/env만 sysinfo(5초 배치) | sysinfo는 매초 프로세스마다 핸들을 열어 틱당 ~100ms. 바꾼 뒤 ~5ms, 대기 CPU -65%, 메모리 -33% |
| macOS / Linux | macOS `sysctl(KERN_PROC_ALL)` + `proc_pid_rusage`(한 번에 CPU·메모리), Linux procfs | 각 OS의 커널 인터페이스. macOS는 libproc만으로는 root 프로세스(launchd, login)가 빠져 트리가 끊김 |
| Unix 고아 | launchd/init/`systemd --user`에 입양된 경우 | Unix에는 "부모 없음"이 거의 없음 |
| 지식의 위치 | `crates/core/catalog.toml`(내장) + 같은 형식의 사용자 `rules.toml` | 제품·OS별 이름과 환경변수를 로직에 박지 않음. Orca 전용 처리도 호스트의 `tab_env`/`label_env`/`launch_env` 데이터로 일반화 |
| Mach 타임베이스 | `mach2` 크레이트 | libc의 Mach 바인딩은 deprecated(이관됨). 경고 숨김 대신 권장 경로 사용 |
| GLM(Z.ai) | 에이전트가 아니라 모델 제공자 속성으로 표시 | 사용자 확인 |
| Working set trim | 제외 | 숫자만 줄고 실제 확보 효과가 거의 없음 |
| 오픈소스 | 직접 구현 | Windows + 탭 묶음 + SSH를 함께 지원하는 도구 없음. zclean(근거 기반 판정), TerminalStateGuard(종료 영향 분석), agent-ps(cwd 매칭)를 참고 |

## 구조

```
crates/core   snapshot(수집) · catalog/install/rules(지식) · model(분류·묶음)
              health(유휴·후보) · net(SSH) · actions(재검증 후 종료)
crates/app    tui(대시보드) · report(--report 텍스트)
```

## 단계와 상태

| 단계 | 내용 | 완료 기준 | 상태 |
|---|---|---|---|
| M0 | 수집, `--dump-json` | 명령줄·cwd·env·시작시각·Private·TCP 포함, 1초 주기 가능 | 완료 — 갱신 60~150ms |
| M1 | 카탈로그 45개, 설치 스캔, 호스트·탭·세션 판정 | 실제 스냅샷에서 Orca 탭 11개, ego, 고아가 맞게 묶임 | 완료 — `tests/fixture.rs` |
| M2 | TUI 세션 뷰, 상세 패널 | 접기·정렬·선택이 행과 일치, 실제 콘솔 렌더링 | 완료 — TestBackend 테스트 + Orca 터미널에서 확인 |
| M3 | 정리 후보, 트리 종료, 보호 | 자식부터 종료, PID 재사용 시 건너뜀, 자기/호스트/시스템 거부 | 완료 — `tests/live.rs` |
| M4 | SSH 뷰, 끊기 | 테스트 ssh 연결이 보이고 끊으면 사라짐 | 완료 — `tests/live.rs` |
| M5 | 규칙 파일, 호스트 등록, 알림 | 규칙 편집·재로드, 미등록 호스트 등록, 새 후보 상태줄 알림 | 완료 |

## 크로스 플랫폼 검증

| OS | 방법 | 결과 |
|---|---|---|
| Windows 11 | 로컬 | clippy 0, 테스트 39개 통과 |
| Linux (Debian bookworm) | Docker 컨테이너 | clippy 0, 테스트 39개 ×3 통과 (ssh 감지·끊기 포함) |
| macOS 26.5 arm64 | SSH로 실제 Mac | clippy 0, 테스트 39개 ×3 통과, 실사용 리포트에서 Orca 탭·claude 세션·입양 고아·인바운드 SSH 확인 |

## 남은 일 / 아이디어

- TCP 연결만 끊기(`SetTcpEntry`, 관리자 권한)
- 트레이 아이콘·시스템 알림 (현재는 상태줄 알림)
- Codex Desktop 등 실행 파일 위치를 아직 확인하지 못한 호스트의 카탈로그 항목 (지금은 "unregistered"로 자동 묶임)
