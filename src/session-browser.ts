export interface ProjectInfo { id: string; name: string; cwd: string; createdAt: number }
export interface CatalogSession { id: string; title: string; cwd: string; createdAt: number; updatedAt: number; status: string }
interface CatalogPage { sessions: CatalogSession[]; nextCursor?: string | null }
interface PreviewMessage { id: string; role: 'user' | 'assistant'; text: string }
interface CatalogPreview { messages: PreviewMessage[]; nextCursor?: string | null; unavailableReason?: string | null }
interface BrowserHost {
  call<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  projectsChanged(projects: ProjectInfo[]): void;
  openSession(session: CatalogSession): Promise<string>;
  focusSession(id: string): void;
  newSession(cwd?: string): void;
  notify(text: string, error?: boolean): void;
}

// One catalog IPC at a time, with only the newest pending intent retained.
// A slow query must not create a server for every intermediate keystroke.
class CatalogQueue {
  private active = false;
  private pending?: { run: () => Promise<void>; discard: () => void };
  add(run: () => Promise<void>, discard: () => void) {
    this.pending?.discard(); this.pending = { run, discard }; void this.pump();
  }
  cancel() { this.pending?.discard(); this.pending = undefined; }
  private async pump() {
    if (this.active) return;
    this.active = true;
    try { while (this.pending) { const work = this.pending; this.pending = undefined; await work.run(); } }
    finally { this.active = false; }
  }
}

