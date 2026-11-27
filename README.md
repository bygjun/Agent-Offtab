<img src="docs/assets/offtab.png" width="64" height="64" alt="">

# Offtab

Codex와 Claude Code 채팅을 화면 한쪽에 띄워두는 Mac 앱입니다. 어떤 작업이 끝났는지, 어디에 내 답이나 승인이 필요한지 보고 목록에서 원래 채팅을 열 수 있습니다.

**[Mac용 DMG 다운로드][dmg]** · [앱 소개][site] · [릴리스 정보][release] · [문제 제보][issues]

Apple Silicon Mac · macOS 14 이상 권장 · v0.5.25 프리뷰

## 설치

1. [Offtab DMG][dmg]를 열고 **Offtab**을 **응용 프로그램** 폴더로 옮깁니다.
2. Offtab을 열고 Codex 또는 Claude Code에서 작업을 시작합니다. Offtab이 이 Mac에 있는 세션 기록을 읽어 목록을 채웁니다.

기존 Session Desk를 쓰고 있다면 이전 앱을 종료한 뒤 Offtab을 열면 됩니다. 창 배치, 읽음·제외 상태와 연결 설정을 이어서 사용합니다.

## 사용 방법

### 작은 창에서 진행 상황 보기

핀을 켜서 다른 창 위에 띄워두세요. 입력이나 승인이 필요한 채팅은 위로 올라옵니다. 백그라운드에서 끝난 작업도 결과를 읽을 때까지 남고, 읽은 뒤 24시간 동안 새 작업이 없으면 작은 창과 모바일 목록에서 빠집니다.

채팅을 누르면 Codex나 Claude Desktop에서 원래 대화가 열립니다. 지원되는 Terminal·iTerm2 세션은 실행 중인 원래 탭으로 이동합니다. 한동안 숨길 채팅은 우클릭해 **목록에서 제외**를 선택하세요. 직접 새 메시지를 보내면 다시 표시됩니다. [최소화 화면 보기](docs/assets/offtab-compact.webp?v=43172430)

### 전체 창에서 지난 채팅 찾기

전체 창에서 최근 24시간이나 7일의 기록을 볼 수 있습니다. 제목, 작업 폴더, 최신 메시지로 검색하세요. 작은 창과 전체 창은 크기와 위치를 각각 기억합니다.

Claude Desktop에서 별도로 시작한 하위 세션은 두 화면 모두에서 부모 채팅 아래에 표시됩니다. 하위 세션에는 제목과 상태, 최근 메시지가 최대 세 줄까지 표시됩니다. 작업 폴더가 부모와 다르면 폴더명도 표시됩니다. 하위 세션이 두 개 이상이면 목록을 접고 펼칠 수 있습니다. 작업 중이거나 입력·승인을 기다리는 하위 세션이 있으면 기본으로 펼쳐집니다.

![Offtab 전체 창에서 Claude 하위 세션을 펼치고 접은 화면](docs/assets/offtab-full.webp?v=214022d7)

*Claude 하위 세션을 포함한 예시 채팅 10개로 찍은 화면입니다.*

## 문제 해결

### Mac에서 Offtab이 열리지 않아요

v0.5.25 프리뷰는 아직 Apple 공증을 받지 않았습니다. 앱을 여는 과정에서 macOS 경고가 나오면 [Apple의 안내][apple]에서 표시된 메시지를 확인하세요. 설치 파일은 [이 저장소의 Releases][releases]에서 받을 수 있습니다.

### 입력이나 승인 대기가 바로 안 보여요

Offtab의 톱니바퀴 메뉴에서 **실시간 대기 감지**를 서비스별로 연결하세요. Codex는 연결 후 **설정 → 훅(Hooks)** 또는 CLI의 `/hooks`에서 훅 승인이 필요합니다. 연결한 뒤 새 세션을 열거나 기존 세션을 다시 열어 적용하세요. 같은 메뉴에서 macOS 알림도 켤 수 있습니다. [Codex 훅][codex-hooks] · [Claude Code 훅][claude-hooks]

### 터미널 탭이 열리지 않아요

원래 탭으로 처음 이동할 때 macOS가 Terminal 또는 iTerm2 자동화 권한을 요청할 수 있습니다. 이동할 탭은 실행 중이어야 합니다.

