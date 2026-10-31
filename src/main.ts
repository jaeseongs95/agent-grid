import { invoke, isTauri } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { SearchAddon } from '@xterm/addon-search';
import { version } from '../package.json';
import '@xterm/xterm/css/xterm.css';
import './style.css';
import { MAX_SAVED_WORKSPACE_SESSIONS, MAX_WORKSPACE_SESSIONS, acceptOutput, chooseSavedSessions, parseLayout, projectGroups, sessionProjectKey, savedSessionMetadata, updateSession, type Layout, type Output, type SavedSession, type Session, type SessionInfo, type ProjectInfo } from './model';
import { createSessionBrowser } from './session-browser';
import { createProjectManager } from './project-manager';
import { createExecutableSettings } from './executable-settings';
import { createExtensionsManager } from './extensions';
import { createMobileControl } from './mobile-control';
import { createWeeklyUsage } from './weekly-usage';
import './weekly-usage.css';
import { createConversation } from './conversation';
import { createContextControls } from './context-controls';
import { createCronManager } from './cron-manager';
import { createSubagentManager } from './subagent-manager';
import { createQuestionCenter } from './question-center';
import { InputQueueError, TerminalInputQueue, TerminalResizeQueue, TerminalOutputAcknowledgements } from './terminal-flow';
import { bindAttribute, bindText, getLanguage, languageStorageError, onLanguageChange, setLanguage, t } from './i18n';

const native = isTauri();
const demo = !native && new URLSearchParams(location.search).has('demo');
const $ = <T extends HTMLElement = HTMLElement>(selector: string) => document.querySelector<T>(selector)!;
const states: Record<string, [string, string]> = { restored: ['저장됨 · 미실행', 'Saved · not started'], notLoaded: ['상태 미확인', 'Unknown state'], starting: ['시작 중', 'Starting'], running: ['실행 중', 'Running'], idle: ['입력 가능', 'Ready'], waiting: ['응답 대기', 'Waiting for response'], stopping: ['종료 중', 'Stopping'], stopped: ['종료됨', 'Stopped'], exited: ['종료됨', 'Exited'], error: ['오류', 'Error'], unknown: ['상태 미확인', 'Unknown state'] };
function stateLabels(state: string): [string, string] { return states[state] ?? states.unknown; }
const sessions = new Map<string, Session>();
interface QueuedInput { data: string; generation: number; selectionVersion: number }
interface QueuedResize { generation: number; selectionVersion: number; cols: number; rows: number }
const terminals = new Map<string, { terminal: Terminal; fit: FitAddon; search: SearchAddon; card: HTMLElement; observer: ResizeObserver; input: TerminalInputQueue<QueuedInput>; resize: TerminalResizeQueue<QueuedResize>; inputWarning: boolean; resizeTimer?: ReturnType<typeof setTimeout> }>();
const conversations = new Map<string, ReturnType<typeof createConversation>>();
const conversationOwners = new Set<string>();
const contextControls = new Map<string, ReturnType<typeof createContextControls>>();
const pending = new Map<string, Output[]>();
const resolving = new Set<string>();
const unlisteners: UnlistenFn[] = [];
let order: string[] = [];
let active = '';
let maximized = '';
let columns = 2;
let columnSizes = [1, 1];
let rowSizes = [1, 1];
let saved: SavedSession[] = [];
// Unresolved slots survive partial restoration; live slots keep the saved order.
let workspaceSlots: (SavedSession | string)[] = [];
let workspaceReady = !native;
let restoringWorkspace = native;
let exitInProgress = false;
let initialization: Promise<void> = Promise.resolve();
let runtimePreparation: Promise<void> | undefined;
let runtimeRestoration: Promise<void> | undefined;
let runtimeReady = !native;
let runtimeGeneration = 0;
let runtimeCommand: Promise<void> | undefined;
let runtimeCommandGeneration = 0;
let executableMutation: Promise<unknown> | undefined;
const restoringRequests = new Map<SavedSession, Promise<SessionInfo>>();
const startOperations = new Set<Promise<SessionInfo>>();
// Failed shutdown events must not erase the restart intent saved before stopping.
const exitResumeIntents = new Set<string>();
const failedRestoreCards = new Set<string>();
const explicitlyStoppedSessions = new Set<string>();
const cancelledRestores = new Set<SavedSession>();
let saveTimer: ReturnType<typeof setTimeout>;
let saveQueue = Promise.resolve();
let dragging = '';
let searchVisible = false;
let pendingStarts = 0;
const pendingThreads = new Map<string, Promise<SessionInfo>>();
let importedProjects: ProjectInfo[] = [];
let selectedProject = '';
let projectSelectionVersion = 0;
let nativeWindowVisible = true;

if (native) $('#app').hidden = true;
$('#app').setAttribute('aria-busy', 'true');
$('#app').innerHTML = `
  <aside class="sidebar">
    <a class="brand" href="#" aria-label="OpenDIY Desktop 홈"><span class="brand-mark"><i></i><i></i><i></i><i></i></span><span class="brand-name">OpenDIY <small>Desktop</small></span><span class="version">${version}</span></a>
    <div class="sidebar-lists">
      <section class="sidebar-projects" aria-labelledby="projects-heading">
        <div id="projects-heading" class="side-heading">프로젝트 <span id="project-count">0</span></div>
        <div class="project-actions"><button id="project-create" class="project-import quiet-button">+ 새 프로젝트</button><button id="project-import" class="project-import quiet-button">+ 프로젝트 가져오기</button></div>
        <div id="projects" class="projects"></div>
      </section>
      <section class="sidebar-sessions" aria-labelledby="sessions-heading">
        <div id="sessions-heading" class="side-heading sessions-heading">세션 <span id="side-count">0</span><button id="side-add" class="icon-button" aria-label="새 세션 만들기">+</button></div>
        <nav id="session-list" aria-label="세션 목록"></nav>
      </section>
    </div>

  </aside>
  <main>
    <header class="topbar"><div class="workspace-actions"><button id="catalog-open" class="quiet-button">세션 불러오기</button><button id="new-session" class="primary-button"><span>+</span> 새 세션</button></div><div class="top-actions"><span id="mode-label" class="mode-label">LOCAL WORKSPACE</span><button id="notifications" class="icon-button" aria-label="질문함">?<span id="notification-count" hidden></span></button><button id="settings-open" class="icon-button" aria-label="설정 열기" aria-haspopup="dialog" aria-controls="settings-dialog" title="설정">⚙</button><button id="exit" class="quiet-button">앱 종료</button></div></header>
    <div id="project-tabs" class="project-tabs" role="tablist" aria-label="프로젝트별 세션"></div>
    <section class="toolbar" aria-label="그리드 도구"><div class="session-summary"><span class="connection-dot"></span><strong id="running-count">0</strong> 실행 중 <span class="divider"></span><span id="total-count">0개 세션</span></div><div class="layout-tools"><button id="search-toggle" class="quiet-button" title="활성 터미널 검색 (Ctrl+Shift+F)">⌕ <span>검색</span></button><span class="divider"></span><span class="layout-label">열</span><div id="column-options" class="segmented" role="group" aria-label="그리드 열 수"><button data-columns="1" aria-label="1열">▯</button><button data-columns="2" class="active" aria-label="2열">▥</button><button data-columns="3" aria-label="3열">▥▯</button><button data-columns="4" aria-label="4열">▥▥</button></div></div></section>
    <div id="runtime-banner" class="runtime-banner" role="status" hidden><span id="runtime-status"></span> <button id="runtime-retry" class="quiet-button" hidden>다시 시도</button></div>
    <div id="restore-banner" class="restore-banner" hidden><span>자동으로 연결하지 못한 저장 세션이 있습니다. 목록에서 다시 연결하거나 제거할 수 있습니다.</span><button id="restore-open" class="quiet-button">저장 세션 보기 →</button></div>
    <section id="search-bar" class="search-bar" hidden><label for="terminal-search">활성 터미널 검색</label><input id="terminal-search" placeholder="검색어 입력"/><button id="search-prev" class="quiet-button" aria-label="이전 검색 결과">↑</button><button id="search-next" class="quiet-button" aria-label="다음 검색 결과">↓</button><span id="search-result" role="status"></span><button id="search-close" class="icon-button" aria-label="검색 닫기">×</button></section>
    <section id="grid-area" class="grid-area" role="tabpanel"><div id="grid" class="terminal-grid"></div><div id="grid-handles"></div><div id="empty" class="empty"><div class="empty-grid">▦</div><h2>다음 작업을 펼쳐 보세요</h2><p>프로젝트 경로를 선택해 첫 세션을 시작하세요.<br>각 세션은 독립된 터미널에서 실행됩니다.</p><button id="empty-add" class="primary-button">+ 첫 세션 만들기</button><small>Alt + 1–9 세션 전환 · Ctrl + Tab 다음 세션</small></div></section>
    <footer><span class="footer-hints">드래그로 순서 변경 <i>·</i> 경계선으로 크기 조절 <i>·</i> Alt + 1–9</span><span id="save-status">레이아웃 자동 저장</span></footer>
  </main>
  <div id="toast-area" class="toast-area" role="status" aria-live="polite"></div>
  <dialog id="settings-dialog" aria-labelledby="settings-title"><div class="dialog-heading"><h2 id="settings-title">설정</h2><button id="settings-close" class="icon-button" data-close aria-label="설정 닫기" autofocus>×</button></div><div class="settings-options"><button id="codex-settings-open" class="side-link" aria-haspopup="dialog" aria-controls="codex-settings-dialog"><span>⚙</span> 실행 파일 설정</button><button id="extensions-open" class="side-link" aria-haspopup="dialog" aria-controls="extensions-dialog"><span>◇</span> <span id="extensions-open-label">플러그인·스킬</span></button><button id="external-toggle" class="side-link" aria-haspopup="dialog" aria-controls="external-dialog"><span>◉</span> 외부 세션 <span class="readonly">읽기 전용</span></button></div><div class="language-setting"><label for="language-select">언어</label><select id="language-select"><option value="ko">한국어</option><option value="en">English</option></select><small id="language-error" role="alert" hidden></small></div></dialog>
  <dialog id="create-dialog"><form id="create-form"><div class="dialog-heading"><span class="eyebrow">NEW SESSION</span><button type="button" class="icon-button" data-close aria-label="닫기">×</button></div><h2>새 세션 만들기</h2><p>선택한 프로젝트에서 로컬 에이전트를 시작합니다.</p><label for="session-title">세션 이름</label><input id="session-title" name="title" placeholder="예: API 개선" required maxlength="120"/><label for="session-cwd">프로젝트 경로</label><input id="session-cwd" name="cwd" placeholder="C:\\work\\my-project" required/><small>실제로 존재하는 로컬 디렉터리의 절대 경로를 입력하세요.</small><label for="session-thread">이어갈 스레드 ID <span class="optional">선택</span></label><input id="session-thread" name="thread" placeholder="비워 두면 새 대화로 시작합니다"/><div id="create-error" class="inline-error" role="alert" hidden></div><div class="dialog-actions"><button type="button" class="quiet-button" data-close>취소</button><button id="create-submit" class="primary-button" type="submit">세션 시작 →</button></div></form></dialog>
  <dialog id="restore-dialog"><div class="dialog-heading"><span class="eyebrow">SAVED WORKSPACE</span><button class="icon-button" data-close aria-label="닫기">×</button></div><h2>저장된 세션</h2><p>이전에 실행 중이던 대화는 자동으로 연결합니다. 남은 항목은 선택해서 다시 연결할 수 있습니다.</p><div id="restore-list" class="restore-list"></div></dialog>
  <dialog id="external-dialog"><div class="dialog-heading"><span class="eyebrow">READ ONLY</span><button class="icon-button" data-close aria-label="닫기">×</button></div><h2>외부 세션</h2><p>다른 앱에서 발견한 기록입니다. 관측 정보이며 현재 실행 상태는 미확인입니다. 입력하거나 종료할 수 없습니다.</p><button id="external-refresh" class="quiet-button">새로 고침</button><div id="external-list" class="external-list"></div></dialog>
  <dialog id="exit-dialog"><div class="dialog-heading"><span class="eyebrow">EXIT WORKSPACE</span><button class="icon-button" data-close aria-label="닫기">×</button></div><h2>OpenDIY Desktop를 종료할까요?</h2><p>앱이 소유한 모든 실행 세션이 종료됩니다. 다른 앱의 세션은 제어하지 않습니다. 창의 닫기 버튼은 트레이로 숨깁니다.</p><p id="unconfirmed-exit-warning" class="inline-error" role="alert" hidden>원격 작업의 중단을 확인하지 못했습니다. 앱을 닫아도 원격 작업이 계속 실행될 수 있습니다. 로컬 자원은 정리되었습니다.</p><div class="dialog-actions"><button class="quiet-button" data-close>계속 작업</button><button id="confirm-unconfirmed-exit" class="danger-button" aria-describedby="unconfirmed-exit-warning" hidden>중단 미확인 상태로 앱 닫기</button><button id="confirm-exit" class="danger-button">세션 종료 후 앱 닫기</button></div></dialog>
`;

