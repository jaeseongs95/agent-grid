import { invoke, isTauri } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { SearchAddon } from '@xterm/addon-search';
import '@xterm/xterm/css/xterm.css';
import './style.css';
import { MAX_WORKSPACE_SESSIONS, acceptOutput, broadcast, broadcastTargets, chooseSavedSessions, parseLayout, updateSession, writable, type Layout, type Output, type SavedSession, type Session, type SessionInfo } from './model';
import { createSessionBrowser, type ProjectInfo } from './session-browser';
import { createExecutableSettings } from './executable-settings';

const native = isTauri();
const demo = !native && new URLSearchParams(location.search).has('demo');
const $ = <T extends HTMLElement = HTMLElement>(selector: string) => document.querySelector<T>(selector)!;
const states: Record<string, string> = { restored: '저장됨 · 미실행', notLoaded: '상태 미확인', starting: '시작 중', running: '실행 중', idle: '입력 가능', waiting: '응답 대기', stopping: '종료 중', stopped: '종료됨', exited: '종료됨', error: '오류', unknown: '상태 미확인' };
const sessions = new Map<string, Session>();
const selected = new Set<string>();
const terminals = new Map<string, { terminal: Terminal; fit: FitAddon; search: SearchAddon; card: HTMLElement; observer: ResizeObserver; input: Promise<void>; resize: Promise<void>; resizeTimer?: ReturnType<typeof setTimeout> }>();
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
let saveTimer: ReturnType<typeof setTimeout>;
let saveQueue = Promise.resolve();
let notificationCount = 0;
let dragging = '';
let searchVisible = false;
let pendingStarts = 0;
const pendingThreads = new Map<string, Promise<SessionInfo>>();
let importedProjects: ProjectInfo[] = [];

