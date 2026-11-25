# Offtab

Codex와 Claude Code 작업을 옆에 띄워 두는 Mac 앱입니다. 다른 창에서 작업하면서 어느 세션이 돌아가는지, 어디에 내 답변이 필요한지, 어떤 결과가 도착했는지 확인할 수 있습니다.

[Offtab 소개](https://bygjun.github.io/Agent-Offtab/) · [Mac 프리뷰 다운로드](https://github.com/bygjun/Agent-Offtab/releases/tag/v0.5.0) · [문제 제보](https://github.com/bygjun/Agent-Offtab/issues/new)

## 설치

Apple Silicon Mac용입니다. macOS 14 이상을 권장합니다.

1. [Releases](https://github.com/bygjun/Agent-Offtab/releases/tag/v0.5.0)에서 `Offtab-0.5.0-arm64.dmg`를 받습니다.
2. DMG를 열고 **Offtab**을 **응용 프로그램** 폴더로 드래그합니다.
3. Offtab을 열고 Codex 또는 Claude Code에서 작업을 시작해 보세요. 이 Mac에 있는 로컬 세션 기록을 읽습니다.

이 프리뷰는 자체 서명된 테스트 빌드입니다. Apple 공증 전이라 다운로드한 앱의 실행을 macOS가 제한할 수 있습니다. [Apple의 Mac 앱 배포 안내](https://developer.apple.com/macos/distribution/)

기존 Session Desk 사용자는 이전 앱을 종료하고 Offtab을 엽니다. 창 배치, 읽음·제외 상태와 연결 설정을 이어서 사용합니다.

## 사용 방법

- **작은 창을 옆에 두기** — 창 크기를 조절하고 핀으로 항상 위 고정을 바꿀 수 있습니다. 전체 창과 작은 창은 크기와 위치를 각각 기억합니다.
- **원본 채팅 열기** — 세션을 클릭하면 해당 Codex·Claude Desktop 채팅을 엽니다. 지원되는 Terminal·iTerm2 세션은 실행 중인 원래 탭으로 이동합니다.
- **대기와 완료 확인** — 입력·승인 대기는 목록 위에 표시됩니다. 백그라운드 작업이 끝나면 읽지 않은 완료로 남고, 읽은 뒤 한 시간 동안 새 작업이 없으면 작은 창에서 빠집니다.
- **목록에서 제외** — 세션을 우클릭해 선택하세요. 원본 채팅에 직접 새 메시지를 보내면 목록에 다시 표시됩니다.
- **지난 세션 찾아보기** — 전체 창에서 최근 24시간 또는 7일을 보고, 제목·작업 폴더·미리보기로 검색할 수 있습니다.

### 선택 사항

톱니바퀴에서 Codex·Claude Code 대기 감지를 각각 연결하고, 필요하면 macOS 알림을 켤 수 있습니다. Codex의 훅은 연결 후 **설정 → 훅(Hooks)** 또는 CLI의 `/hooks`에서 승인합니다. 연결한 뒤 새 세션을 열거나 기존 세션을 다시 열어 적용할 수 있습니다. [Codex 훅](https://learn.chatgpt.com/docs/hooks) · [Claude Code 훅](https://code.claude.com/docs/en/hooks)

### 데이터와 지원 범위

대화 기록은 로컬에서 읽고 외부 서버로 보내지 않습니다. 지원 범위는 로컬 Codex와 Claude Code 세션입니다. 일반 Claude 웹 대화는 수집하지 않습니다. 원본 터미널 탭으로 처음 이동할 때는 macOS가 터미널 자동화 권한을 요청할 수 있습니다.

## 소스에서 실행

Tauri 2, Rust, React 기반입니다. Node.js, rustup, Xcode Command Line Tools가 필요합니다. Rust 버전은 `rust-toolchain.toml`을 따릅니다.

```sh
npm ci
npm run check
node --test tests/compact-list.test.mjs
npm start
```

- 예시 세션으로 웹 데모를 열려면 `npm run demo` 실행 후 `release/session-desk-demo.html`을 브라우저에서 엽니다.
- Apple Silicon DMG를 만들려면 `npm run package:mac`을 실행합니다. 결과물은 `src-tauri/target/aarch64-apple-darwin/release/bundle/dmg/`에 생성됩니다.
- 서비스 아이콘 출처는 [Lobe Icons](https://github.com/lobehub/lobe-icons)입니다. Codex와 Claude Code는 각 서비스의 제품명입니다.