// Register only app-owned templates, before any terminal or user content is inserted.
const markupLabels: Record<string, [string, string]> = {
  "OpenDIY Desktop 홈": ["OpenDIY Desktop 홈", "OpenDIY Desktop home"],
  "프로젝트": ["프로젝트", "Projects"],
  "+ 새 프로젝트": ["+ 새 프로젝트", "+ New project"],
  "+ 프로젝트 가져오기": ["+ 프로젝트 가져오기", "+ Import project"],
  "세션": ["세션", "Sessions"],
  "새 세션 만들기": ["새 세션 만들기", "Create a session"],
  "세션 목록": ["세션 목록", "Session list"],
  "외부 세션": ["외부 세션", "External sessions"],
  "읽기 전용": ["읽기 전용", "Read only"],
  "실행 파일 설정": ["실행 파일 설정", "Executable settings"],
  "설정": ["설정", "Settings"],
  "설정 열기": ["설정 열기", "Open settings"],
  "설정 닫기": ["설정 닫기", "Close settings"],
  "언어": ["언어", "Language"],
  "LOCAL WORKSPACE": ["로컬 작업 공간", "LOCAL WORKSPACE"],
  "알림 보기": ["알림 보기", "View notifications"],
  "앱 종료": ["앱 종료", "Exit app"],
  "세션 불러오기": ["세션 불러오기", "Load sessions"],
  "새 세션": ["새 세션", "New session"],
  "프로젝트별 세션": ["프로젝트별 세션", "Sessions by project"],
  "그리드 도구": ["그리드 도구", "Grid tools"],
  "실행 중": ["실행 중", "running"],
  "0개 세션": ["0개 세션", "0 sessions"],
  "활성 터미널 검색 (Ctrl+Shift+F)": ["활성 터미널 검색 (Ctrl+Shift+F)", "Search active terminal (Ctrl+Shift+F)"],
  "검색": ["검색", "Search"],
  "열": ["열", "Columns"],
  "그리드 열 수": ["그리드 열 수", "Grid column count"],
  "1열": ["1열", "1 column"],
  "2열": ["2열", "2 columns"],
  "3열": ["3열", "3 columns"],
  "4열": ["4열", "4 columns"],
  "자동으로 연결하지 못한 저장 세션이 있습니다. 목록에서 다시 연결하거나 제거할 수 있습니다.": ["자동으로 연결하지 못한 저장 세션이 있습니다. 목록에서 다시 연결하거나 제거할 수 있습니다.", "Some saved sessions could not reconnect automatically. Retry or remove them from the list."],
  "저장 세션 보기 →": ["저장 세션 보기 →", "View saved sessions →"],
  "활성 터미널 검색": ["활성 터미널 검색", "Search active terminal"],
  "검색어 입력": ["검색어 입력", "Enter search text"],
  "이전 검색 결과": ["이전 검색 결과", "Previous match"],
  "다음 검색 결과": ["다음 검색 결과", "Next match"],
  "검색 닫기": ["검색 닫기", "Close search"],
  "다음 작업을 펼쳐 보세요": ["다음 작업을 펼쳐 보세요", "Open your next task"],
  "프로젝트 경로를 선택해 첫 세션을 시작하세요.": ["프로젝트 경로를 선택해 첫 세션을 시작하세요.", "Choose a project path to start your first session."],
  "각 세션은 독립된 터미널에서 실행됩니다.": ["각 세션은 독립된 터미널에서 실행됩니다.", "Each session runs in its own terminal."],
  "+ 첫 세션 만들기": ["+ 첫 세션 만들기", "+ Create first session"],
  "Alt + 1–9 세션 전환 · Ctrl + Tab 다음 세션": ["Alt + 1–9 세션 전환 · Ctrl + Tab 다음 세션", "Alt + 1–9 switch sessions · Ctrl + Tab next session"],
  "드래그로 순서 변경": ["드래그로 순서 변경", "Drag to reorder"],
  "경계선으로 크기 조절": ["경계선으로 크기 조절", "Drag borders to resize"],
  "레이아웃 자동 저장": ["레이아웃 자동 저장", "Layout autosave"],
  "NEW SESSION": ["새 세션", "NEW SESSION"],
  "닫기": ["닫기", "Close"],
  "선택한 프로젝트에서 로컬 에이전트를 시작합니다.": ["선택한 프로젝트에서 로컬 에이전트를 시작합니다.", "Start a local agent in the selected project."],
  "세션 이름": ["세션 이름", "Session name"],
  "예: API 개선": ["예: API 개선", "e.g. API improvements"],
  "프로젝트 경로": ["프로젝트 경로", "Project path"],
  "실제로 존재하는 로컬 디렉터리의 절대 경로를 입력하세요.": ["실제로 존재하는 로컬 디렉터리의 절대 경로를 입력하세요.", "Enter the absolute path of an existing local directory."],
  "이어갈 스레드 ID": ["이어갈 스레드 ID", "Thread ID to resume"],
  "선택": ["선택", "optional"],
  "비워 두면 새 대화로 시작합니다": ["비워 두면 새 대화로 시작합니다", "Leave blank to start a new conversation"],
  "취소": ["취소", "Cancel"],
  "세션 시작 →": ["세션 시작 →", "Start session →"],
  "SAVED WORKSPACE": ["저장된 작업 공간", "SAVED WORKSPACE"],
  "저장된 세션": ["저장된 세션", "Saved sessions"],
  "이전에 실행 중이던 대화는 자동으로 연결합니다. 남은 항목은 선택해서 다시 연결할 수 있습니다.": ["이전에 실행 중이던 대화는 자동으로 연결합니다. 남은 항목은 선택해서 다시 연결할 수 있습니다.", "Previously running conversations reconnect automatically. Choose remaining sessions to retry."],
  "READ ONLY": ["읽기 전용", "READ ONLY"],
  "다른 앱에서 발견한 기록입니다. 관측 정보이며 현재 실행 상태는 미확인입니다. 입력하거나 종료할 수 없습니다.": ["다른 앱에서 발견한 기록입니다. 관측 정보이며 현재 실행 상태는 미확인입니다. 입력하거나 종료할 수 없습니다.", "Records found in other apps. These observations do not confirm the current runtime state. Input and stopping are unavailable."],
  "새로 고침": ["새로 고침", "Refresh"],
  "ACTIVITY": ["활동", "ACTIVITY"],
  "알림": ["알림", "Notifications"],
  "새로운 알림이 없습니다.": ["새로운 알림이 없습니다.", "No new notifications."],
  "EXIT WORKSPACE": ["작업 공간 종료", "EXIT WORKSPACE"],
  "OpenDIY Desktop를 종료할까요?": ["OpenDIY Desktop를 종료할까요?", "Exit OpenDIY Desktop?"],
  "앱이 소유한 모든 실행 세션이 종료됩니다. 다른 앱의 세션은 제어하지 않습니다. 창의 닫기 버튼은 트레이로 숨깁니다.": ["앱이 소유한 모든 실행 세션이 종료됩니다. 다른 앱의 세션은 제어하지 않습니다. 창의 닫기 버튼은 트레이로 숨깁니다.", "All running sessions owned by this app will stop. Sessions in other apps are unaffected. The window close button hides the app to the tray."],
  "계속 작업": ["계속 작업", "Keep working"],
  "세션 종료 후 앱 닫기": ["세션 종료 후 앱 닫기", "Stop sessions and exit"],
  "원격 작업의 중단을 확인하지 못했습니다. 앱을 닫아도 원격 작업이 계속 실행될 수 있습니다. 로컬 자원은 정리되었습니다.": ["원격 작업의 중단을 확인하지 못했습니다. 앱을 닫아도 원격 작업이 계속 실행될 수 있습니다. 로컬 자원은 정리되었습니다.", "The remote stop could not be confirmed. Remote work may continue running after the app closes. Local resources have been cleaned up."],
  "중단 미확인 상태로 앱 닫기": ["중단 미확인 상태로 앱 닫기", "Close app with unconfirmed remote stop"],
  "앞으로 이동": ["앞으로 이동", "Move earlier"],
  "뒤로 이동": ["뒤로 이동", "Move later"],
  "세션 최대화": ["세션 최대화", "Maximize session"],
  "이 세션 종료": ["이 세션 종료", "Stop this session"],
  "직접 입력 · IME · 붙여넣기": ["직접 입력 · IME · 붙여넣기", "Direct input · IME · paste"],
};
function localizeMarkup(root: Element) {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    const text = node.textContent ?? ''; const labels = markupLabels[text.trim()];
    if (labels) { const leading = text.match(/^\s*/)?.[0] ?? ''; const trailing = text.match(/\s*$/)?.[0] ?? ''; bindText(node, leading + labels[0] + trailing, leading + labels[1] + trailing); }
  }
  for (const element of [root, ...root.querySelectorAll('*')]) for (const attribute of ['title', 'aria-label', 'placeholder']) {
    const labels = markupLabels[element.getAttribute(attribute) ?? ''];
    if (labels) bindAttribute(element, attribute, ...labels);
  }
}
localizeMarkup($('#app'));
const languageSelect = $<HTMLSelectElement>('#language-select'); languageSelect.value = getLanguage();
let nativeLanguageReady = false;
let nativeLanguageQueue: Promise<void> = Promise.resolve();
let nativeLanguageRevision = 0;
function showLanguageStatus(ko: string, en: string, pending = false) {
  const node = $('#language-error'); node.hidden = false; node.dataset.state = pending ? 'pending' : 'error'; node.setAttribute('role', pending ? 'status' : 'alert');
  bindText(node, ko, en);
}
function showLanguageError(error: unknown) {
  showLanguageStatus(`언어 설정을 저장할 수 없습니다. 이번 실행에는 적용됩니다. ${message(error, 'ko')}`, `Cannot save the language setting. It applies for this session. ${message(error, 'en')}`);
}
function showNativeCacheStatus() {
  const error = languageStorageError();
  if (error === undefined) $('#language-error').hidden = true;
  else showLanguageStatus(`앱의 언어 설정은 저장됐지만 로컬 캐시는 저장하지 못했습니다. ${message(error, 'ko')}`, `The app language is saved, but its local cache could not be updated. ${message(error, 'en')}`);
}
languageSelect.disabled = native;
languageSelect.onchange = () => {
  const previous = getLanguage(); $('#language-error').hidden = true;
  try { setLanguage(languageSelect.value as 'ko' | 'en'); }
  catch (error) { if (!native) showLanguageError(error); }
  // An explicit choice can replace an unreadable setting; startup never does so automatically.
  if (native && !nativeLanguageReady) { nativeLanguageReady = true; void syncNativeLanguage(getLanguage()); }
  else if (native && previous === getLanguage()) void syncNativeLanguage(getLanguage());
};
onLanguageChange(() => {
  languageSelect.value = getLanguage();
  for (const field of createFields) if (field.validity.customError) validateRequired(field);
  if (native && nativeLanguageReady) void syncNativeLanguage(getLanguage());
});
if (!native && languageStorageError() !== undefined) showLanguageError(languageStorageError());
function syncNativeLanguage(language: 'ko' | 'en'): Promise<void> {
  const revision = ++nativeLanguageRevision;
  showLanguageStatus('언어 설정 저장 중…', 'Saving language setting…', true);
  // Capture the requested language and serialize durable saves, including rapid toggles.
  const operation = nativeLanguageQueue.catch(() => {}).then(async () => { await call('set_ui_language', { language }); });
  nativeLanguageQueue = operation;
  void operation.then(() => { if (revision === nativeLanguageRevision) showNativeCacheStatus(); }, error => {
    if (revision === nativeLanguageRevision) showLanguageError(error);
    notifyError('언어 설정 저장 실패', 'Could not save language setting', error);
  });
  return operation;
}
async function initializeNativeLanguage() {
  showLanguageStatus('언어 설정 확인 중…', 'Loading language setting…', true);
  try {
    const savedLanguage = await call<unknown>('get_ui_language');
    if (savedLanguage !== 'ko' && savedLanguage !== 'en') throw new UiError('저장된 언어 설정이 올바르지 않습니다.', 'The saved language setting is invalid.');
    if (savedLanguage === 'ko' || savedLanguage === 'en') {
      try { setLanguage(savedLanguage); } catch { /* Native storage is authoritative; report a cache failure after the durable save. */ }
    }
    nativeLanguageReady = true;
  } catch (error) {
    showLanguageStatus(`저장된 언어를 확인하지 못했습니다. 기존 설정을 덮어쓰지 않습니다. ${message(error, 'ko')}`, `Could not read the saved language. The existing setting will not be overwritten. ${message(error, 'en')}`);
    notifyError('언어 설정 조회 실패', 'Could not load language setting', error);
  } finally { languageSelect.disabled = false; $('#app').hidden = false; }
  if (nativeLanguageReady) await syncNativeLanguage(getLanguage()).catch(() => {}); // A failed save remains visible while the rest of the app starts.
}