$('#app').innerHTML = `
  <aside class="sidebar">
    <a class="brand" href="#" aria-label="Agent Grid 홈"><span class="brand-mark"><i></i><i></i><i></i><i></i></span>Agent Grid<span class="version">0.1</span></a>
    <div class="workspace-label">WORKSPACE <span class="local-pill">LOCAL</span></div>
    <button class="workspace-choice"><span class="folder-icon">▤</span><span>나의 작업 공간<small>독립된 터미널, 하나의 화면</small></span></button>
    <div class="side-heading">프로젝트 <span id="project-count">0</span></div>
    <button id="project-import" class="project-import quiet-button">+ 프로젝트 가져오기</button>
    <div id="projects" class="projects"></div>
    <div class="side-heading sessions-heading">세션 <span id="side-count">0</span><button id="side-add" class="icon-button" aria-label="새 세션 만들기">+</button></div>
    <nav id="session-list" aria-label="세션 목록"></nav>
    <button id="external-toggle" class="side-link"><span>◉</span> 외부 세션 <span class="readonly">읽기 전용</span></button>
    <button id="codex-settings-open" class="side-link"><span>⚙</span> 실행 파일 설정</button>
    <div class="sidebar-bottom"><span class="connection-dot"></span><span id="connection">연결 확인 중</span><small>Windows · 로컬 실행</small></div>
  </aside>
  <main>
    <header class="topbar"><div class="breadcrumb">작업 공간 <span>/</span> <strong>세션 그리드</strong></div><div class="top-actions"><span id="mode-label" class="mode-label">LOCAL WORKSPACE</span><button id="notifications" class="icon-button" aria-label="알림 보기">♧<span id="notification-count" hidden></span></button><button id="exit" class="quiet-button">앱 종료</button></div></header>
    <section class="workspace-header"><div><div class="eyebrow">YOUR AGENTS, IN VIEW</div><h1>세션 그리드<span class="heading-dot">.</span></h1><p>흐름을 놓치지 않고, 여러 작업을 함께.</p></div><div class="workspace-actions"><button id="catalog-open" class="quiet-button">세션 불러오기</button><button id="new-session" class="primary-button"><span>+</span> 새 세션</button></div></section>
    <section class="toolbar" aria-label="그리드 도구"><div class="session-summary"><span class="connection-dot"></span><strong id="running-count">0</strong> 실행 중 <span class="divider"></span><span id="total-count">0개 세션</span></div><div class="layout-tools"><button id="search-toggle" class="quiet-button" title="활성 터미널 검색 (Ctrl+Shift+F)">⌕ <span>검색</span></button><span class="divider"></span><span class="layout-label">열</span><div id="column-options" class="segmented" role="group" aria-label="그리드 열 수"><button data-columns="1" aria-label="1열">▯</button><button data-columns="2" class="active" aria-label="2열">▥</button><button data-columns="3" aria-label="3열">▥▯</button><button data-columns="4" aria-label="4열">▥▥</button></div></div></section>
    <div id="runtime-banner" class="runtime-banner" hidden></div>
    <div id="restore-banner" class="restore-banner" hidden><span>저장된 작업 공간이 있습니다. 프로세스는 자동으로 시작하지 않습니다.</span><button id="restore-open" class="quiet-button">저장 세션 보기 →</button></div>
    <section id="search-bar" class="search-bar" hidden><label for="terminal-search">활성 터미널 검색</label><input id="terminal-search" placeholder="검색어 입력"/><button id="search-prev" class="quiet-button" aria-label="이전 검색 결과">↑</button><button id="search-next" class="quiet-button" aria-label="다음 검색 결과">↓</button><span id="search-result" role="status"></span><button id="search-close" class="icon-button" aria-label="검색 닫기">×</button></section>
    <section id="grid-area" class="grid-area"><div id="grid" class="terminal-grid"></div><div id="grid-handles"></div><div id="empty" class="empty"><div class="empty-grid">▦</div><h2>다음 작업을 펼쳐 보세요</h2><p>프로젝트 경로를 선택해 첫 세션을 시작하세요.<br>각 세션은 독립된 터미널에서 실행됩니다.</p><button id="empty-add" class="primary-button">+ 첫 세션 만들기</button><small>Alt + 1–9 세션 전환 · Ctrl + Tab 다음 세션</small></div></section>
    <section class="broadcast-panel"><div class="broadcast-top"><div><strong>선택 전송</strong><span id="selected-count">선택한 세션이 없습니다</span></div><button id="clear-selection" class="text-button">선택 해제</button></div><form id="broadcast-form"><span class="broadcast-symbol">↗</span><input id="broadcast-input" aria-label="선택한 세션에 보낼 메시지" placeholder="체크한 세션에 같은 메시지를 보냅니다. 대기·미확인 세션은 제외됩니다." autocomplete="off"/><button id="broadcast-send" type="submit" class="send-button" disabled>전송 <span>↵</span></button></form><div id="broadcast-results" class="broadcast-results" role="status" hidden></div></section>
    <footer><span><span class="connection-dot"></span> <span id="footer-state">로컬 세션만 제어합니다</span></span><span>드래그로 순서 변경 <i>·</i> 경계선으로 크기 조절 <i>·</i> Alt + 1–9</span><span id="save-status">레이아웃 자동 저장</span></footer>
  </main>
  <div id="toast-area" class="toast-area" role="status" aria-live="polite"></div>
  <dialog id="create-dialog"><form id="create-form"><div class="dialog-heading"><span class="eyebrow">NEW SESSION</span><button type="button" class="icon-button" data-close aria-label="닫기">×</button></div><h2>새 세션 만들기</h2><p>선택한 프로젝트에서 로컬 에이전트를 시작합니다.</p><label for="session-title">세션 이름</label><input id="session-title" name="title" placeholder="예: API 개선" required maxlength="120"/><label for="session-cwd">프로젝트 경로</label><input id="session-cwd" name="cwd" placeholder="C:\\work\\my-project" required/><small>실제로 존재하는 로컬 디렉터리의 절대 경로를 입력하세요.</small><label for="session-thread">이어갈 스레드 ID <span class="optional">선택</span></label><input id="session-thread" name="thread" placeholder="비워 두면 새 대화로 시작합니다"/><div id="create-error" class="inline-error" role="alert" hidden></div><div class="dialog-actions"><button type="button" class="quiet-button" data-close>취소</button><button id="create-submit" class="primary-button" type="submit">세션 시작 →</button></div></form></dialog>
  <dialog id="restore-dialog"><div class="dialog-heading"><span class="eyebrow">SAVED WORKSPACE</span><button class="icon-button" data-close aria-label="닫기">×</button></div><h2>저장된 세션</h2><p>버튼을 누른 세션만 시작합니다. 스레드 ID가 있으면 해당 대화를 재개합니다.</p><div id="restore-list" class="restore-list"></div></dialog>
  <dialog id="external-dialog"><div class="dialog-heading"><span class="eyebrow">READ ONLY</span><button class="icon-button" data-close aria-label="닫기">×</button></div><h2>외부 세션</h2><p>다른 앱에서 발견한 기록입니다. 관측 정보이며 현재 실행 상태는 미확인입니다. 입력하거나 종료할 수 없습니다.</p><button id="external-refresh" class="quiet-button">새로 고침</button><div id="external-list" class="external-list"></div></dialog>
  <dialog id="notification-dialog"><div class="dialog-heading"><span class="eyebrow">ACTIVITY</span><button class="icon-button" data-close aria-label="닫기">×</button></div><h2>알림</h2><div id="notification-list" class="notification-list"><p class="muted">새로운 알림이 없습니다.</p></div></dialog>
  <dialog id="exit-dialog"><div class="dialog-heading"><span class="eyebrow">EXIT WORKSPACE</span><button class="icon-button" data-close aria-label="닫기">×</button></div><h2>Agent Grid를 종료할까요?</h2><p>앱이 소유한 모든 실행 세션이 종료됩니다. 다른 앱의 세션은 제어하지 않습니다. 창의 닫기 버튼은 트레이로 숨깁니다.</p><div class="dialog-actions"><button class="quiet-button" data-close>계속 작업</button><button id="confirm-exit" class="danger-button">세션 종료 후 앱 닫기</button></div></dialog>
`;

