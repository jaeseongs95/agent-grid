# Agent Grid

Independent MIT implementation. Never copy Claudes source, assets, or UI text.
Windows-first Tauri 2, Rust portable-pty, TypeScript/Vite/xterm.js.
Keep vendor protocols in the Codex adapter; do not hardcode model names.
Preserve user authentication, config and external sessions. Do not install or stop existing services.
Work branch: feat/initial-desktop. Root agent owns Git and releases; subagents never commit or push.
New text: UTF-8 without BOM, LF. Build artifacts must stay ignored.
Commands: npm ci; npm run build; npm test; cargo test --manifest-path src-tauri/Cargo.toml.
If Korean paths break native builds, use D:/codex/build/agent-grid with a separate checkout and target.