class UiError extends Error { constructor(readonly ko: string, readonly en: string) { super(t(ko, en)); } }
function message(error: unknown, language = getLanguage()) { return error instanceof UiError ? language === 'ko' ? error.ko : error.en : error instanceof Error ? error.message : String(error); }
function showError(node: Node, error: unknown) { bindText(node, message(error, 'ko'), message(error, 'en')); }
function notifyError(ko: string, en: string, error: unknown) { notify(`${ko}: ${message(error, 'ko')}`, true, `${en}: ${message(error, 'en')}`); }
function listStatus(node: Element, ko: string, en: string) { const text = document.createElement('span'); bindText(text, ko, en); node.replaceChildren(text); }
function resumeTitle(value: string) {
  const clean = value.replace(/\p{Cc}/gu, ' ').trim() || t('이름 없는 세션', 'Untitled session');
  const encoder = new TextEncoder(); let title = ''; let bytes = 0;
  // Catalog previews can be longer than native session labels. Keep whole Unicode characters.
  for (const character of clean) {
    const size = encoder.encode(character).byteLength;
    if (bytes + size > 512) break;
    title += character; bytes += size;
  }
  return title.trim() || t('이름 없는 세션', 'Untitled session');
}
function projectPath(cwd: string) { return cwd.replace(/^\\\\\?\\UNC\\/i, '\\\\').replace(/^\\\\\?\\/, '').replace(/\//g, '\\'); }
function selectedProjectCwd() { return projectGroups(importedProjects, [...sessions.values()]).find(project => project.key === selectedProject)?.cwd ?? undefined; }
function selectedProjectId() { return importedProjects.find(project => `id:${project.id}` === selectedProject)?.id; }
function visibleOrder() { return order.filter(id => isVisible(id)); }
function visibleSessions() { return visibleOrder().map(id => sessions.get(id)!); }
function isVisible(id: string) { const session = sessions.get(id); return !!session && (!selectedProject || sessionProjectKey(session, importedProjects) === selectedProject); }
function selectProject(key: string) {
  projectSelectionVersion++; selectedProject = key; maximized = ''; dragging = '';
  if (!isVisible(active)) active = visibleOrder()[0] ?? '';
  for (const [id, view] of terminals) { view.input.cancelPending(); view.resize.cancelPending(); clearTimeout(view.resizeTimer); refreshInputState(id); if (!isVisible(id)) view.terminal.blur(); }
  bindText($('#search-result'), '', '');
  renderAll();
  const selectionVersion = projectSelectionVersion;
  for (const id of visibleOrder()) {
    const view = terminals.get(id)!; const generation = sessions.get(id)!.generation;
    // 선행 출력 파싱 뒤, 현재 선택과 터미널에만 최신 화면 위치를 적용한다.
    view.terminal.write('', () => {
      if (projectSelectionVersion !== selectionVersion || terminals.get(id) !== view || sessions.get(id)?.generation !== generation || view.card.hidden) return;
      view.fit.fit(); view.terminal.scrollToBottom();
    });
  }
}
function focusSession(id: string) {
  const session = sessions.get(id); if (!session) return;
  if (!isVisible(id)) selectProject(sessionProjectKey(session, importedProjects));
  if (maximized) maximized = id;
  renderGrid(); setActive(id);
}
function notify(text: string, error = false, english = text) {
  const toast = document.createElement('div');
  toast.className = `toast${error ? ' error' : ''}`;
  bindText(toast, text, english);
  $('#toast-area').append(toast);
  setTimeout(() => toast.remove(), error ? 12000 : 6000);

}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!native) throw new UiError(demo ? '디자인 데모에서는 프로세스를 실행하거나 입력할 수 없습니다.' : '데스크톱 앱에서 사용할 수 있는 기능입니다.', demo ? 'The design demo cannot run processes or accept input.' : 'This feature is available in the desktop app.');
  return invoke<T>(command, args);
}

function prepareRuntime(): Promise<void> {
  if (!native || exitInProgress) return Promise.resolve();
  if (runtimePreparation) return runtimePreparation;
  const generation = runtimeGeneration;
  const mutation = executableMutation;
  $('#runtime-banner').hidden = false;
  const retry = $<HTMLButtonElement>('#runtime-retry'); retry.hidden = true; retry.disabled = true;
  bindText($('#runtime-status'), '공유서버를 준비하는 중… 준비되면 이전 세션을 복원합니다.', 'Preparing the shared server… Saved sessions will resume when it is ready.');
  const operation = Promise.resolve().then(async () => {
    // A settings command can save its selection before its IPC response arrives.
    if (mutation) await mutation.catch(() => {});
    // The native startup operation has one owner; invalidation is not cancellation.
    if (runtimeCommand) await runtimeCommand.catch(() => {});
    if (generation !== runtimeGeneration || exitInProgress) return false;
    const command = call<void>('prepare_runtime'); runtimeCommand = command; runtimeCommandGeneration = generation;
    try { await command; }
    finally { if (runtimeCommand === command) runtimeCommand = undefined; }
    return true;
  });
  runtimePreparation = operation.then(prepared => {
    if (generation !== runtimeGeneration) return prepareRuntime();
    if (!prepared) { runtimePreparation = undefined; return; }
    $('#runtime-banner').hidden = true;
    runtimeReady = true;
    weeklyUsage.setVisible(nativeWindowVisible);
  }, error => {
    if (generation !== runtimeGeneration) return prepareRuntime();
    bindText($('#runtime-status'), `공유서버 준비 실패: ${message(error, 'ko')}`, `Could not prepare the shared server: ${message(error, 'en')}`);
    bindText(retry, '다시 시도', 'Retry'); retry.hidden = false; retry.disabled = false;
    throw error;
  });
  return runtimePreparation;
}

function resumeWorkspace(): Promise<void> {
  if (runtimeRestoration) return runtimeRestoration;
  if (exitInProgress) return Promise.resolve();
  restoringWorkspace = true; updateRestoreBanner();
  const operation = (async () => {
    try {
      await prepareRuntime();
      for (const request of [...saved]) {
        if (exitInProgress || sessions.size + pendingStarts >= MAX_WORKSPACE_SESSIONS) break;
        if (!saved.includes(request) || request.resumeOnStartup !== true || !request.threadId) continue;
        try { await restoreSaved(request, false); } catch (error) { notifyError('자동 복원 실패', 'Could not automatically restore session', error); }
      }
    } finally {
      restoringWorkspace = false; updateRestoreBanner(); scheduleSave(); renderAll();
    }
  })();
  runtimeRestoration = operation;
  void operation.finally(() => { runtimeRestoration = undefined; }).catch(() => {});
  return operation;
}
$('#runtime-retry').onclick = () => {
  if (exitInProgress || runtimeRestoration) return;
  runtimePreparation = undefined;
  weeklyUsage.refresh();
  void resumeWorkspace().catch(error => notifyError('공유서버 준비 실패', 'Could not prepare the shared server', error));
};

function pendingRestoreOwnerIds() {
  return new Set([...failedRestoreCards].filter(id => saved.some(request => request.threadId && sessions.get(id)?.threadId === request.threadId)));
}
function workspaceLayout(): Layout {
  const pendingOwners = pendingRestoreOwnerIds();
  const live = order.filter(id => !pendingOwners.has(id));
  // Reordering live cards leaves unresolved restore slots in place.
  workspaceSlots = workspaceSlots.flatMap<SavedSession | string>(slot => typeof slot === 'string'
    ? sessions.has(slot) && live.length ? [live.shift()!] : []
    : saved.includes(slot) ? [slot] : []);
  workspaceSlots.push(...live);
  if (workspaceSlots.length > MAX_SAVED_WORKSPACE_SESSIONS) throw new UiError('저장 메타데이터는 최대 512개입니다. 기존 저장은 보존했습니다. 불필요한 저장 항목을 제거한 뒤 다시 시도하세요.', 'Workspace metadata is limited to 512 entries. Existing storage was preserved. Remove unused saved entries and retry.');
  return { version: 1, columns, columnSizes: [...columnSizes], rowSizes: [...rowSizes], sessions: workspaceSlots.map(slot => {
    if (typeof slot !== 'string') return savedSessionMetadata(slot);
    const info = sessions.get(slot)!;
    return savedSessionMetadata({ ...info, sessionId: info.id, resumeOnStartup: !explicitlyStoppedSessions.has(info.id) && (exitResumeIntents.has(info.id) || (info.active && info.state !== 'stopping')) });
  }) };
}

async function persistLayout(): Promise<void> {
  let layout: Layout;
  try { layout = workspaceLayout(); }
  catch (error) { bindText($('#save-status'), '레이아웃 저장 실패', 'Could not save layout'); notifyError('저장 실패', 'Could not save', error); throw error; }
  bindText($('#save-status'), '저장 중…', 'Saving…');
  const operation = saveQueue.catch(() => {}).then(() => call('save_layout', { layout })).then(() => { bindText($('#save-status'), '레이아웃 저장됨', 'Layout saved'); });
  saveQueue = operation;
  void operation.catch(error => { bindText($('#save-status'), '레이아웃 저장 실패', 'Could not save layout'); notifyError('저장 실패', 'Could not save', error); });
  return operation;
}
function scheduleSave() {
  if (!native || !workspaceReady || restoringWorkspace || restoringRequests.size || exitInProgress) return;
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => { void persistLayout().catch(() => {}); }, 400);
}
function updateRestoreBanner() {
  $('#restore-banner').hidden = saved.length === 0;
  bindText($('#restore-banner span'), restoringWorkspace ? '이전 작업 공간을 복원하는 중…' : '자동으로 연결하지 못한 저장 세션이 있습니다. 목록에서 다시 연결하거나 제거할 수 있습니다.', restoringWorkspace ? 'Restoring the previous workspace…' : 'Some saved sessions could not reconnect automatically. Retry or remove them from the list.');
  if ($<HTMLDialogElement>('#restore-dialog').open) renderRestoreList();
}
function bindRestored(request: SavedSession, info: SessionInfo) {
  // A delayed success cannot undo a stop confirmed while this restore was pending.
  if (cancelledRestores.has(request)) explicitlyStoppedSessions.add(info.id);
  workspaceSlots = workspaceSlots.map(slot => slot === request ? info.id : slot);
  // Duplicate saved references to the same conversation share one terminal.
  workspaceSlots = workspaceSlots.filter((slot, index, slots) => typeof slot !== 'string' || slots.indexOf(slot) === index);
  saved = saved.filter(item => item !== request);
  const restoredOrder = workspaceSlots.filter((slot): slot is string => typeof slot === 'string' && sessions.has(slot));
  order = [...restoredOrder, ...order.filter(id => !restoredOrder.includes(id))];
  renderAll(); updateRestoreBanner();
}
function restoreSaved(request: SavedSession, focus = true): Promise<SessionInfo> {
  const pending = restoringRequests.get(request); if (pending) return pending;
  cancelledRestores.delete(request);
  for (const id of failedRestoreCards) if (sessions.get(id)?.threadId === request.threadId && sessions.get(id)?.active === false) removeSession(id, true);
  const before = new Set(sessions.keys());
  const operation = start(request, focus).then(info => {
    if (!info.active || ['error', 'stopping'].includes(info.state)) throw new UiError('세션 연결이 아직 완료되지 않았습니다. 기존 세션 상태를 확인한 뒤 다시 연결하세요.', 'The session is not connected yet. Check its existing state before retrying.');
    bindRestored(request, info); return info;
  }).catch(error => {
    // A rejected launch may have emitted a terminal error. Keep live ownership,
    // but remove newly reaped error cards so the saved retry does not duplicate them.
    for (const info of [...sessions.values()]) if ((!before.has(info.id) || failedRestoreCards.has(info.id)) && info.threadId === request.threadId) {
      failedRestoreCards.add(info.id);
      if (info.active) Object.assign(request, { sessionId: info.id, projectId: info.projectId ?? null, projectAssigned: info.projectAssigned ?? false, projectRevision: info.projectRevision ?? 0 });
      if (!info.active) removeSession(info.id, true);
    }
    throw error;
  }).finally(() => { restoringRequests.delete(request); cancelledRestores.delete(request); updateRestoreBanner(); scheduleSave(); });
  restoringRequests.set(request, operation); updateRestoreBanner();
  return operation;
}

