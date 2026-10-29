# OpenDIY Desktop

Long-term product goal: replace the ChatGPT desktop app with OpenDIY Desktop. Eventually benchmark both ChatGPT and Claude desktop apps and integrate the relevant features and usability into OpenDIY Desktop; this is future product direction, not authorization to start that broader work now.
메모리 최적화는 조회 팝업과 실제 세션 모두에 적용한다. 화면은 최근 구간과 실시간 출력만 유지하고 과거 구간은 필요할 때 조회한다. 모델 문맥을 임의로 절단하지 않으며, 대기열·캐시·자원 수명 상한과 실제 해제 근거를 확인한다.
The user-selected Codex runtime source is https://github.com/jaeseongs95/opendiy-codex; verify its actual CLI/app-server capabilities and support its executable through the existing path setting.
For plugin/skill management, the current Codex desktop app is the functional reference; registration, installation and activation must actually integrate with the Codex CLI.

Independent MIT implementation. Never copy Claudes source, assets, or UI text.
Windows-first Tauri 2, Rust portable-pty, TypeScript/Vite/xterm.js.
Keep vendor protocols in the Codex adapter; do not hardcode model names.
Preserve user authentication, config and external sessions. Do not install or stop existing services.
Work branch: feat/initial-desktop. Root agent owns Git and releases; subagents never commit or push.
New text: UTF-8 without BOM, LF. Build artifacts must stay ignored.
Commands: npm ci; npm run build; npm test; cargo test --manifest-path src-tauri/Cargo.toml.
If Korean paths break native builds, use D:/codex/build/agent-grid with a separate checkout and target.