function message(error: unknown) { return error instanceof Error ? error.message : String(error); }
function resumeTitle(value: string) {
  const clean = value.replace(/\p{Cc}/gu, ' ').trim() || '이름 없는 세션';
  const encoder = new TextEncoder(); let title = ''; let bytes = 0;
  // Catalog previews can be longer than native session labels. Keep whole Unicode characters.
  for (const character of clean) {
    const size = encoder.encode(character).byteLength;
    if (bytes + size > 512) break;
    title += character; bytes += size;
  }
  return title.trim() || '이름 없는 세션';
}
function projectPath(cwd: string) { return cwd.replace(/^\\\\\?\\UNC\\/i, '\\\\').replace(/^\\\\\?\\/, '').replace(/\//g, '\\'); }
function notify(text: string, error = false) {
  const toast = document.createElement('div');
  toast.className = `toast${error ? ' error' : ''}`;
  toast.textContent = text;
  $('#toast-area').append(toast);
  setTimeout(() => toast.remove(), error ? 12000 : 6000);
  const item = document.createElement('p'); item.textContent = `${new Date().toLocaleTimeString('ko-KR', { hour: '2-digit', minute: '2-digit' })}  ${text}`;
  const list = $('#notification-list'); list.querySelector('.muted')?.remove(); list.prepend(item);
  while (list.children.length > 100) list.lastElementChild?.remove();
  $('#notification-count').hidden = false; $('#notification-count').textContent = String(++notificationCount);
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!native) throw new Error(demo ? '디자인 데모에서는 프로세스를 실행하거나 입력할 수 없습니다.' : '데스크톱 앱에서 사용할 수 있는 기능입니다.');
  return invoke<T>(command, args);
}

function scheduleSave() {
  if (!native) return;
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    const layout: Layout = { version: 1, columns, columnSizes: [...columnSizes], rowSizes: [...rowSizes], sessions: order.map(id => sessions.get(id)!).filter(Boolean).map(({ title, cwd, threadId }) => ({ title, cwd, threadId })) };
    $('#save-status').textContent = '저장 중…';
    saveQueue = saveQueue.then(() => call('save_layout', { layout })).then(() => { $('#save-status').textContent = '레이아웃 저장됨'; }).catch(error => { $('#save-status').textContent = '레이아웃 저장 실패'; notify(`저장 실패: ${message(error)}`, true); });
  }, 400);
}

function setActive(id: string, focus = true) {
  active = id;
  for (const [key, view] of terminals) view.card.classList.toggle('focused', key === id);
  for (const element of document.querySelectorAll<HTMLElement>('[data-session-link]')) element.classList.toggle('active', element.dataset.sessionLink === id);
  if (focus) terminals.get(id)?.terminal.focus();
}

function sendInput(id: string, data: string) {
  const session = sessions.get(id); const view = terminals.get(id);
  if (!session || !view || ['restored', 'starting', 'stopping', 'stopped', 'exited'].includes(session.state)) return;
  const generation = session.generation;
  view.input = view.input.then(() => call<void>('session_write', { id, generation, data })).catch(error => notify(`${session.title} 입력 실패: ${message(error)}`, true));
}

function queueResize(id: string) {
  const view = terminals.get(id); if (!native || !view) return;
  clearTimeout(view.resizeTimer);
  view.resizeTimer = setTimeout(() => {
    view.resize = view.resize.then(async () => {
      const current = sessions.get(id);
      if (!current || ['restored', 'starting', 'stopping', 'stopped', 'exited'].includes(current.state)) return;
      await call('session_resize', { id, generation: current.generation, cols: view.terminal.cols, rows: view.terminal.rows });
    }).catch(error => notify(`터미널 크기 변경 실패: ${message(error)}`, true));
  }, 60);
}