function setActive(id: string, focus = true) {
  if (id && !isVisible(id)) return;
  active = id;
  for (const [key, view] of terminals) view.card.classList.toggle('focused', key === id);
  for (const element of document.querySelectorAll<HTMLElement>('[data-session-link]')) element.classList.toggle('active', element.dataset.sessionLink === id);
  if (focus) { const view = terminals.get(id); if (view && !view.card.querySelector<HTMLElement>('.conversation-host')?.hidden) view.card.querySelector<HTMLTextAreaElement>('.conversation-input')?.focus(); else view?.terminal.focus(); }
}

function canType(id: string) {
  const session = sessions.get(id);
  return native && !!session?.active && isVisible(id) && !['restored', 'starting', 'stopping', 'stopped', 'exited'].includes(session.state);
}
function refreshInputState(id: string) {
  const view = terminals.get(id); if (!view) return;
  view.terminal.options.disableStdin = !canType(id) || conversationOwners.has(id) || view.input.paused || view.input.disposed;
  view.card.dataset.inputPending = String(view.input.size); view.card.dataset.inputBytes = String(view.input.byteLength);
  const paused = String(view.input.paused);
  if (view.card.dataset.inputPaused !== paused) {
    view.card.dataset.inputPaused = paused;
    bindText(view.card.querySelector('.terminal-hint')!, view.input.paused ? '입력 전송 대기 중 · 완료되면 입력이 재개됩니다' : '직접 입력 · IME · 붙여넣기', view.input.paused ? 'Input paused until pending writes finish' : 'Direct input · IME · paste');
  }
  if (!view.input.size) view.inputWarning = false;
}
function inputError(error: unknown) {
  return error instanceof InputQueueError ? new UiError(error.reason === 'message' ? '한 번에 64KiB 이하로 입력하세요. 이 입력은 전송하지 않았습니다.' : error.reason === 'full' ? '입력 전송 대기열이 가득 찼습니다. 이 입력은 전송하지 않았습니다. 완료 후 다시 시도하세요.' : '대기 중 프로젝트 또는 세션 상태가 변경되어 전송하지 않았습니다.', error.reason === 'message' ? 'Input must be at most 64KiB per operation. This input was not sent.' : error.reason === 'full' ? 'The input queue is full. This input was not sent. Retry after pending writes finish.' : 'Not sent because the project or session state changed while queued.') : error;
}
function sendInput(id: string, data: string) {
  const session = sessions.get(id); const view = terminals.get(id);
  if (!session || !view || !canType(id) || conversationOwners.has(id)) return;
  void view.input.enqueue({ data, generation: session.generation, selectionVersion: projectSelectionVersion }).catch(error => {
    if (error instanceof InputQueueError && error.reason === 'cancelled') return;
    if (error instanceof InputQueueError && view.inputWarning) return;
    if (error instanceof InputQueueError) view.inputWarning = true;
    notifyError(`${session.title} 입력 실패`, `${session.title} input failed`, inputError(error));
  });
}

function queueResize(id: string) {
  const view = terminals.get(id); const session = sessions.get(id); if (!native || !view || !session) return;
  clearTimeout(view.resizeTimer);
  const generation = session.generation; const selectionVersion = projectSelectionVersion;
  view.resizeTimer = setTimeout(() => view.resize.enqueue({ generation, selectionVersion, cols: view.terminal.cols, rows: view.terminal.rows }), 60);
}

function mountConversation(session: Session) {
  const view = terminals.get(session.id)!;
  const { card, terminal, fit, input } = view;
  card.querySelector('.conversation-host')?.remove();
  const conversationHost = document.createElement('div');
  card.querySelector('.terminal-host')!.after(conversationHost);
  conversationHost.hidden = true;
  let conversation: ReturnType<typeof createConversation> | undefined;
  const toggle = card.querySelector<HTMLButtonElement>('.conversation-toggle')!;
  function getConversation() {
    if (!conversation) {
      conversation = createConversation({ sessionId: session.id, generation: session.generation, host: conversationHost, call,
        ownerChanged(owned) { if (conversations.get(session.id) !== conversation) return; if (owned) { conversationOwners.add(session.id); input.cancelPending(); } else conversationOwners.delete(session.id); refreshInputState(session.id); },
      });
      conversations.set(session.id, conversation);
    }
    return conversation!;
  }
  toggle.onclick = async () => {
    toggle.disabled = true;
    try {
      getConversation();
      const show = conversationHost.hidden;
      if (show) await conversation!.setVisible(true);
      else await conversation!.handoffToTerminal();
      await conversation!.setForeground(!card.hidden && !document.hidden && nativeWindowVisible);
      card.querySelector<HTMLElement>('.terminal-host')!.hidden = show;
      toggle.setAttribute('aria-pressed', String(show));
      if (!show) { fit.fit(); terminal.focus(); }
    } catch (error) {
      const showingConversation = !conversationHost.hidden;
      card.querySelector<HTMLElement>('.terminal-host')!.hidden = showingConversation;
      toggle.setAttribute('aria-pressed', String(showingConversation));
      notify('보기를 전환하지 못했습니다. 현재 화면과 작성 중인 내용을 확인한 뒤 다시 시도하세요.', true, 'Could not switch views. Check the current view and your draft before trying again.');
    }
    finally { toggle.disabled = false; }
  };
  contextControls.get(session.id)?.dispose();
  contextControls.set(session.id, createContextControls({
    host: card.querySelector<HTMLElement>('.context-controls')!, statusHost: card.querySelector<HTMLElement>('.session-compaction')!, sessionId: session.id, generation: session.generation, call, notify,
    async prepare() {
      if (toggle.disabled) throw new Error('대화 화면 전환 중');
      toggle.disabled = true;
      try {
        const controller = getConversation();
        await controller.setVisible(true);
        await controller.setForeground(!card.hidden && !document.hidden && nativeWindowVisible);
        card.querySelector<HTMLElement>('.terminal-host')!.hidden = true;
        toggle.setAttribute('aria-pressed', 'true');
      } catch (error) {
        const showingConversation = !conversationHost.hidden;
        card.querySelector<HTMLElement>('.terminal-host')!.hidden = showingConversation;
        toggle.setAttribute('aria-pressed', String(showingConversation));
        throw error;
      } finally { toggle.disabled = false; }
    },
    compact: () => getConversation().compact(),
  }));
}

function createTerminal(session: Session) {
  const card = document.createElement('article'); card.className = 'terminal-card'; card.dataset.id = session.id;
  card.innerHTML = `<header class="terminal-header"><span class="drag-handle" draggable="true" title="드래그로 순서 변경" aria-hidden="true">⠿</span><span class="terminal-title"></span><span class="session-compaction" role="status" hidden></span><span class="session-state"></span><div class="card-actions"><button class="session-id-copy icon-button" type="button" disabled><svg viewBox="0 0 24 24" aria-hidden="true"><rect x="8" y="8" width="12" height="13" rx="2"/><path d="M16 8V5a2 2 0 0 0-2-2H5a2 2 0 0 0-2 2v9a2 2 0 0 0 2 2h3"/></svg></button><span class="context-controls"></span><button class="subagents-open icon-button" type="button" aria-haspopup="dialog" aria-controls="subagents-dialog"><svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="5" r="3"/><circle cx="5" cy="19" r="3"/><circle cx="19" cy="19" r="3"/><path d="M12 8v4M5 16v-4h14v4"/></svg></button><button class="cron-open icon-button" type="button" aria-haspopup="dialog" aria-controls="cron-dialog"><svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 6v6l4 2"/></svg></button><button class="move-left icon-button" aria-label="앞으로 이동" title="앞으로 이동">‹</button><button class="move-right icon-button" aria-label="뒤로 이동" title="뒤로 이동">›</button><button class="conversation-toggle icon-button" type="button" aria-label="대화 보기" title="대화 / 터미널">☷</button><button class="maximize icon-button" aria-label="세션 최대화" title="세션 최대화">⛶</button><button class="stop icon-button" aria-label="이 세션 종료" title="이 세션 종료">×</button></div></header><div class="terminal-path"></div><div class="terminal-host"></div><div class="terminal-footer"><span class="terminal-identity"></span><span class="terminal-hint">직접 입력 · IME · 붙여넣기</span></div>`;
  localizeMarkup(card);
  const terminal = new Terminal({ fontFamily: '"Cascadia Code", "Cascadia Mono", Consolas, monospace', fontSize: 12, lineHeight: 1.45, cursorBlink: true, scrollback: 10000, disableStdin: !native, allowProposedApi: false, theme: { background: '#141820', foreground: '#ced5e3', cursor: '#a5b4fc', selectionBackground: '#3b4567', black: '#212735', red: '#f78b99', green: '#91d6ad', yellow: '#eac184', blue: '#8eaafb', magenta: '#c4a4f3', cyan: '#87cfd9', white: '#e3e8f0' } });
  const fit = new FitAddon(); const search = new SearchAddon(); terminal.loadAddon(fit); terminal.loadAddon(search);
  $('#grid').append(card); terminal.open(card.querySelector('.terminal-host')!);
  let frame = 0;
  const observer = new ResizeObserver(() => { cancelAnimationFrame(frame); frame = requestAnimationFrame(() => { if (card.offsetWidth && card.offsetHeight) fit.fit(); }); });
  observer.observe(card.querySelector('.terminal-host')!);
  const input = new TerminalInputQueue<QueuedInput>(async value => {
    const current = sessions.get(session.id);
    if (!current || value.selectionVersion !== projectSelectionVersion || current.generation !== value.generation || (!canType(session.id) || conversationOwners.has(session.id))) throw new InputQueueError('cancelled');
    await call<void>('session_write', { id: session.id, generation: value.generation, data: value.data });
  }, () => refreshInputState(session.id));
  const resize = new TerminalResizeQueue<QueuedResize>(async value => {
    const current = sessions.get(session.id);
    if (!current || value.selectionVersion !== projectSelectionVersion || current.generation !== value.generation || !canType(session.id)) return;
    await call('session_resize', { id: session.id, generation: value.generation, cols: value.cols, rows: value.rows });
  }, error => notifyError('터미널 크기 변경 실패', 'Could not resize terminal', error));
  terminals.set(session.id, { terminal, fit, search, card, observer, input, resize, inputWarning: false });
  refreshInputState(session.id);
  mountConversation(session);
  terminal.onData(data => sendInput(session.id, data));
  terminal.onResize(() => queueResize(session.id));
  terminal.attachCustomKeyEventHandler(event => {
    if (event.key === 'Enter' && event.shiftKey && !event.altKey && !event.ctrlKey && !event.metaKey) {
      // Windows 콘솔에서도 줄바꿈으로 해석되는 Alt+Enter를 보낸다.
      // 용량 제한과 세션 세대 검사를 거치는 기존 입력 대기열을 사용한다.
      if (event.isComposing || event.keyCode === 229) return false;
      event.preventDefault();
      if (event.type === 'keydown') sendInput(session.id, '\u001b\r');
      return false;
    }
    return !(event.type === 'keydown' && ((event.altKey && /^[1-9]$/.test(event.key)) || (event.ctrlKey && event.key === 'Tab') || (event.ctrlKey && event.shiftKey && event.key.toLowerCase() === 'f')));
  });
  card.addEventListener('pointerdown', () => setActive(session.id, false));
  card.querySelector('.maximize')!.addEventListener('click', () => { maximized = maximized === session.id ? '' : session.id; renderGrid(); setActive(session.id); });
  card.querySelector('.stop')!.addEventListener('click', () => {
    const current = sessions.get(session.id)!;
    if (!current.active) { removeSession(session.id); return; }
    if (confirm(t(`“${current.title}” 세션을 종료할까요? 실행 중인 작업이 중단됩니다.`, `Stop session “${current.title}”? Running work will be interrupted.`))) {
      recordExplicitStop(current);
      void call('session_stop', { id: current.id, generation: current.generation }).then(() => {
        // A reaped error may not emit another state event. A live observation error
        // is removed only after the native stop has completed its actual cleanup.
        if (current.state === 'error' && sessions.get(current.id)?.generation === current.generation) removeSession(current.id);
      }).catch(error => notifyError('종료 실패', 'Could not stop session', error));
    }
  });
  const copyIdButton = card.querySelector<HTMLButtonElement>('.session-id-copy')!;
  bindAttribute(copyIdButton, 'aria-label', '세션 ID 복사', 'Copy session ID'); bindAttribute(copyIdButton, 'title', '세션 ID 복사', 'Copy session ID');
  copyIdButton.onclick = async () => {
    const threadId = sessions.get(session.id)?.threadId;
    if (!threadId || demo) return;
    try { await navigator.clipboard.writeText(threadId); notify('세션 ID를 복사했습니다.', false, 'Session ID copied.'); }
    catch { notify('세션 ID를 복사하지 못했습니다. 다시 시도하세요.', true, 'Could not copy the session ID. Try again.'); }
  };
  const subagentsButton = card.querySelector<HTMLButtonElement>('.subagents-open')!;
  bindAttribute(subagentsButton, 'aria-label', '서브에이전트 관리', 'Manage subagents'); bindAttribute(subagentsButton, 'title', '서브에이전트 관리', 'Manage subagents');
  subagentsButton.onclick = () => { const current = sessions.get(session.id); if (current?.active && current.threadId && !exitInProgress) void subagentManager.open(current); };
  const cronButton = card.querySelector<HTMLButtonElement>('.cron-open')!;
  bindAttribute(cronButton, 'aria-label', '예약 작업', 'Scheduled jobs'); bindAttribute(cronButton, 'title', '예약 작업', 'Scheduled jobs');
  cronButton.onclick = () => { const current = sessions.get(session.id); if (current?.active && !exitInProgress) void cronManager.open(current); };
  const projectMove = document.createElement('button'); projectMove.className = 'project-move icon-button'; projectMove.textContent = '▱'; bindAttribute(projectMove, 'aria-label', '프로젝트로 이동', 'Move to project'); bindAttribute(projectMove, 'title', '프로젝트로 이동', 'Move to project'); projectMove.onclick = () => projectManager.move(sessions.get(session.id)!); card.querySelector('.card-actions')!.prepend(projectMove);
  card.querySelector('.move-left')!.addEventListener('click', () => move(session.id, -1));
  card.querySelector('.move-right')!.addEventListener('click', () => move(session.id, 1));
  card.querySelector('.drag-handle')!.addEventListener('dragstart', event => { dragging = session.id; (event as DragEvent).dataTransfer!.effectAllowed = 'move'; (event as DragEvent).dataTransfer!.setData('text/plain', session.id); card.classList.add('dragging'); });
  card.addEventListener('dragend', () => { dragging = ''; card.classList.remove('dragging'); });
  card.addEventListener('dragover', event => { if (dragging && dragging !== session.id) event.preventDefault(); });
  card.addEventListener('drop', event => { event.preventDefault(); if (!dragging || dragging === session.id || !isVisible(dragging) || !isVisible(session.id)) return; const visible = visibleOrder(); const from = visible.indexOf(dragging); const to = visible.indexOf(session.id); if (from < 0 || to < 0) return; visible.splice(from, 1); visible.splice(to, 0, dragging); let index = 0; order = order.map(id => isVisible(id) ? visible[index++] : id); renderGrid(); renderSide(); scheduleSave(); });
  return terminal;
}