## 지원 범위와 데이터

이 Mac의 Codex와 Claude Code 세션을 지원합니다. 일반 Claude 웹 대화는 수집하지 않습니다. Offtab은 대화 기록을 Mac에서 읽습니다. 휴대폰 연결을 켜면 등록된 브라우저와 세션 정보, 답장과 첨부를 중계 서버를 통해 암호화해 주고받습니다.

## 소스에서 실행

Node.js, rustup, Xcode Command Line Tools가 필요합니다. Rust 버전은 `rust-toolchain.toml`을 따릅니다.

```sh
npm ci
npm run check
node --test tests/compact-list.test.mjs
npm start
```

- 웹 데모는 `npm run demo`로 만든 뒤 `release/session-desk-demo.html`을 브라우저에서 엽니다.
- Apple Silicon 배포용 DMG는 `npm run package:mac`으로 만듭니다. 이 명령은 `OFFTAB_SIGNING_TEAM` 또는 `~/Library/Application Support/Offtab Release/signing-team`에 지정한 팀의 Developer ID Application 인증서로 서명합니다. 업데이트에서도 같은 팀과 번들 ID를 사용해야 Mac 권한을 유지할 수 있습니다. DMG는 `src-tauri/target/aarch64-apple-darwin/release/bundle/dmg/`에 생성됩니다.
- 이 Mac에서 사용할 앱은 `npm run package:mac:local`로 빌드합니다. `~/Library/Application Support/Offtab Release/local-signing-cert`에 저장된 인증서 지문으로 키체인의 Offtab 전용 인증서를 찾고, 같은 번들 ID와 인증서로 서명한 앱을 `release/local/`에 저장합니다. 임시 서명 버전에서 처음 전환할 때는 macOS 권한을 한 번 다시 허용해야 합니다.
- 앱은 Tauri 2, Rust, React로 만들었습니다. 서비스 아이콘은 [Lobe Icons][lobe]에서 가져왔습니다. Codex와 Claude Code는 각 서비스의 제품명입니다.

[dmg]: https://github.com/bygjun/Agent-Offtab/releases/download/v0.5.25/Offtab-0.5.25-arm64.dmg
[release]: https://github.com/bygjun/Agent-Offtab/releases/tag/v0.5.25
[releases]: https://github.com/bygjun/Agent-Offtab/releases
[site]: https://bygjun.github.io/Agent-Offtab/
[issues]: https://github.com/bygjun/Agent-Offtab/issues/new
[apple]: https://support.apple.com/ko-kr/102445
[codex-hooks]: https://learn.chatgpt.com/docs/hooks
[claude-hooks]: https://code.claude.com/docs/en/hooks
[lobe]: https://github.com/lobehub/lobe-icons

## 휴대폰 연결 · 직접 중계 설정

공개 설치판은 처음 실행할 때 휴대폰 연결 메뉴가 숨겨져 있습니다. 아래 방법으로 자기 Mac의 중계 주소를 설정하면 **설정 → 휴대폰 연결** 메뉴가 표시됩니다. 기존에 저장한 중계 주소와 등록 기기는 업데이트 후에도 이어서 사용합니다.

### 내 HTTPS 주소에서 사용하기