function createTerminal(session: Session) {
  const card = document.createElement('article'); card.className = 'terminal-card'; card.dataset.id = session.id;
  card.innerHTML = `<header class="terminal-header"><label class="select-session" title="선택 전송 대상"><input type="checkbox" aria-label="선택 전송 대상"/></label><span class="drag-handle" draggable="true" title="드래그로 순서 변경" aria-hidden="true">⠿</span><span class="terminal-title"></span><span class="session-state"></span><div class="card-actions"><button class="move-left icon-button" aria-label="앞으로 이동" title="앞으로 이동">‹</button><button class="move-right icon-button" aria-label="뒤로 이동" title="뒤로 이동">›</button><button class="maximize icon-button" aria-label="세션 최대화" title="세션 최대화">⛶</button><button class="stop icon-button" aria-label="이 세션 종료" title="이 세션 종료">×</button></div></header><div class="terminal-path"></div><div class="terminal-host"></div><div class="terminal-footer"><span class="terminal-identity"></span><span class="terminal-hint">직접 입력 · IME · 붙여넣기</span></div>`;
  const terminal = new Terminal({ fontFamily: '"Cascadia Code", "Cascadia Mono", Consolas, monospace', fontSize: 12, lineHeight: 1.45, cursorBlink: true, scrollback: 10000, disableStdin: !native, allowProposedApi: false, theme: { background: '#141820', foreground: '#ced5e3', cursor: '#a5b4fc', selectionBackground: '#3b4567', black: '#212735', red: '#f78b99', green: '#91d6ad', yellow: '#eac184', blue: '#8eaafb', magenta: '#c4a4f3', cyan: '#87cfd9', white: '#e3e8f0' } });
  const fit = new FitAddon(); const search = new SearchAddon(); terminal.loadAddon(fit); terminal.loadAddon(search);
  $('#grid').append(card); terminal.open(card.querySelector('.terminal-host')!);
  let frame = 0;
  const observer = new ResizeObserver(() => { cancelAnimationFrame(frame); frame = requestAnimationFrame(() => { if (card.offsetWidth && card.offsetHeight) fit.fit(); }); });
  observer.observe(card.querySelector('.terminal-host')!);
  terminals.set(session.id, { terminal, fit, search, card, observer, input: Promise.resolve(), resize: Promise.resolve() });
  terminal.onData(data => sendInput(session.id, data));
  terminal.onResize(() => queueResize(session.id));
  terminal.attachCustomKeyEventHandler(event => !(event.type === 'keydown' && ((event.altKey && /^[1-9]$/.test(event.key)) || (event.ctrlKey && event.key === 'Tab') || (event.ctrlKey && event.shiftKey && event.key.toLowerCase() === 'f'))));
  card.addEventListener('pointerdown', () => setActive(session.id, false));
  card.querySelector('input')!.addEventListener('change', event => { if ((event.target as HTMLInputElement).checked) selected.add(session.id); else selected.delete(session.id); renderCounts(); });
  card.querySelector('.maximize')!.addEventListener('click', () => { maximized = maximized === session.id ? '' : session.id; renderGrid(); setActive(session.id); });
  card.querySelector('.stop')!.addEventListener('click', () => {
    const current = sessions.get(session.id)!;
    if (['restored', 'stopped', 'exited'].includes(current.state)) { removeSession(session.id); return; }
    if (confirm(`“${current.title}” 세션을 종료할까요? 실행 중인 작업이 중단됩니다.`)) {
      void call('session_stop', { id: current.id, generation: current.generation }).then(() => {
        // A reaped error may not emit another state event. A live observation error
        // is removed only after the native stop has completed its actual cleanup.
        if (current.state === 'error' && sessions.get(current.id)?.generation === current.generation) removeSession(current.id);
      }).catch(error => notify(`종료 실패: ${message(error)}`, true));
    }
  });
  card.querySelector('.move-left')!.addEventListener('click', () => move(session.id, -1));
  card.querySelector('.move-right')!.addEventListener('click', () => move(session.id, 1));
  card.querySelector('.drag-handle')!.addEventListener('dragstart', event => { dragging = session.id; (event as DragEvent).dataTransfer!.effectAllowed = 'move'; (event as DragEvent).dataTransfer!.setData('text/plain', session.id); card.classList.add('dragging'); });
  card.addEventListener('dragend', () => { dragging = ''; card.classList.remove('dragging'); });
  card.addEventListener('dragover', event => { if (dragging && dragging !== session.id) event.preventDefault(); });
  card.addEventListener('drop', event => { event.preventDefault(); if (!dragging || dragging === session.id) return; const from = order.indexOf(dragging); const to = order.indexOf(session.id); if (from < 0 || to < 0) return; order.splice(from, 1); order.splice(to, 0, dragging); renderGrid(); renderSide(); scheduleSave(); });
  return terminal;
}

function move(id: string, offset: number) { const index = order.indexOf(id); const next = index + offset; if (next < 0 || next >= order.length) return; [order[index], order[next]] = [order[next], order[index]]; renderGrid(); renderSide(); scheduleSave(); }
function removeSession(id: string) { const view = terminals.get(id); clearTimeout(view?.resizeTimer); view?.observer.disconnect(); view?.terminal.dispose(); view?.card.remove(); terminals.delete(id); sessions.delete(id); selected.delete(id); order = order.filter(item => item !== id); if (maximized === id) maximized = ''; if (active === id) active = order[0] ?? ''; renderAll(); scheduleSave(); }

function receiveSession(info: SessionInfo, persist = true) {
  const previous = sessions.get(info.id); const session = updateSession(previous, info);
  if (session === previous) return;
  sessions.set(session.id, session);
  if (!order.includes(session.id)) order.push(session.id);
  const terminal = terminals.get(session.id)?.terminal ?? createTerminal(session);
  if (previous && previous.generation !== session.generation) terminal.reset();
  if (previous && previous.state !== session.state) notify(`${session.title} · ${states[session.state] ?? '상태 미확인'}`);
  renderAll();
  if (!previous || previous.state === 'starting') queueResize(session.id);
  const buffered = pending.get(session.id); if (buffered) { pending.delete(session.id); buffered.forEach(receiveOutput); }
  if (!active) setActive(session.id, false);
  if (persist) scheduleSave();
}

function ack(output: Output) { if (native) void call('session_output_ack', { id: output.id, generation: output.generation, seq: output.seq }).catch(error => notify(`출력 확인 전송 실패: ${message(error)}`, true)); }
async function resolveOrphan(id: string) {
  if (!native || resolving.has(id)) return;
  resolving.add(id);
  try {
    const list = await call<SessionInfo[]>('list_sessions');
    const session = list.find(item => item.id === id);
    if (session) receiveSession(session);
    else {
      const queue = pending.get(id); pending.delete(id);
      if (queue?.length) { queue.forEach(ack); notify('소유 세션을 찾을 수 없는 출력이 도착했습니다.', true); }
    }
  } catch (error) {
    const queue = pending.get(id); pending.delete(id); queue?.forEach(ack);
    notify(`터미널 메타데이터 조회 실패: ${message(error)}`, true);
  } finally { resolving.delete(id); }
}
function receiveOutput(output: Output) {
  const session = sessions.get(output.id);
  if (!session) {
    const queue = pending.get(output.id) ?? [];
    // Bound startup metadata races; do not permit an orphan event to grow memory forever.
    if (pending.size < 32 && queue.reduce((size, part) => size + part.data.length, 0) + output.data.length <= 524288) { queue.push(output); pending.set(output.id, queue); void resolveOrphan(output.id); }
    else { notify('메타데이터가 없는 터미널 출력이 버퍼 한도를 초과했습니다.', true); ack(output); }
    return;
  }
  if (!acceptOutput(session, output)) { ack(output); return; }
  session.seq = output.seq;
  const terminal = terminals.get(output.id)?.terminal;
  if (terminal) terminal.write(output.data, () => ack(output)); else ack(output);
}