function move(id: string, offset: number) { const visible = visibleOrder(); const index = visible.indexOf(id); const next = index + offset; if (index < 0 || next < 0 || next >= visible.length) return; const from = order.indexOf(id); const to = order.indexOf(visible[next]); [order[from], order[to]] = [order[to], order[from]]; renderGrid(); renderSide(); scheduleSave(); }
function recordExplicitStop(session: SessionInfo) {
  exitResumeIntents.delete(session.id); explicitlyStoppedSessions.add(session.id);
  if (!session.threadId) return;
  for (const request of new Set([...saved, ...restoringRequests.keys()])) if (request.threadId === session.threadId) {
    request.resumeOnStartup = false;
    if (restoringRequests.has(request)) cancelledRestores.add(request);
  }
}
function removeSession(id: string, preserveRestore = false) {
  const session = sessions.get(id);
  if (!preserveRestore && session) recordExplicitStop(session);
  if (!preserveRestore && session?.threadId) {
    const removed = saved.filter(request => request.threadId === session.threadId);
    saved = saved.filter(request => !removed.includes(request));
    workspaceSlots = workspaceSlots.filter(slot => typeof slot === 'string' || !removed.includes(slot));
    updateRestoreBanner();
  }
  contextControls.get(id)?.dispose(); contextControls.delete(id); workspaceSlots = workspaceSlots.filter(slot => slot !== id); exitResumeIntents.delete(id); failedRestoreCards.delete(id); explicitlyStoppedSessions.delete(id); const view = terminals.get(id); const conversation = conversations.get(id); conversations.delete(id); conversationOwners.delete(id); if (conversation) void conversation.dispose({ sessionEnded: true }).catch(error => notify('대화 연결 정리를 확인하지 못했습니다. 세션 상태를 확인하세요.', true, 'Conversation cleanup could not be confirmed. Check the session status.')); clearTimeout(view?.resizeTimer); view?.input.dispose(); view?.resize.dispose(); view?.observer.disconnect(); view?.terminal.dispose(); view?.card.remove(); terminals.delete(id); sessions.delete(id); order = order.filter(item => item !== id); if (maximized === id) maximized = ''; if (active === id) active = visibleOrder()[0] ?? ''; renderAll(); scheduleSave(); }

function receiveSession(info: SessionInfo, persist = true) {
  const previous = sessions.get(info.id); const session = updateSession(previous, info);
  if (session === previous) return;
  sessions.set(session.id, session);
  // Pending restores retain object identity while current membership advances.
  for (const request of saved) if (request.threadId && request.threadId === session.threadId && request.sessionId === session.id && typeof session.projectRevision === 'number' && session.projectRevision >= (request.projectRevision ?? 0)) {
    Object.assign(request, { projectId: session.projectId ?? null, projectAssigned: session.projectAssigned ?? false, projectRevision: session.projectRevision });
  }
  if (!order.includes(session.id)) order.push(session.id);
  const terminal = terminals.get(session.id)?.terminal ?? createTerminal(session);
  if (previous && previous.generation !== session.generation) { const view = terminals.get(session.id)!; view.input.cancelPending(); view.resize.cancelPending(); clearTimeout(view.resizeTimer); terminal.write('', () => terminal.reset()); const oldConversation = conversations.get(session.id); conversations.delete(session.id); conversationOwners.delete(session.id); if (oldConversation) void oldConversation.dispose({ sessionEnded: true }).catch(error => notify('이전 대화 연결 정리를 확인하지 못했습니다. 세션 상태를 확인하세요.', true, 'Previous conversation cleanup could not be confirmed. Check the session status.')); mountConversation(session); }
  if (previous && previous.state !== session.state) notify(`${session.title} · ${stateLabels(session.state)[0]}`, false, `${session.title} · ${stateLabels(session.state)[1]}`);
  renderAll();
  if (!previous || previous.state === 'starting') queueResize(session.id);
  const buffered = pending.get(session.id); if (buffered) { pending.delete(session.id); buffered.forEach(receiveOutput); }
  if (!active) setActive(session.id, false);
  if (persist) scheduleSave();
}

const outputAcknowledgements = new TerminalOutputAcknowledgements(output => { if (native) void call('session_output_ack', { id: output.id, generation: output.generation, seq: output.seq }).catch(error => notifyError('출력 확인 전송 실패', 'Could not acknowledge output', error)); });
function ack(output: Output) { outputAcknowledgements.enqueue(output); }
async function resolveOrphan(id: string) {
  if (!native || resolving.has(id)) return;
  resolving.add(id);
  try {
    const list = await call<SessionInfo[]>('list_sessions');
    const session = list.find(item => item.id === id);
    if (session) receiveSession(session);
    else {
      const queue = pending.get(id); pending.delete(id);
      if (queue?.length) { queue.forEach(ack); notify('소유 세션을 찾을 수 없는 출력이 도착했습니다.', true, 'Output arrived for a session that is no longer owned.'); }
    }
  } catch (error) {
    const queue = pending.get(id); pending.delete(id); queue?.forEach(ack);
    notifyError('터미널 메타데이터 조회 실패', 'Could not load terminal metadata', error);
  } finally { resolving.delete(id); }
}
function receiveOutput(output: Output) {
  const session = sessions.get(output.id);
  if (!session) {
    const queue = pending.get(output.id) ?? [];
    // Bound startup metadata races; do not permit an orphan event to grow memory forever.
    if (pending.size < 32 && queue.reduce((size, part) => size + part.data.length, 0) + output.data.length <= 524288) { queue.push(output); pending.set(output.id, queue); void resolveOrphan(output.id); }
    else { notify('메타데이터가 없는 터미널 출력이 버퍼 한도를 초과했습니다.', true, 'Terminal output without metadata exceeded the buffer limit.'); ack(output); }
    return;
  }
  if (!acceptOutput(session, output)) { ack(output); return; }
  session.seq = output.seq;
  const terminal = terminals.get(output.id)?.terminal;
  if (terminal) terminal.write(output.data, () => ack(output)); else ack(output);
}

function renderCounts() {
  const visible = visibleSessions(); const live = visible.filter(session => session.active).length;
  $('#running-count').textContent = String(live); bindText($('#total-count'), `${visible.length}개 세션`, `${visible.length} session${visible.length === 1 ? '' : 's'}`); $('#side-count').textContent = String(visible.length);
}