export function createSessionBrowser(host: BrowserHost) {
  const dialog = document.createElement('dialog'); dialog.id = 'catalog-dialog';
  dialog.setAttribute('aria-labelledby', 'catalog-title');
  dialog.innerHTML = `<div class="dialog-heading"><div><span class="eyebrow">SESSION LIBRARY</span><h2 id="catalog-title">세션 불러오기</h2></div><button id="catalog-close" class="icon-button" aria-label="세션 불러오기 닫기">×</button></div>
    <p class="catalog-description">프로젝트와 제목으로 찾고, 최근 대화를 확인한 뒤 이어서 작업하세요.</p>
    <div class="catalog-panes"><section class="catalog-sidebar" aria-label="세션 검색과 목록">
      <label for="catalog-project">프로젝트</label><select id="catalog-project"><option value="">모든 프로젝트</option></select>
      <label for="catalog-search">세션 제목 검색</label><input id="catalog-search" type="search" placeholder="찾을 세션 제목을 입력하세요" autocomplete="off" maxlength="200"/>
      <div class="catalog-list-heading"><span>최근 수정 순</span><button id="catalog-refresh" class="text-button">새로 고침</button></div>
      <div id="catalog-list-status" class="catalog-status" role="status" aria-live="polite"></div><div id="catalog-list" role="listbox" aria-label="기존 세션 목록" tabindex="0"></div>
      <button id="catalog-more" class="quiet-button" hidden>세션 더 불러오기</button>
    </section><section class="catalog-detail" aria-label="선택한 세션 정보">
      <div id="catalog-placeholder" class="catalog-placeholder">왼쪽에서 이어갈 세션을 선택하세요.</div>
      <div id="catalog-selected" hidden><h3 id="catalog-session-title"></h3><dl id="catalog-metadata"></dl><h4>최근 대화</h4><div id="catalog-preview-status" class="catalog-status" role="status" aria-live="polite"></div><div id="catalog-messages"></div><button id="catalog-preview-more" class="quiet-button" hidden>이전 대화 더 보기</button></div>
      <div id="catalog-resume-error" class="inline-error" role="alert" hidden></div>
      <div class="catalog-actions"><button id="catalog-new" class="quiet-button">새 세션 시작</button><button id="catalog-resume" class="primary-button" disabled>세션 불러오기 →</button></div>
    </section></div>`;
  document.body.append(dialog);
  const $ = <T extends HTMLElement = HTMLElement>(selector: string) => dialog.querySelector<T>(selector)!;
  const list = $('#catalog-list'); const projectSelect = $<HTMLSelectElement>('#catalog-project');
  const search = $<HTMLInputElement>('#catalog-search'); const queue = new CatalogQueue();
  let projects: ProjectInfo[] = []; let rows: CatalogSession[] = []; let selected: CatalogSession | undefined;
  let listVersion = 0; let previewVersion = 0; let listCursor: string | null = null; let previewCursor: string | null = null;
  let listLoading = false; let previewLoading = false; let opening = false; let openAttempt = 0; let timer: ReturnType<typeof setTimeout> | undefined;
  const errorMessage = (error: unknown) => error instanceof Error ? error.message : String(error);
  const date = (seconds: number) => Number.isFinite(seconds) && seconds > 0 ? new Date(seconds * 1000).toLocaleString('ko-KR') : '기록 없음';

  function setProjects(value: ProjectInfo[]) {
    projects = value; host.projectsChanged(projects);
    const current = projectSelect.value; projectSelect.replaceChildren(new Option('모든 프로젝트', ''));
    for (const project of projects) projectSelect.add(new Option(project.name, project.cwd));
    if (current && !projects.some(project => project.cwd === current)) projectSelect.add(new Option(current, current));
    projectSelect.value = current;
  }
  async function refreshProjects() { const value = await host.call<ProjectInfo[]>('list_projects'); setProjects(value); }
  function renderRows() {
    list.replaceChildren();
    for (const row of rows) {
      const button = document.createElement('button'); button.className = `catalog-row${row.id === selected?.id ? ' selected' : ''}`;
      button.setAttribute('role', 'option'); button.setAttribute('aria-selected', String(row.id === selected?.id)); button.dataset.threadId = row.id;
      const title = document.createElement('strong'); title.textContent = row.title || '이름 없는 세션';
      const cwd = document.createElement('span'); cwd.textContent = row.cwd;
      const time = document.createElement('small'); time.textContent = date(row.updatedAt);
      button.append(title, cwd, time); button.onclick = () => select(row); list.append(button);
    }
    list.setAttribute('aria-busy', String(listLoading));
    $<HTMLButtonElement>('#catalog-more').hidden = !listCursor;
    $<HTMLButtonElement>('#catalog-more').disabled = listLoading;
  }
  function clearSelection() {
    selected = undefined; previewVersion++; previewCursor = null; previewLoading = false;
    $('#catalog-selected').hidden = true; $('#catalog-placeholder').hidden = false;
    $('#catalog-placeholder').textContent = '왼쪽에서 이어갈 세션을 선택하세요.';
    $('#catalog-messages').replaceChildren(); $('#catalog-resume-error').hidden = true;
    $<HTMLButtonElement>('#catalog-resume').disabled = true;
    $('#catalog-preview-more').hidden = true;
  }
  function querySoon(delay = 0) {
    clearTimeout(timer); const version = ++listVersion; clearSelection(); rows = []; listCursor = null;
    listLoading = true; $('#catalog-list-status').textContent = '세션을 찾는 중…'; renderRows();
    timer = setTimeout(() => loadList(version), delay);
  }
  function loadList(version: number, cursor?: string) {
    const query = search.value.trim(); const cwd = projectSelect.value;
    const current = () => dialog.open && version === listVersion;
    const done = () => { if (current()) { listLoading = false; renderRows(); } };
    queue.add(async () => {
      if (!current()) return;
      try {
        const page = await host.call<CatalogPage>('catalog_list', { request: { query, ...(cwd ? { cwd } : {}), ...(cursor ? { cursor } : {}) } });
        if (!current()) return;
        const known = new Set(rows.map(row => row.id));
        rows.push(...page.sessions.filter(row => !known.has(row.id)));
        listCursor = page.nextCursor ?? null;
        $('#catalog-list-status').textContent = rows.length ? `${rows.length}개 세션${listCursor ? ' · 더 불러올 수 있습니다' : ''}` : '검색 결과가 없습니다. 이 프로젝트에서 새 세션을 시작할 수 있습니다.';
        if (!selected && rows.length) select(rows[0]);
        else if (!rows.length) $('#catalog-placeholder').textContent = '조건에 맞는 세션이 없습니다. 다른 제목으로 찾거나 새 세션을 시작하세요.';
      } catch (error) { if (current()) $('#catalog-list-status').textContent = `세션 조회 실패: ${errorMessage(error)}`; }
      finally { done(); }
    }, done);
  }
  function select(row: CatalogSession) {
    if (opening) return;
    selected = row; const version = ++previewVersion; previewCursor = null; previewLoading = false;
    $('#catalog-placeholder').hidden = true; $('#catalog-selected').hidden = false;
    $('#catalog-session-title').textContent = row.title || '이름 없는 세션';
    const metadata = $('#catalog-metadata'); metadata.replaceChildren();
    for (const [label, value] of [['프로젝트', row.cwd], ['생성', date(row.createdAt)], ['마지막 수정', date(row.updatedAt)], ['상태', '현재 상태 미확인 · 저장된 기록'], ['대화 ID', row.id]]) {
      const term = document.createElement('dt'); term.textContent = label; const detail = document.createElement('dd'); detail.textContent = value; metadata.append(term, detail);
    }
    $('#catalog-messages').replaceChildren(); $('#catalog-resume-error').hidden = true;
    $<HTMLButtonElement>('#catalog-resume').disabled = false; renderRows(); loadPreview(row, version);
  }
  function loadPreview(row: CatalogSession, version: number, cursor?: string) {
    previewLoading = true; $('#catalog-preview-status').textContent = '최근 대화를 불러오는 중…'; $('#catalog-preview-more').hidden = true;
    const current = () => dialog.open && version === previewVersion && selected?.id === row.id;
    const done = () => { if (current()) { previewLoading = false; $<HTMLButtonElement>('#catalog-preview-more').hidden = !previewCursor; } };
    queue.add(async () => {
      if (!current()) return;
      try {
        const page = await host.call<CatalogPreview>('catalog_preview', { threadId: row.id, ...(cursor ? { cursor } : {}) });
        if (!current()) return;
        const fragment = document.createDocumentFragment(); const known = new Set([...$('#catalog-messages').children].map(element => (element as HTMLElement).dataset.messageId));
        for (const item of page.messages) {
          if (known.has(item.id)) continue;
          const message = document.createElement('article'); message.className = `catalog-message ${item.role === 'user' ? 'user' : 'assistant'}`; message.dataset.messageId = item.id;
          const role = document.createElement('strong'); role.textContent = item.role === 'user' ? '사용자' : '답변';
          const body = document.createElement('p'); body.textContent = item.text; message.append(role, body); fragment.append(message);
        }
        if (cursor) $('#catalog-messages').prepend(fragment); else $('#catalog-messages').replaceChildren(fragment);
        previewCursor = page.nextCursor ?? null;
        $('#catalog-preview-status').textContent = page.unavailableReason || ($('#catalog-messages').children.length ? '대화 미리보기 · 읽기 전용' : '아직 표시할 대화가 없습니다.');
      } catch (error) { if (current()) $('#catalog-preview-status').textContent = `미리보기 조회 실패: ${errorMessage(error)}`; }
      finally { done(); }
    }, done);
  }
  async function resume() {
    const row = selected; if (!row || opening) return;
    const attempt = ++openAttempt; const version = listVersion;
    opening = true; $<HTMLButtonElement>('#catalog-resume').disabled = true; $('#catalog-resume-error').hidden = true;
    try { const id = await host.openSession(row); if (dialog.open && version === listVersion) { dialog.close(); host.focusSession(id); } }
    catch (error) { if (dialog.open && version === listVersion) { $('#catalog-resume-error').hidden = false; $('#catalog-resume-error').textContent = `불러오기 실패: ${errorMessage(error)}`; } else host.notify(`불러오기 실패: ${errorMessage(error)}`, true); }
    finally { if (attempt === openAttempt) { opening = false; $<HTMLButtonElement>('#catalog-resume').disabled = !selected; } }
  }
  async function open(cwd?: string) {
    if (dialog.open) return;
    search.value = ''; projectSelect.value = '';
    if (cwd) { if (![...projectSelect.options].some(option => option.value === cwd)) projectSelect.add(new Option(cwd.split(/[\\/]/).filter(Boolean).at(-1) || cwd, cwd)); projectSelect.value = cwd; }
    dialog.showModal(); querySoon(); search.focus();
    // Project registry errors do not prevent browsing unfiltered existing sessions.
    try { await refreshProjects(); } catch (error) { host.notify(`프로젝트 목록 조회 실패: ${errorMessage(error)}`, true); }
  }
  async function importProject() {
    const button = document.querySelector<HTMLButtonElement>('#project-import')!; button.disabled = true;
    try { const project = await host.call<ProjectInfo | null>('choose_project'); if (project) { setProjects([...projects.filter(item => item.id !== project.id && item.cwd !== project.cwd), project]); await open(project.cwd); } }
    catch (error) { host.notify(`프로젝트 가져오기 실패: ${errorMessage(error)}`, true); }
    finally { button.disabled = false; }
  }
  $('#catalog-close').onclick = () => dialog.close();
  dialog.addEventListener('close', () => { clearTimeout(timer); listVersion++; previewVersion++; openAttempt++; opening = false; queue.cancel(); clearSelection(); });
  search.addEventListener('compositionstart', () => clearTimeout(timer));
  search.oninput = event => { if (!(event as InputEvent).isComposing) querySoon(250); };
  search.addEventListener('compositionend', () => querySoon(250));
  projectSelect.onchange = () => querySoon();
  $('#catalog-refresh').onclick = () => querySoon();
  $('#catalog-more').onclick = () => { if (listCursor && !listLoading) { listLoading = true; renderRows(); loadList(listVersion, listCursor); } };
  $('#catalog-preview-more').onclick = () => { if (selected && previewCursor && !previewLoading) loadPreview(selected, previewVersion, previewCursor); };
  $('#catalog-new').onclick = () => { const cwd = projectSelect.value || selected?.cwd; dialog.close(); host.newSession(cwd); };
  $('#catalog-resume').onclick = () => void resume();
  dialog.addEventListener('keydown', event => {
    if (event.isComposing || event.keyCode === 229) return;
    if (event.key === 'Escape') { event.preventDefault(); dialog.close(); return; }
    if (event.altKey || event.ctrlKey || event.metaKey || opening) return;
    const target = event.target as HTMLElement;
    if (event.key === 'Enter' && target === search) { event.preventDefault(); querySoon(); }
    else if (['ArrowDown', 'ArrowUp'].includes(event.key) && (target === search || target === list || list.contains(target))) {
      event.preventDefault(); if (!rows.length) return;
      const index = rows.findIndex(row => row.id === selected?.id); const next = Math.max(0, Math.min(rows.length - 1, index + (event.key === 'ArrowDown' ? 1 : -1)));
      select(rows[next]); list.querySelector<HTMLElement>('[aria-selected="true"]')?.focus();
    } else if (event.key === 'Enter' && (target === list || list.contains(target))) { event.preventDefault(); void resume(); }
  });
  return { open, importProject, refreshProjects };
}