function renderCounts() {
  const live = [...sessions.values()].filter(session => !['restored', 'stopped', 'exited', 'error'].includes(session.state)).length;
  $('#running-count').textContent = String(live); $('#total-count').textContent = `${sessions.size}개 세션`; $('#side-count').textContent = String(sessions.size);
  const targets = broadcastTargets([...sessions.values()], selected);
  $('#selected-count').textContent = selected.size ? `${selected.size}개 선택 · 전송 가능 ${targets.length}개` : '선택한 세션이 없습니다';
  $<HTMLButtonElement>('#broadcast-send').disabled = !native || targets.length === 0 || !$<HTMLInputElement>('#broadcast-input').value.trim();
}

function renderSide() {
  const projects = new Map<string, number>();
  for (const project of importedProjects) projects.set(projectPath(project.cwd), 0);
  for (const session of sessions.values()) { const cwd = projectPath(session.cwd); projects.set(cwd, (projects.get(cwd) ?? 0) + 1); }
  $('#project-count').textContent = String(projects.size);
  $('#projects').replaceChildren();
  for (const [cwd, count] of projects) { const row = document.createElement('button'); row.className = 'project-row'; const label = document.createElement('span'); label.textContent = importedProjects.find(project => projectPath(project.cwd) === cwd)?.name ?? cwd.split(/[\\/]/).filter(Boolean).at(-1) ?? cwd; row.title = cwd; const total = document.createElement('small'); total.textContent = String(count); row.append(document.createTextNode('▱ '), label, total); row.onclick = () => void sessionBrowser.open(cwd); $('#projects').append(row); }
  if (!projects.size) { const p = document.createElement('p'); p.className = 'side-empty'; p.textContent = '폴더를 가져와 작업을 시작하세요'; $('#projects').append(p); }
  $('#session-list').replaceChildren();
  order.forEach((id, index) => { const session = sessions.get(id)!; const button = document.createElement('button'); button.className = `session-link${active === id ? ' active' : ''}`; button.dataset.sessionLink = id; const dot = document.createElement('span'); dot.className = `status-dot ${session.state}`; const label = document.createElement('span'); label.textContent = session.title; const shortcut = document.createElement('small'); shortcut.textContent = index < 9 ? `⌥${index + 1}` : ''; button.append(dot, label, shortcut); button.onclick = () => { if (maximized) maximized = id; renderGrid(); setActive(id); }; $('#session-list').append(button); });
}

function renderGrid() {
  const grid = $('#grid');
  const count = maximized ? 1 : Math.min(columns, Math.max(1, order.length));
  const rows = Math.max(1, Math.ceil(order.length / count));
  while (columnSizes.length < count) columnSizes.push(1);
  while (rowSizes.length < rows) rowSizes.push(1);
  grid.style.gridTemplateColumns = maximized ? 'minmax(0,1fr)' : columnSizes.slice(0, count).map(size => `minmax(0,${size}fr)`).join(' ');
  grid.style.gridTemplateRows = maximized ? 'minmax(0,1fr)' : rowSizes.slice(0, rows).map(size => `minmax(0,${size}fr)`).join(' ');
  order.forEach((id, index) => { const view = terminals.get(id)!; view.card.hidden = !!maximized && id !== maximized; if (grid.children[index] !== view.card) grid.insertBefore(view.card, grid.children[index] ?? null); });
  $('#empty').hidden = order.length > 0;
  for (const button of document.querySelectorAll<HTMLElement>('[data-columns]')) { button.classList.toggle('active', Number(button.dataset.columns) === columns); button.setAttribute('aria-pressed', String(Number(button.dataset.columns) === columns)); }
  renderHandles(count, rows);
}