function renderSide() {
  const projects = projectGroups(importedProjects, [...sessions.values()]);
  if (selectedProject === 'unassigned' && !projects.some(project => project.key === 'unassigned')) projects.push({ key: 'unassigned', name: '', cwd: null, count: 0, favorite: false });
  $('#project-count').textContent = String(importedProjects.length);
  $('#projects').replaceChildren();
  for (const project of projects) {
    const { key, cwd, name, count } = project;
    const container = document.createElement('div'); container.className = 'project-item';
    const row = document.createElement('button'); row.className = `project-row${selectedProject === key ? ' active' : ''}`; row.setAttribute('aria-pressed', String(selectedProject === key));
    const label = document.createElement('span'); if (key === 'unassigned') bindText(label, '미분류', 'Unassigned'); else label.textContent = name;
    row.title = cwd ?? t('실행 폴더 없음', 'No working folder'); row.dataset.projectCwd = cwd ?? ''; row.dataset.projectKey = key;
    const total = document.createElement('small'); total.textContent = String(count); row.append(document.createTextNode(project.favorite ? '★ ' : '▱ '), label, total); row.onclick = () => selectProject(key); container.append(row);
    if (project.id) { const manage = document.createElement('button'); manage.className = 'project-manage icon-button'; manage.dataset.projectId = project.id; manage.textContent = '⋯'; bindAttribute(manage, 'aria-label', `${name} 프로젝트 관리`, `Manage project ${name}`); manage.onclick = () => projectManager.open(importedProjects.find(item => item.id === project.id)!); row.oncontextmenu = event => { event.preventDefault(); manage.click(); }; container.append(manage); }
    $('#projects').append(container);
  }
  if (!projects.length) { const p = document.createElement('p'); p.className = 'side-empty'; bindText(p, '프로젝트를 만들거나 폴더를 가져오세요', 'Create a project or import a folder'); $('#projects').append(p); }
  const activeProjects = new Set([...sessions.values()].filter(session => session.active).map(session => sessionProjectKey(session, importedProjects)));
  const tabs = $('#project-tabs'); const focused = tabs.contains(document.activeElement) ? (document.activeElement as HTMLElement).dataset.projectKey : undefined; tabs.replaceChildren();
  [{ key: '', name: '', cwd: null, count: sessions.size, favorite: false }, ...projects].forEach(({ key, cwd, name, count, favorite }, index) => {
    const tab = document.createElement('button'); const chosen = key === selectedProject;
    tab.id = `project-tab-${index}`; tab.className = `project-tab${chosen ? ' active' : ''}`; tab.dataset.projectCwd = cwd ?? ''; tab.dataset.projectKey = key; tab.setAttribute('role', 'tab'); tab.setAttribute('aria-selected', String(chosen)); tab.setAttribute('aria-controls', 'grid-area'); tab.tabIndex = chosen ? 0 : -1;
    if (cwd) tab.title = cwd; else bindAttribute(tab, 'title', key ? '논리적 세션 묶음 · 실행 폴더와 별개' : '모든 프로젝트의 세션', key ? 'Session group independent of working folders' : 'Sessions from all projects');
    const label = document.createElement('span'); if (key === 'unassigned') bindText(label, '미분류', 'Unassigned'); else if (key) label.textContent = name; else bindText(label, '전체', 'All');
    const hasActiveSession = key ? activeProjects.has(key) : activeProjects.size > 0;
    const light = document.createElement('span'); light.className = `project-activity${hasActiveSession ? ' is-active' : ''}`; light.setAttribute('role', 'img'); bindAttribute(light, 'aria-label', hasActiveSession ? '활성 세션 있음' : '활성 세션 없음', hasActiveSession ? 'Active sessions' : 'No active sessions'); bindAttribute(light, 'title', hasActiveSession ? '활성 세션 있음' : '활성 세션 없음', hasActiveSession ? 'Active sessions' : 'No active sessions');
    const total = document.createElement('small'); total.textContent = String(count); if (favorite) tab.append(document.createTextNode('★ ')); tab.append(label, document.createTextNode(' '), total, light); tab.onclick = () => { selectProject(key); $('#project-tabs [aria-selected="true"]').focus(); }; tabs.append(tab);
    if (chosen) $('#grid-area').setAttribute('aria-labelledby', tab.id);
  });
  if (focused !== undefined) [...tabs.querySelectorAll<HTMLElement>('[role="tab"]')].find(tab => tab.dataset.projectKey === focused)?.focus();
  $('#session-list').replaceChildren();
  visibleOrder().forEach((id, index) => { const session = sessions.get(id)!; const button = document.createElement('button'); button.className = `session-link${active === id ? ' active' : ''}`; button.dataset.sessionLink = id; const dot = document.createElement('span'); dot.className = `status-dot ${session.state}`; const label = document.createElement('span'); label.textContent = session.title; const shortcut = document.createElement('small'); shortcut.textContent = index < 9 ? `⌥${index + 1}` : ''; button.append(dot, label, shortcut); button.onclick = () => focusSession(id); $('#session-list').append(button); });
}

function renderGrid() {
  const grid = $('#grid');
  const visible = visibleOrder();
  const count = maximized ? 1 : Math.min(columns, Math.max(1, visible.length));
  const rows = Math.max(1, Math.ceil(visible.length / count));
  while (columnSizes.length < count) columnSizes.push(1);
  while (rowSizes.length < rows) rowSizes.push(1);
  grid.style.gridTemplateColumns = maximized ? 'minmax(0,1fr)' : columnSizes.slice(0, count).map(size => `minmax(0,${size}fr)`).join(' ');
  grid.style.gridTemplateRows = maximized ? 'minmax(0,1fr)' : rowSizes.slice(0, rows).map(size => `minmax(0,${size}fr)`).join(' ');
  order.forEach((id, index) => { const view = terminals.get(id)!; view.card.hidden = !isVisible(id) || (!!maximized && id !== maximized); refreshInputState(id); if (grid.children[index] !== view.card) grid.insertBefore(view.card, grid.children[index] ?? null); });
  $('#empty').hidden = visible.length > 0;
  bindText($('#empty h2'), selectedProject ? '이 프로젝트에 열린 세션이 없습니다' : '다음 작업을 펼쳐 보세요', selectedProject ? 'No open sessions in this project' : 'Open your next task');
  bindText($('#empty p'), selectedProject ? '새 세션을 만들거나 세션 불러오기에서 기존 대화를 이어가세요. 다른 프로젝트의 세션은 계속 실행됩니다.' : '프로젝트 경로를 선택해 첫 세션을 시작하세요. 각 세션은 독립된 터미널에서 실행됩니다.', selectedProject ? 'Create a session or load an existing conversation. Sessions in other projects keep running.' : 'Choose a project path to start your first session. Each session runs in its own terminal.');
  for (const button of document.querySelectorAll<HTMLElement>('[data-columns]')) { button.classList.toggle('active', Number(button.dataset.columns) === columns); button.setAttribute('aria-pressed', String(Number(button.dataset.columns) === columns)); }
  renderHandles(count, rows);
  updateConversationVisibility();
}

function updateConversationVisibility() {
  for (const [id, controller] of contextControls) {
    const view = terminals.get(id); const session = sessions.get(id);
    controller.setVisible(native && !!session?.active && !!view && !view.card.hidden && !document.hidden && nativeWindowVisible);
  }
  for (const [id, controller] of conversations) {
    const view = terminals.get(id);
    void controller.setForeground(!!view && !view.card.hidden && !document.hidden && nativeWindowVisible).catch(error => notify('대화 화면을 갱신하지 못했습니다. 해당 세션을 다시 선택하세요.', true, 'Could not refresh the conversation view. Select the session again.'));
  }
}
document.addEventListener('visibilitychange', updateConversationVisibility);