1. Mac에 Node.js를 설치하고 이 저장소를 내려받습니다. [Cloudflare Tunnel 설정](https://developers.cloudflare.com/tunnel/get-started/)을 참고해 이 Mac에서 실행하는 터널에 자기 도메인의 호스트명을 만들고, 서비스 주소를 `http://127.0.0.1:63915`로 지정합니다.
2. 저장소 폴더에서 아래 명령으로 모바일 페이지와 중계를 실행합니다. `offtab.example.com`은 본인의 호스트명으로 바꾸세요.

   ```sh
   npm ci
   node build/mobile.mjs
   OFFTAB_RELAY_ORIGIN=https://offtab.example.com/ \
     OFFTAB_RELAY_LOCAL_REGISTRATION=1 HOST=127.0.0.1 PORT=63915 node relay/server.mjs
   ```

3. 설치된 Offtab을 종료한 뒤 아래 명령으로 한 번 실행합니다. 주소가 저장되면 다음에는 평소처럼 Dock이나 응용 프로그램에서 열 수 있습니다.

   ```sh
   open -a Offtab --env OFFTAB_RELAY_URL=https://offtab.example.com/ \
     --env OFFTAB_RELAY_LOCAL_URL=http://127.0.0.1:63915/
   ```

4. Mac Offtab의 **설정 → 휴대폰 연결**에서 QR을 만들고, 휴대폰으로 찍은 뒤 Mac에 표시된 6자리 PIN을 입력합니다. 등록한 기기에서는 이 Mac의 모든 프로젝트에서 최근 세션을 볼 수 있습니다. 기기를 제거하려면 Mac 설정에서 **연결 해제**를 누르세요.

Mac과 중계, 터널이 실행 중이어야 휴대폰에서 세션을 갱신하고 답장을 전달할 수 있습니다. 기본 기기 등록 파일은 `~/.offtab-relay/`에 저장되며, 호스트명과 등록 파일을 유지하면 중계나 앱을 다시 켜도 기존 기기가 연결됩니다. 컨테이너로 운영한다면 `OFFTAB_RELAY_STATE`를 재시작 후에도 유지되는 볼륨의 파일 경로로 지정하세요.

### Mac의 브라우저에서 미리 보기

개발 환경에서는 터미널 두 개에서 각각 `npm run mobile`과 `OFFTAB_RELAY_URL=http://127.0.0.1:63915/ npm start`를 실행합니다. **설정 → 휴대폰 연결 → 브라우저에서 미리보기**에서 Mac 브라우저를 열어 6자리 PIN을 입력하세요. 이 주소는 Mac에서만 열립니다.

### 모바일에서 답장하기

답장을 보낼 기기의 **답장 허용**을 Mac에서 켭니다. 기기가 여러 대라면 **모든 기기 답장 허용**으로 현재 연결된 기기의 권한을 한 번에 바꿀 수 있습니다. 새 기기는 조회 전용으로 시작합니다. 모바일 웹에서 세션을 터치하면 최근 답변을 읽고, 지원되는 세션에서 답장을 보낼 수 있습니다. 답장을 보내고 목록으로 돌아가도 전송을 계속 확인할 수 있습니다.

사진과 UTF-8 텍스트를 합쳐 최대 4개, 총 20MB까지 첨부할 수 있습니다. 첨부 한도는 사진 한 장당 5MB, 텍스트 파일 하나당 16KB입니다. 모바일은 텍스트 초안과 처리 중인 답장 ID를 등록한 브라우저에 암호화해 보관합니다. 대화 화면을 위로 스크롤하면 최근 메시지 최대 10개를 볼 수 있습니다.

iPhone 홈 화면에서 Offtab을 열 때 연결을 다시 요청하면 Mac에서 새 연결 주소를 만들어 붙여넣으세요.

Codex 답장과 사진은 기존 채팅의 대기열에 들어갑니다. 열린 Claude Desktop 채팅에 답장과 첨부를 보내려면 Mac Offtab의 **휴대폰 연결 → 손쉬운 사용 설정**에서 Offtab을 허용하세요. Mac이 잠겨 있거나 Claude 입력창에 작성 중인 메시지가 있으면 전송을 멈춥니다. 종료된 Claude 채팅은 사용 가능한 Claude Code CLI로 이어갑니다.

대기열 접수나 전송 완료 안내는 잠깐 표시됩니다. 실패한 답장은 해당 채팅에서 안내와 작성한 내용을 다시 확인할 수 있습니다.

### Mac 로그인 시 중계 켜기

로컬에서 관리하는 Cloudflare 터널 설정 파일과 `cloudflared` 실행 파일이 있다면, 모바일 페이지를 빌드한 뒤 아래 명령으로 사용자 LaunchAgent 파일을 만들 수 있습니다.

```sh
node relay/macos-services.mjs https://offtab.example.com/ /path/cloudflared.yml /path/cloudflared /output/directory
```

생성한 두 plist를 사용자 `~/Library/LaunchAgents`에 두면 다음 로그인부터 실행됩니다. 중계를 LaunchAgent로 전환할 때는 이전과 같은 `OFFTAB_RELAY_STATE` 경로를 사용하세요. 생성된 중계 plist에 해당 경로를 지정하면 같은 기기 등록을 이어서 쓸 수 있습니다.