function renderHandles(count: number, rows: number) {
  const layer = $('#grid-handles'); layer.replaceChildren();
  if (maximized || !order.length) return;
  for (const [axis, length, sizes] of [['column', count, columnSizes], ['row', rows, rowSizes]] as const) {
    const total = sizes.slice(0, length).reduce((a, b) => a + b, 0);
    let position = 0;
    for (let index = 0; index < length - 1; index++) {
      position += sizes[index]; const boundary = position; const handle = document.createElement('div'); handle.className = `resize-handle ${axis}`;
      handle.style[axis === 'column' ? 'left' : 'top'] = `${position / total * 100}%`;
      handle.setAttribute('role', 'separator'); handle.setAttribute('aria-orientation', axis === 'column' ? 'vertical' : 'horizontal'); handle.setAttribute('aria-label', `${index + 1}번째 ${axis === 'column' ? '열' : '행'} 크기 조절`); handle.tabIndex = 0;
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
  for (const [id, session] of sessions) {
    const card = terminals.get(id)!.card;
    card.querySelector('.terminal-title')!.textContent = session.title;
    card.querySelector('.terminal-path')!.textContent = `▱  ${session.cwd}`;
    const state = card.querySelector('.session-state')!;
    state.textContent = states[session.state] ?? '상태 미확인';
    state.className = `session-state ${session.state}`;
    const identity = card.querySelector<HTMLElement>('.terminal-identity')!;
    identity.textContent = `${String(order.indexOf(id) + 1).padStart(2, '0')}  ·  ${demo ? 'DEMO · 실제 세션 아님' : session.threadId ? `스레드 ${session.threadId.slice(0, 12)}` : '로컬 세션'}`;
    identity.title = session.threadId ?? '';
    const checkbox = card.querySelector<HTMLInputElement>('input')!;
    checkbox.checked = selected.has(id);
    checkbox.disabled = !writable(session);
    card.classList.toggle('focused', id === active);
  }
  renderCounts(); renderSide(); renderGrid();
}

function openCreate(cwd?: string) { $('#create-error').hidden = true; if (cwd) { $<HTMLInputElement>('#session-cwd').value = cwd; $<HTMLInputElement>('#session-thread').value = ''; } $<HTMLDialogElement>('#create-dialog').showModal(); $<HTMLInputElement>('#session-title').focus(); }
const sessionBrowser = createSessionBrowser({
  call,
  projectsChanged(projects) { importedProjects = projects; renderSide(); },
  async openSession(session) {
    return (await start({ title: resumeTitle(session.title), cwd: session.cwd, threadId: session.id })).id;
  },
  focusSession(id) { if (maximized) maximized = id; renderGrid(); setActive(id); },
  newSession: openCreate,
  notify,
});
for (const id of ['new-session', 'side-add', 'empty-add']) $(`#${id}`).onclick = () => openCreate();
$('#catalog-open').onclick = () => void sessionBrowser.open();
$('#project-import').onclick = () => void sessionBrowser.importProject();
const executableSettings = createExecutableSettings(call);
$('#codex-settings-open').onclick = () => void executableSettings.open();
for (const button of document.querySelectorAll<HTMLElement>('[data-close]')) button.onclick = () => button.closest('dialog')?.close();
$('#column-options').onclick = event => { const button = (event.target as HTMLElement).closest<HTMLElement>('[data-columns]'); if (button) { columns = Number(button.dataset.columns); maximized = ''; renderGrid(); scheduleSave(); } };
$('#clear-selection').onclick = () => { selected.clear(); renderAll(); };
$('#broadcast-input').addEventListener('input', renderCounts);
$('#broadcast-form').onsubmit = async event => {
  event.preventDefault(); const input = $<HTMLInputElement>('#broadcast-input'); if (!input.value.trim()) return;
  const targets = broadcastTargets([...sessions.values()], selected); if (!targets.length) return;
  const data = input.value + '\r'; const send = $<HTMLButtonElement>('#broadcast-send'); send.disabled = true; input.disabled = true;
  const results = $('#broadcast-results'); results.hidden = false; results.textContent = '선택한 세션에 전송 중…';
  const outcomes = await broadcast(targets, id => sessions.get(id), target => {
    const view = terminals.get(target.id);
    const operation = (view?.input ?? Promise.resolve()).then(async () => {
      const current = sessions.get(target.id);
      if (!current || current.generation !== target.generation || !writable(current)) throw new Error('대기 중 세션 상태가 변경되어 전송하지 않았습니다.');
      await call('session_write', { id: target.id, generation: target.generation, data });
    });
    if (view) view.input = operation.catch(() => {}); // Outcome is shown per target below; keep the input queue usable.
    return operation;
  });
  results.replaceChildren(...outcomes.map(outcome => { const item = document.createElement('span'); item.textContent = `${outcome.title}: ${outcome.status === 'sent' ? '전송됨' : outcome.status === 'skipped' ? '상태 변경으로 건너뜀' : `실패 — ${outcome.error}`}`; return item; }));
  input.disabled = false; renderCounts();
};

async function start(request: SavedSession) {
  if (request.threadId) {
    const pending = pendingThreads.get(request.threadId);
    if (pending) return pending;
    const existing = [...sessions.values()].find(item => item.threadId === request.threadId && !['restored', 'stopped', 'exited'].includes(item.state));
    if (existing) { if (maximized) maximized = existing.id; renderGrid(); setActive(existing.id); return existing; }
  }
  // Reserve a slot before awaiting IPC so simultaneous restore clicks cannot exceed persistence capacity.
  if (sessions.size + pendingStarts >= MAX_WORKSPACE_SESSIONS) throw new Error(`작업 공간에는 시작 중인 세션을 포함해 최대 ${MAX_WORKSPACE_SESSIONS}개를 표시할 수 있습니다. 완료된 세션을 닫은 뒤 다시 시작하세요.`);
  pendingStarts++;
  const operation = (async () => {
    try {
      const info = await call<SessionInfo>('start_session', { request: { ...request, cols: 100, rows: 30 } });
      if ((sessions.get(info.id)?.generation ?? -1) < info.generation) receiveSession(info);
      setActive(info.id);
      queueResize(info.id);
      return info;
    } finally { pendingStarts--; if (request.threadId) pendingThreads.delete(request.threadId); }
  })();
  if (request.threadId) pendingThreads.set(request.threadId, operation);
  return operation;
}
$('#create-form').onsubmit = async event => {
  event.preventDefault(); const button = $<HTMLButtonElement>('#create-submit'); button.disabled = true;
  const threadId = $<HTMLInputElement>('#session-thread').value.trim();
  try { await start({ title: $<HTMLInputElement>('#session-title').value.trim(), cwd: $<HTMLInputElement>('#session-cwd').value.trim(), ...(threadId ? { threadId } : {}) }); $<HTMLDialogElement>('#create-dialog').close(); $<HTMLFormElement>('#create-form').reset(); }
  catch (error) { $('#create-error').hidden = false; $('#create-error').textContent = message(error); } finally { button.disabled = false; }
};
$('#restore-open').onclick = () => {
  const list = $('#restore-list'); list.replaceChildren();
  for (const session of saved) { const row = document.createElement('div'); row.className = 'restore-item'; const title = document.createElement('strong'); title.textContent = session.title; const path = document.createElement('small'); path.textContent = session.cwd; const thread = document.createElement('small'); thread.textContent = session.threadId ? `스레드 ${session.threadId}` : '스레드 ID 없음 · 새 대화로 시작'; const button = document.createElement('button'); button.className = 'quiet-button'; button.textContent = session.threadId ? '이 대화 재개 →' : '새 대화 시작 →'; button.onclick = async () => { button.disabled = true; try { await start(session); button.textContent = '시작됨'; } catch (error) { notify(`복원 실패: ${message(error)}`, true); button.disabled = false; } }; row.append(title, path, thread, button); list.append(row); }
  $<HTMLDialogElement>('#restore-dialog').showModal();
};

async function refreshExternal() {
  const list = $('#external-list'); list.textContent = '관측 기록을 불러오는 중…';
  try { const external = await call<{ id: string; title: string; cwd: string; status: string; observedAt: string }[]>('list_external'); list.replaceChildren();
    for (const session of external) { const item = document.createElement('div'); item.className = 'external-item'; const title = document.createElement('strong'); title.textContent = session.title; const cwd = document.createElement('small'); cwd.textContent = session.cwd || '경로 미확인'; const state = document.createElement('small'); state.textContent = `현재 상태 미확인 · 관측 ${session.observedAt || '시각 미확인'}`; item.append(title, cwd, state); list.append(item); }
    if (!external.length) list.textContent = '발견된 외부 기록이 없습니다.';
  } catch (error) { list.textContent = `불러오기 실패: ${message(error)}`; }
}
$('#external-toggle').onclick = () => { $<HTMLDialogElement>('#external-dialog').showModal(); void refreshExternal(); };
$('#external-refresh').onclick = () => void refreshExternal();
$('#notifications').onclick = () => { notificationCount = 0; $('#notification-count').hidden = true; $<HTMLDialogElement>('#notification-dialog').showModal(); };
$('#exit').onclick = () => $<HTMLDialogElement>('#exit-dialog').showModal();
$('#confirm-exit').onclick = async () => { const button = $<HTMLButtonElement>('#confirm-exit'); button.disabled = true; try { clearTimeout(saveTimer); scheduleSave(); await new Promise(resolve => setTimeout(resolve, 450)); await saveQueue; await call('complete_exit', { confirmed: true }); } catch (error) { notify(`앱 종료 실패: ${message(error)}`, true); } finally { button.disabled = false; } };

function toggleSearch(force?: boolean) { searchVisible = force ?? !searchVisible; $('#search-bar').hidden = !searchVisible; if (searchVisible) $<HTMLInputElement>('#terminal-search').focus(); }
function find(previous = false) { const value = $<HTMLInputElement>('#terminal-search').value; const search = terminals.get(active)?.search; $('#search-result').textContent = !value ? '' : search && (previous ? search.findPrevious(value) : search.findNext(value)) ? '결과 찾음' : '결과 없음'; }
$('#search-toggle').onclick = () => toggleSearch(); $('#search-close').onclick = () => toggleSearch(false); $('#search-prev').onclick = () => find(true); $('#search-next').onclick = () => find(); $('#terminal-search').oninput = () => find(); $('#terminal-search').onkeydown = event => { if (event.key === 'Enter') { event.preventDefault(); find(event.shiftKey); } if (event.key === 'Escape') toggleSearch(false); };
document.addEventListener('keydown', event => {
  if (document.querySelector('dialog[open]')) return;
  let id: string | undefined;
  if (event.altKey && /^[1-9]$/.test(event.key)) id = order[Number(event.key) - 1];
  if (event.ctrlKey && event.key === 'Tab' && order.length) id = order[(order.indexOf(active) + (event.shiftKey ? order.length - 1 : 1)) % order.length];
  if (id) { event.preventDefault(); event.stopPropagation(); if (maximized) { maximized = id; renderGrid(); } setActive(id); }
  if (event.ctrlKey && event.shiftKey && event.key.toLowerCase() === 'f') { event.preventDefault(); event.stopPropagation(); toggleSearch(true); }
  if (event.key === 'Escape' && maximized) { event.preventDefault(); event.stopPropagation(); maximized = ''; renderGrid(); }
}, true);

async function initialize() {
  if (!native) {
    $('#connection').textContent = demo ? '디자인 데모' : '브라우저 미리보기'; $('#mode-label').textContent = demo ? 'DESIGN DEMO' : 'BROWSER PREVIEW'; $('#runtime-banner').hidden = false;
    $('#runtime-banner').textContent = demo ? '디자인 데모 · 아래 터미널은 정적인 예시입니다. 실제 프로세스 실행·입력·전송은 지원하지 않습니다.' : '브라우저 미리보기입니다. 실제 세션 실행은 Agent Grid 데스크톱 앱에서 사용할 수 있습니다.';
    $('#save-status').textContent = '미리보기 · 저장 안 함';
    if (demo) seedDemo(); renderAll(); return;
  }
  try {
    unlisteners.push(await listen<Output>('terminal-output', event => receiveOutput(event.payload)));
    unlisteners.push(await listen<SessionInfo>('session-changed', event => receiveSession(event.payload)));
    unlisteners.push(await listen<string>('application-error', event => notify(event.payload, true)));
    unlisteners.push(await listen('exit-requested', () => { const dialog = $<HTMLDialogElement>('#exit-dialog'); if (!dialog.open) dialog.showModal(); }));
    const [layoutValue, existing] = await Promise.all([call<unknown>('load_layout'), call<SessionInfo[]>('list_sessions')]);
    const wrapper = layoutValue as { layout?: unknown; sessions?: SessionInfo[] } | null;
    const layout = parseLayout(wrapper && 'layout' in wrapper ? wrapper.layout : layoutValue);
    if (layout) { columns = layout.columns; columnSizes = layout.columnSizes; rowSizes = layout.rowSizes; }
    for (const session of existing) if (session.state !== 'restored' && (sessions.get(session.id)?.generation ?? -1) < session.generation) receiveSession(session, false);
    saved = chooseSavedSessions(layout, existing);
    $('#restore-banner').hidden = saved.length === 0;
    $('#connection').textContent = '로컬 연결됨'; renderAll();
    void sessionBrowser.refreshProjects().catch(error => notify(`프로젝트 목록 조회 실패: ${message(error)}`, true));
  } catch (error) { $('#connection').textContent = '연결 오류'; notify(`초기화 실패: ${message(error)}`, true); }
}

function seedDemo() {
  const examples = [
    { title: 'API 구조 개선', cwd: 'C:\\workspace\\orbit-api', state: 'running', lines: ['\x1b[38;5;141m╭─ agent · local workspace\x1b[0m', '', '\x1b[90m›\x1b[0m API 모듈의 의존성을 정리해 주세요.', '', '\x1b[38;5;110m●\x1b[0m 현재 라우트와 서비스 경계를 확인하고 있습니다.', '', '  \x1b[90mRead\x1b[0m  src/routes/projects.ts', '  \x1b[90mRead\x1b[0m  src/services/project-service.ts', '  \x1b[90mRead\x1b[0m  src/db/connection.ts', '', '\x1b[38;5;114m✓\x1b[0m 12개 모듈의 호출 관계 확인', '\x1b[38;5;110m◌\x1b[0m 중복된 검증 로직을 정리하는 중…'] },
    { title: '대시보드 UI', cwd: 'C:\\workspace\\orbit-web', state: 'idle', lines: ['\x1b[38;5;141m╭─ agent · local workspace\x1b[0m', '', '\x1b[90m›\x1b[0m 대시보드 카드 레이아웃을 개선해 주세요.', '', '\x1b[38;5;114m●\x1b[0m 카드 간격과 작은 화면에서의 배치를 조정했습니다.', '', '  \x1b[38;5;114m+\x1b[0m src/components/Overview.tsx', '  \x1b[38;5;114m+\x1b[0m src/styles/dashboard.css', '', '\x1b[90m  다음 작업을 입력할 수 있습니다.\x1b[0m', '', '\x1b[38;5;141m›\x1b[0m '] },
    { title: '회귀 테스트', cwd: 'C:\\workspace\\orbit-api', state: 'running', lines: ['\x1b[38;5;141m╭─ agent · local workspace\x1b[0m', '', '\x1b[90m›\x1b[0m 변경된 API의 회귀 테스트를 확인해 주세요.', '', '\x1b[38;5;110m●\x1b[0m 테스트 시나리오를 검토하고 있습니다.', '', '  \x1b[90mInspect\x1b[0m  tests/project.test.ts', '  \x1b[90mInspect\x1b[0m  tests/auth.test.ts', '', '\x1b[38;5;114m✓\x1b[0m 권한 경계와 잘못된 입력 사례 확인', '\x1b[38;5;110m◌\x1b[0m 타임아웃 경로 분석 중…'] },
    { title: '릴리스 노트', cwd: 'C:\\workspace\\orbit-docs', state: 'waiting', lines: ['\x1b[38;5;141m╭─ agent · local workspace\x1b[0m', '', '\x1b[90m›\x1b[0m 이번 변경의 릴리스 노트를 준비해 주세요.', '', '\x1b[38;5;179m●\x1b[0m 확인이 필요합니다.', '', '  공개 문서에 포함할 변경 범위를 선택해 주세요.', '', '  \x1b[38;5;179m1.\x1b[0m 사용자에게 보이는 변경만 포함', '  \x1b[90m2.\x1b[0m 개발자 변경 내역도 포함', '', '\x1b[90m  응답을 기다리고 있습니다…\x1b[0m'] },
  ];
  examples.forEach((example, index) => { const id = `demo-${index}`; receiveSession({ ...example, id, generation: 1 }, false); terminals.get(id)!.terminal.write(example.lines.join('\r\n')); });
  setActive('demo-0', false);
}

window.addEventListener('beforeunload', () => { for (const unlisten of unlisteners) unlisten(); for (const view of terminals.values()) { clearTimeout(view.resizeTimer); view.observer.disconnect(); view.terminal.dispose(); } });
void initialize();