function renderHandles(count: number, rows: number) {
  const layer = $('#grid-handles'); layer.replaceChildren();
  if (maximized || !visibleOrder().length) return;
  for (const [axis, length, sizes] of [['column', count, columnSizes], ['row', rows, rowSizes]] as const) {
    const total = sizes.slice(0, length).reduce((a, b) => a + b, 0);
    let position = 0;
    for (let index = 0; index < length - 1; index++) {
      position += sizes[index]; const boundary = position; const handle = document.createElement('div'); handle.className = `resize-handle ${axis}`;
      handle.style[axis === 'column' ? 'left' : 'top'] = `${position / total * 100}%`;
      handle.setAttribute('role', 'separator'); handle.setAttribute('aria-orientation', axis === 'column' ? 'vertical' : 'horizontal'); bindAttribute(handle, 'aria-label', `${index + 1}번째 ${axis === 'column' ? '열' : '행'} 크기 조절`, `Resize ${axis} ${index + 1}`); handle.tabIndex = 0;
      const adjust = (delta: number, first: number, second: number) => { const sum = first + second; sizes[index] = Math.max(sum * 0.15, Math.min(sum * 0.85, first + delta)); sizes[index + 1] = sum - sizes[index]; const grid = $('#grid'); grid.style[axis === 'column' ? 'gridTemplateColumns' : 'gridTemplateRows'] = sizes.slice(0, length).map(size => `minmax(0,${size}fr)`).join(' '); };
      handle.onkeydown = event => { if (!['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(event.key)) return; event.preventDefault(); adjust((event.key === 'ArrowRight' || event.key === 'ArrowDown' ? 1 : -1) * total * 0.03, sizes[index], sizes[index + 1]); renderHandles(count, rows); scheduleSave(); };
      handle.onpointerdown = event => {
        event.preventDefault(); handle.setPointerCapture(event.pointerId); const start = axis === 'column' ? event.clientX : event.clientY; const first = sizes[index]; const second = sizes[index + 1]; const bounds = $('#grid-area').getBoundingClientRect(); const pixels = axis === 'column' ? bounds.width : bounds.height;
        handle.onpointermove = move => { const current = axis === 'column' ? move.clientX : move.clientY; adjust((current - start) / pixels * total, first, second); handle.style[axis === 'column' ? 'left' : 'top'] = `${(boundary + sizes[index] - first) / total * 100}%`; };
        handle.onpointerup = () => { handle.onpointermove = null; renderHandles(count, rows); scheduleSave(); };
        handle.onpointercancel = handle.onpointerup;
      };
      layer.append(handle);
    }
  }
}

function renderAll() {
  cronManager.updateSessions([...sessions.values()]);
  subagentManager.updateSessions([...sessions.values()]);
  for (const [id, session] of sessions) {
    const card = terminals.get(id)!.card;
    card.querySelector<HTMLButtonElement>('.session-id-copy')!.disabled = demo || !session.threadId;
    card.querySelector<HTMLButtonElement>('.cron-open')!.disabled = !session.active || exitInProgress;
    card.querySelector<HTMLButtonElement>('.subagents-open')!.disabled = !native || !session.active || !session.threadId || exitInProgress;
    card.querySelector('.terminal-title')!.textContent = session.title;
    card.querySelector('.terminal-path')!.textContent = `▱  ${session.cwd}`;
    const state = card.querySelector('.session-state')!;
    bindText(state, ...stateLabels(session.state));
    state.className = `session-state ${session.state}`;
    const identity = card.querySelector<HTMLElement>('.terminal-identity')!;
    const index = String(order.indexOf(id) + 1).padStart(2, '0'); bindText(identity, `${index}  ·  ${demo ? 'DEMO · 실제 세션 아님' : session.threadId ? `스레드 ${session.threadId.slice(0, 12)}` : '로컬 세션'}`, `${index}  ·  ${demo ? 'DEMO · example session' : session.threadId ? `Thread ${session.threadId.slice(0, 12)}` : 'Local session'}`);
    identity.title = session.threadId ?? '';
    card.classList.toggle('focused', id === active);
  }
  renderCounts(); renderSide(); renderGrid();
}

let createProjectId: string | null | undefined;
function openCreate(cwd = selectedProjectCwd()) { createProjectId = selectedProjectId() ?? (selectedProject === 'unassigned' ? null : undefined); $('#create-error').hidden = true; $<HTMLInputElement>('#session-cwd').value = cwd ? projectPath(cwd) : ''; $<HTMLInputElement>('#session-thread').value = ''; for (const field of createFields) validateRequired(field); $<HTMLDialogElement>('#create-dialog').showModal(); $<HTMLInputElement>('#session-title').focus(); }
const createFields = [$<HTMLInputElement>('#session-title'), $<HTMLInputElement>('#session-cwd')];
function validateRequired(field: HTMLInputElement) { field.setCustomValidity(field.value.trim() ? '' : t('이 항목을 입력하세요.', 'Please fill out this field.')); }
for (const field of createFields) { field.addEventListener('invalid', () => validateRequired(field)); field.addEventListener('input', () => validateRequired(field)); }
const sessionBrowser = createSessionBrowser({
  call,
  projectsChanged(projects) { importedProjects = projects; if (selectedProject.startsWith('id:') && !selectedProjectId()) selectProject(''); else renderAll(); },
  projectImported(project) { selectProject(`id:${project.id}`); },
  async openSession(session) {
    const opening = start({ title: resumeTitle(session.title), cwd: session.cwd, threadId: session.id, ...(selectedProjectId() ? { projectId: selectedProjectId(), projectAssigned: true } : selectedProject === 'unassigned' ? { projectId: null, projectAssigned: true } : {}) });
    const selectionVersion = projectSelectionVersion; const info = await opening;
    // The dialog restores focus when closing; refocus only the original tab intent.
    return () => { if (selectionVersion === projectSelectionVersion && isVisible(info.id)) focusSession(info.id); };
  },
  newSession: openCreate,
  notify,
});
const cronManager = createCronManager({ call, notify });
const subagentManager = createSubagentManager({ call, notify });
const projectManager = createProjectManager({
  call, projects: () => importedProjects,
  async changed(project, deletedId) {
    if (deletedId) {
      const belongs = (session: SavedSession) => session.projectId === deletedId || (!session.projectAssigned && sessionProjectKey(session, importedProjects) === `id:${deletedId}`);
      const unassign = <T extends SavedSession>(session: T): T => ({ ...session, projectId: null, projectAssigned: true, projectRevision: (session.projectRevision ?? 0) + 1 });
      for (const session of saved) if (belongs(session)) Object.assign(session, unassign(session));
      for (const session of sessions.values()) if (belongs(session)) receiveSession(unassign(session));
    }
    await sessionBrowser.refreshProjects();
    if (project) selectProject(`id:${project.id}`);
    scheduleSave();
  },
  moved(info) { receiveSession(info); selectProject(sessionProjectKey(info, importedProjects)); focusSession(info.id); },
});
$('#project-create').onclick = () => projectManager.open();
for (const id of ['new-session', 'side-add', 'empty-add']) $(`#${id}`).onclick = () => openCreate();
$('#catalog-open').onclick = () => void sessionBrowser.open(selectedProjectCwd());
$('#project-import').onclick = () => void sessionBrowser.importProject();
$('#project-tabs').addEventListener('keydown', event => {
  if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
  const tabs = [...$('#project-tabs').querySelectorAll<HTMLElement>('[role="tab"]')];
  const index = tabs.indexOf(event.target as HTMLElement); if (index < 0) return;
  event.preventDefault();
  const next = event.key === 'Home' ? 0 : event.key === 'End' ? tabs.length - 1 : (index + (event.key === 'ArrowRight' ? 1 : tabs.length - 1)) % tabs.length;
  selectProject(tabs[next].dataset.projectKey ?? ''); $('#project-tabs [aria-selected="true"]').focus();
});
const extensionsManager = createExtensionsManager({ call, notify });
const mobileControl = createMobileControl({ call, notify, openExecutableSettings: () => void executableSettings.open() });
const weeklyUsage = createWeeklyUsage({ call, initiallyVisible: !native });
const questionCenter = createQuestionCenter({ call, button: $<HTMLButtonElement>('#notifications'), openSession: focusSession });
document.body.append(questionCenter.element);
$('.sidebar').append(weeklyUsage.element);
bindText($('#extensions-open-label'), '플러그인·스킬', 'Plugins & skills');
$('#extensions-open').onclick = () => extensionsManager.open(selectedProjectCwd());
const executableSettings = createExecutableSettings(async <T>(command: string, args?: Record<string, unknown>) => {
  if (!['choose_codex_executable', 'set_codex_executable'].includes(command)) return call<T>(command, args);
  runtimeGeneration++; runtimePreparation = undefined; runtimeReady = false;
  weeklyUsage.setVisible(false); weeklyUsage.refresh();
  const mutation = call<T>(command, args); executableMutation = mutation;
  try { return await mutation; }
  finally {
    if (executableMutation === mutation) {
      executableMutation = undefined;
      if (workspaceReady && !exitInProgress) void prepareRuntime().then(() => resumeWorkspace()).catch(error => notifyError('공유서버 준비 실패', 'Could not prepare the shared server', error));
    }
  }
});
$('#codex-settings-open').onclick = () => void executableSettings.open();
$('#settings-open').onclick = () => $<HTMLDialogElement>('#settings-dialog').showModal();
for (const button of document.querySelectorAll<HTMLElement>('[data-close]')) button.onclick = () => button.closest('dialog')?.close();
$('#column-options').onclick = event => { const button = (event.target as HTMLElement).closest<HTMLElement>('[data-columns]'); if (button) { columns = Number(button.dataset.columns); maximized = ''; renderGrid(); scheduleSave(); } };

async function start(request: SavedSession, focus = true) {
  if (exitInProgress) throw new UiError('앱 종료 중에는 세션을 시작할 수 없습니다.', 'Cannot start a session while the app is exiting.');
  if (!workspaceReady) throw new UiError('저장된 작업 공간을 확인하고 있습니다. 잠시 후 다시 시도하세요.', 'The saved workspace is still loading. Try again shortly.');
  for (const info of sessions.values()) if (info.threadId === request.threadId) exitResumeIntents.delete(info.id);
  // Select at the user's intent, never when a delayed native start completes.
  const requestedProject = sessionProjectKey(request, importedProjects);
  if (focus && selectedProject && selectedProject !== requestedProject) selectProject(requestedProject);
  const selectionVersion = projectSelectionVersion;
  if (request.threadId) {
    const pending = pendingThreads.get(request.threadId);
    if (pending) return pending;
  }
  let reserved = false;
  // Register the shared promise before its first ownership query can start.
  const operation = Promise.resolve().then(async () => {
    try {
      await prepareRuntime();
      if (exitInProgress) throw new UiError('앱 종료 중에는 세션을 시작할 수 없습니다.', 'Cannot start a session while the app is exiting.');
      const existing = request.threadId && [...sessions.values()].find(item => item.threadId === request.threadId && item.active);
      if (existing) {
        let latest: SessionInfo[];
        try { latest = await call<SessionInfo[]>('list_sessions'); }
        catch (error) { throw new UiError(`실행 중 세션 확인 실패: ${message(error, 'ko')}`, `Could not check running sessions: ${message(error, 'en')}`); }
        for (const info of latest) if (sessions.has(info.id)) receiveSession(info);
        const owned = latest.find(info => info.threadId === request.threadId && info.active);
        if (owned) {
          receiveSession(owned);
          if (sessions.get(owned.id)?.active) { if (focus && selectionVersion === projectSelectionVersion) focusSession(owned.id); return owned; }
        }
      }
      // Reserve capacity only for a new native process, after reconciling any existing card.
      if (sessions.size + pendingStarts >= MAX_WORKSPACE_SESSIONS) throw new UiError(`작업 공간에는 시작 중인 세션을 포함해 최대 ${MAX_WORKSPACE_SESSIONS}개를 표시할 수 있습니다. 완료된 세션을 닫은 뒤 다시 시작하세요.`, `A workspace can display at most ${MAX_WORKSPACE_SESSIONS} sessions, including sessions that are starting. Close a finished session and try again.`);
      if (sessions.size + saved.length - pendingRestoreOwnerIds().size + pendingStarts - Number(saved.includes(request)) >= MAX_SAVED_WORKSPACE_SESSIONS) throw new UiError('저장 메타데이터는 최대 512개입니다. 저장 목록에서 불필요한 항목을 제거한 뒤 다시 시도하세요.', 'Workspace metadata is limited to 512 entries. Remove unused saved entries and retry.');
      pendingStarts++; reserved = true;
      try {
        // Ownership refreshes above may have yielded while the executable changed.
        await prepareRuntime();
        if (exitInProgress) throw new UiError('앱 종료 중에는 세션을 시작할 수 없습니다.', 'Cannot start a session while the app is exiting.');
        const info = await call<SessionInfo>('start_session', { request: { title: request.title, cwd: request.cwd, ...(request.threadId ? { threadId: request.threadId } : {}), ...(request.projectAssigned || request.projectId !== undefined ? { projectId: request.projectId ?? null, projectAssigned: true } : {}), cols: 100, rows: 30 } });
        if (!info.active || (sessions.get(info.id)?.generation ?? -1) < info.generation) receiveSession(info);
        if (focus && selectionVersion === projectSelectionVersion) focusSession(info.id);
        queueResize(info.id);
        return info;
      } catch (error) {
        if (!native) throw error;
        try {
          // The cleanup event can arrive after the rejected IPC. Reconcile owned cards before allowing retry.
          const latest = await call<SessionInfo[]>('list_sessions');
          for (const info of latest) if (sessions.has(info.id)) receiveSession(info);
        } catch (refreshError) { throw new UiError(`${message(error, 'ko')} · 세션 상태 확인 실패: ${message(refreshError, 'ko')}`, `${message(error, 'en')} · Could not check session state: ${message(refreshError, 'en')}`); }
        throw error;
      }
    } finally { if (reserved) pendingStarts--; if (request.threadId) pendingThreads.delete(request.threadId); }
  });
  if (request.threadId) pendingThreads.set(request.threadId, operation);
  startOperations.add(operation);
  void operation.finally(() => startOperations.delete(operation)).catch(() => {});
  return operation;
}
$('#create-form').onsubmit = async event => {
  event.preventDefault(); const button = $<HTMLButtonElement>('#create-submit'); button.disabled = true;
  const threadId = $<HTMLInputElement>('#session-thread').value.trim();
  try { await start({ title: $<HTMLInputElement>('#session-title').value.trim(), cwd: $<HTMLInputElement>('#session-cwd').value.trim(), ...(threadId ? { threadId } : {}), ...(createProjectId !== undefined ? { projectId: createProjectId, projectAssigned: true } : {}) }); $<HTMLDialogElement>('#create-dialog').close(); $<HTMLFormElement>('#create-form').reset(); }
  catch (error) { $('#create-error').hidden = false; showError($('#create-error'), error); } finally { button.disabled = false; }
};
function renderRestoreList() {
  const list = $('#restore-list'); list.replaceChildren();
  for (const session of saved) {
    const row = document.createElement('div'); row.className = 'restore-item';
    const title = document.createElement('strong'); title.textContent = session.title;
    const path = document.createElement('small'); path.textContent = session.cwd;
    const thread = document.createElement('small'); bindText(thread, session.threadId ? `스레드 ${session.threadId}` : '스레드 ID 없음 · 새 대화는 선택할 때만 시작', session.threadId ? `Thread ${session.threadId}` : 'No thread ID · a new conversation starts only if selected');
    const button = document.createElement('button'); button.className = 'quiet-button';
    bindText(button, session.threadId ? '이 대화 재개 →' : '새 대화 시작 →', session.threadId ? 'Resume conversation →' : 'Start new conversation →');
    button.disabled = restoringRequests.has(session) || exitInProgress;
    button.onclick = async () => { button.disabled = true; try { await restoreSaved(session); } catch (error) { notifyError('복원 실패', 'Could not restore session', error); } };
    const remove = document.createElement('button'); remove.className = 'quiet-button'; bindText(remove, '목록에서 제거', 'Remove from list'); remove.disabled = button.disabled;
    remove.onclick = () => { saved = saved.filter(item => item !== session); workspaceSlots = workspaceSlots.filter(slot => slot !== session); updateRestoreBanner(); scheduleSave(); };
    row.append(title, path, thread, button, remove); list.append(row);
  }
}
$('#restore-open').onclick = () => { renderRestoreList(); $<HTMLDialogElement>('#restore-dialog').showModal(); };

async function refreshExternal() {
  const list = $('#external-list'); listStatus(list, '관측 기록을 불러오는 중…', 'Loading observed records…');
  try { const external = await call<{ id: string; title: string; cwd: string; status: string; observedAt: string }[]>('list_external'); list.replaceChildren();
    for (const session of external) { const item = document.createElement('div'); item.className = 'external-item'; const title = document.createElement('strong'); title.textContent = session.title; const cwd = document.createElement('small'); if (session.cwd) cwd.textContent = session.cwd; else bindText(cwd, '경로 미확인', 'Unknown path'); const state = document.createElement('small'); bindText(state, `현재 상태 미확인 · 관측 ${session.observedAt || '시각 미확인'}`, `Current state unknown · observed ${session.observedAt || 'at an unknown time'}`); item.append(title, cwd, state); list.append(item); }
    if (!external.length) listStatus(list, '발견된 외부 기록이 없습니다.', 'No external records found.');
  } catch (error) { listStatus(list, `불러오기 실패: ${message(error, 'ko')}`, `Could not load records: ${message(error, 'en')}`); }
}
$('#external-toggle').onclick = () => { $<HTMLDialogElement>('#external-dialog').showModal(); void refreshExternal(); };
$('#external-refresh').onclick = () => void refreshExternal();
const exitDialog = $<HTMLDialogElement>('#exit-dialog');
const unconfirmedExitButton = $<HTMLButtonElement>('#confirm-unconfirmed-exit');
function resetExitRecovery() { unconfirmedExitButton.hidden = true; $('#unconfirmed-exit-warning').hidden = true; }
function openExitDialog() { if (!exitDialog.open) { resetExitRecovery(); exitDialog.showModal(); } }
$('#exit').onclick = openExitDialog;
exitDialog.onclose = resetExitRecovery;
exitDialog.oncancel = event => { if ($<HTMLButtonElement>('#confirm-exit').disabled) event.preventDefault(); };
async function requestExit(allowUnconfirmedRemote = false) {
  const button = $<HTMLButtonElement>('#confirm-exit'); if (button.disabled || (allowUnconfirmedRemote && unconfirmedExitButton.hidden)) return;
  resetExitRecovery(); exitInProgress = true; clearTimeout(saveTimer); updateRestoreBanner();
  const extensionsButton = $<HTMLButtonElement>('#extensions-open');
  const dismissButtons = [...exitDialog.querySelectorAll<HTMLButtonElement>('[data-close]')];
  const dismissStates = dismissButtons.map(control => control.disabled);
  const languageWasDisabled = languageSelect.disabled; const extensionsWasDisabled = extensionsButton.disabled; let completed = false;
  const runtimeWasReady = runtimeReady;
  button.disabled = true; unconfirmedExitButton.disabled = true; extensionsButton.disabled = true; for (const control of dismissButtons) control.disabled = true; if (native) languageSelect.disabled = true;
  try {
    bindText(button, '진행 중 작업 완료를 기다리는 중…', 'Waiting for work to finish…');
    // Raw preparation owns the native operation even when no restore is pending.
    // A rejection settles ownership; prepareRuntime retains its distinct failure state.
    const preparing = runtimeCommand && runtimeCommandGeneration === runtimeGeneration ? runtimePreparation : undefined;
    await Promise.all([extensionsManager.beginExit(), mobileControl.beginExit(), weeklyUsage.beginExit(), questionCenter.beginExit(), cronManager.beginExit(), subagentManager.beginExit(), initialization, runtimeRestoration?.catch(() => {}), Promise.allSettled([runtimeCommand, preparing])]);
    await Promise.allSettled([...startOperations, ...restoringRequests.values()]);
    if (native && !workspaceReady) throw new UiError('저장된 작업 공간을 확인하지 못해 종료 정보를 덮어쓰지 않았습니다.', 'Could not load the saved workspace; exit metadata was not overwritten.');
    bindText(button, '종료 중…', 'Exiting…');
    for (const session of sessions.values()) if (session.active && session.state !== 'stopping' && !explicitlyStoppedSessions.has(session.id)) exitResumeIntents.add(session.id);
    await persistLayout();
    if (native) await nativeLanguageQueue;
    await call('complete_exit', { confirmed: true, ...(allowUnconfirmedRemote ? { allowUnconfirmedRemote: true } : {}) }); completed = true;
  } catch (error) {
    extensionsManager.cancelExit(); mobileControl.cancelExit(); weeklyUsage.setVisible(false); weeklyUsage.cancelExit(); questionCenter.cancelExit(); cronManager.cancelExit(); subagentManager.cancelExit();
    if (native) {
      try { if (await call<boolean>('unconfirmed_exit_available') === true) { unconfirmedExitButton.hidden = false; $('#unconfirmed-exit-warning').hidden = false; } }
      catch { /* 복구 선택은 숨긴 채 원래 종료 오류를 보존한다. */ }
    }
    notifyError('앱 종료 실패', 'Could not exit app', error);
  }
  finally {
    button.disabled = false; unconfirmedExitButton.disabled = false; dismissButtons.forEach((control, index) => { control.disabled = dismissStates[index]; }); bindText(button, '세션 종료 후 앱 닫기', 'Stop sessions and exit');
    if (!completed) {
      exitInProgress = false; updateRestoreBanner(); extensionsButton.disabled = extensionsWasDisabled; if (native) languageSelect.disabled = languageWasDisabled;
      // Skipped preparation clears its cache; an actual failure retains a rejected promise.
      const resumePreparation = !runtimeWasReady && workspaceReady && (runtimeReady || !runtimePreparation);
      if (resumePreparation) weeklyUsage.refresh();
      weeklyUsage.setVisible(nativeWindowVisible && runtimeReady);
      if (resumePreparation) void resumeWorkspace().catch(error => notifyError('공유서버 준비 실패', 'Could not prepare the shared server', error));
    }
  }
}
$('#confirm-exit').onclick = () => void requestExit();
unconfirmedExitButton.onclick = () => void requestExit(true);

function toggleSearch(force?: boolean) { searchVisible = force ?? !searchVisible; $('#search-bar').hidden = !searchVisible; if (searchVisible) $<HTMLInputElement>('#terminal-search').focus(); }
function find(previous = false) { const value = $<HTMLInputElement>('#terminal-search').value; const search = terminals.get(active)?.search; const found = !!value && search && (previous ? search.findPrevious(value) : search.findNext(value)); bindText($('#search-result'), !value ? '' : found ? '결과 찾음' : '결과 없음', !value ? '' : found ? 'Match found' : 'No matches'); }
$('#search-toggle').onclick = () => toggleSearch(); $('#search-close').onclick = () => toggleSearch(false); $('#search-prev').onclick = () => find(true); $('#search-next').onclick = () => find(); $('#terminal-search').oninput = () => find(); $('#terminal-search').onkeydown = event => { if (event.key === 'Enter') { event.preventDefault(); find(event.shiftKey); } if (event.key === 'Escape') toggleSearch(false); };
document.addEventListener('keydown', event => {
  if (document.querySelector('dialog[open]')) return;
  let id: string | undefined;
  const visible = visibleOrder();
  if (event.altKey && /^[1-9]$/.test(event.key)) id = visible[Number(event.key) - 1];
  if (event.ctrlKey && event.key === 'Tab' && visible.length) id = visible[(visible.indexOf(active) + (event.shiftKey ? visible.length - 1 : 1)) % visible.length];
  if (id) { event.preventDefault(); event.stopPropagation(); if (maximized) { maximized = id; renderGrid(); } setActive(id); }
  if (event.ctrlKey && event.shiftKey && event.key.toLowerCase() === 'f') { event.preventDefault(); event.stopPropagation(); toggleSearch(true); }
  if (event.key === 'Escape' && maximized) { event.preventDefault(); event.stopPropagation(); maximized = ''; renderGrid(); }
}, true);

async function initialize() {
  if (!native) {
    bindText($('#mode-label'), demo ? '디자인 데모' : '브라우저 미리보기', demo ? 'DESIGN DEMO' : 'BROWSER PREVIEW'); $('#runtime-banner').hidden = false;
    bindText($('#runtime-banner'), demo ? '디자인 데모 · 아래 터미널은 정적인 예시입니다. 실제 프로세스 실행·입력·전송은 지원하지 않습니다.' : '브라우저 미리보기입니다. 실제 세션 실행은 OpenDIY Desktop 앱에서 사용할 수 있습니다.', demo ? 'Design demo · These terminals are static examples. Process execution, input and sending are unavailable.' : 'Browser preview. Run real sessions in the OpenDIY Desktop app.');
    bindText($('#save-status'), '미리보기 · 저장 안 함', 'Preview · not saved');
    if (demo) seedDemo(); renderAll(); $('#app').setAttribute('aria-busy', 'false'); return;
  }
  try {
    await initializeNativeLanguage();
    unlisteners.push(await listen<Output>('terminal-output', event => receiveOutput(event.payload)));
    unlisteners.push(await listen<SessionInfo>('session-changed', event => receiveSession(event.payload)));
    unlisteners.push(await listen<string>('application-error', event => notify(event.payload, true)));
    unlisteners.push(await listen<boolean>('application-visibility', event => { nativeWindowVisible = event.payload; weeklyUsage.setVisible(event.payload && runtimeReady); questionCenter.setVisible(event.payload); cronManager.setVisible(event.payload); subagentManager.setVisible(event.payload); updateConversationVisibility(); }));
    unlisteners.push(await listen('exit-requested', openExitDialog));
    const [layoutValue, existing] = await Promise.all([call<unknown>('load_layout'), call<SessionInfo[]>('list_sessions')]);
    const wrapper = layoutValue as { layout?: unknown; sessions?: SessionInfo[] } | null;
    let layout: Layout | undefined;
    try { layout = parseLayout(wrapper && 'layout' in wrapper ? wrapper.layout : layoutValue); }
    catch (error) { if (error instanceof RangeError) throw new UiError('저장된 작업 공간이 512개 메타데이터 한도를 초과했습니다. 원본 저장을 덮어쓰지 않았습니다.', 'The saved workspace exceeds the 512-entry metadata limit. Existing storage was not overwritten.'); throw error; }
    if (layout) { columns = layout.columns; columnSizes = layout.columnSizes; rowSizes = layout.rowSizes; }
    for (const session of existing) if (session.state !== 'restored' && (sessions.get(session.id)?.generation ?? -1) < session.generation) receiveSession(session, false);
    saved = chooseSavedSessions(layout, existing); workspaceSlots = [...saved]; workspaceReady = true;
    for (const request of [...saved]) {
      const owned = existing.find(info => info.state !== 'restored' && (info.id === request.sessionId || (request.threadId && info.threadId === request.threadId)));
      if (owned) bindRestored(request, sessions.get(owned.id) ?? owned);
    }
    updateRestoreBanner();
    renderAll();
    void sessionBrowser.refreshProjects().catch(error => notifyError('프로젝트 목록 조회 실패', 'Could not load projects', error));
    await resumeWorkspace();
  } catch (error) { notifyError('초기화 실패', 'Could not initialize', error); }
  finally { $('#app').setAttribute('aria-busy', 'false'); }
}

function seedDemo() {
  const examples = [
    { title: 'API 구조 개선', cwd: 'C:\\workspace\\orbit-api', state: 'running', lines: ['\x1b[38;5;141m╭─ agent · local workspace\x1b[0m', '', '\x1b[90m›\x1b[0m API 모듈의 의존성을 정리해 주세요.', '', '\x1b[38;5;110m●\x1b[0m 현재 라우트와 서비스 경계를 확인하고 있습니다.', '', '  \x1b[90mRead\x1b[0m  src/routes/projects.ts', '  \x1b[90mRead\x1b[0m  src/services/project-service.ts', '  \x1b[90mRead\x1b[0m  src/db/connection.ts', '', '\x1b[38;5;114m✓\x1b[0m 12개 모듈의 호출 관계 확인', '\x1b[38;5;110m◌\x1b[0m 중복된 검증 로직을 정리하는 중…'] },
    { title: '대시보드 UI', cwd: 'C:\\workspace\\orbit-web', state: 'idle', lines: ['\x1b[38;5;141m╭─ agent · local workspace\x1b[0m', '', '\x1b[90m›\x1b[0m 대시보드 카드 레이아웃을 개선해 주세요.', '', '\x1b[38;5;114m●\x1b[0m 카드 간격과 작은 화면에서의 배치를 조정했습니다.', '', '  \x1b[38;5;114m+\x1b[0m src/components/Overview.tsx', '  \x1b[38;5;114m+\x1b[0m src/styles/dashboard.css', '', '\x1b[90m  다음 작업을 입력할 수 있습니다.\x1b[0m', '', '\x1b[38;5;141m›\x1b[0m '] },
    { title: '회귀 테스트', cwd: 'C:\\workspace\\orbit-api', state: 'running', lines: ['\x1b[38;5;141m╭─ agent · local workspace\x1b[0m', '', '\x1b[90m›\x1b[0m 변경된 API의 회귀 테스트를 확인해 주세요.', '', '\x1b[38;5;110m●\x1b[0m 테스트 시나리오를 검토하고 있습니다.', '', '  \x1b[90mInspect\x1b[0m  tests/project.test.ts', '  \x1b[90mInspect\x1b[0m  tests/auth.test.ts', '', '\x1b[38;5;114m✓\x1b[0m 권한 경계와 잘못된 입력 사례 확인', '\x1b[38;5;110m◌\x1b[0m 타임아웃 경로 분석 중…'] },
    { title: '릴리스 노트', cwd: 'C:\\workspace\\orbit-docs', state: 'waiting', lines: ['\x1b[38;5;141m╭─ agent · local workspace\x1b[0m', '', '\x1b[90m›\x1b[0m 이번 변경의 릴리스 노트를 준비해 주세요.', '', '\x1b[38;5;179m●\x1b[0m 확인이 필요합니다.', '', '  공개 문서에 포함할 변경 범위를 선택해 주세요.', '', '  \x1b[38;5;179m1.\x1b[0m 사용자에게 보이는 변경만 포함', '  \x1b[90m2.\x1b[0m 개발자 변경 내역도 포함', '', '\x1b[90m  응답을 기다리고 있습니다…\x1b[0m'] },
  ];
  examples.forEach((example, index) => { const id = `demo-${index}`; receiveSession({ ...example, id, generation: 1, active: true }, false); terminals.get(id)!.terminal.write(example.lines.join('\r\n')); });
  setActive('demo-0', false);
}

window.addEventListener('beforeunload', () => { exitInProgress = true; for (const unlisten of unlisteners) unlisten(); for (const view of terminals.values()) { clearTimeout(view.resizeTimer); view.input.dispose(); view.resize.dispose(); view.observer.disconnect(); view.terminal.dispose(); } });
initialization = initialize();

window.addEventListener('beforeunload', () => questionCenter.dispose());
