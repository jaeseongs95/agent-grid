import { readFile } from 'node:fs/promises';
import { test, expect, type Page } from '@playwright/test';

const projects = [
  { id: 'alpha', name: 'Alpha', cwd: 'D:\\Work\\Alpha', createdAt: 1700000000, favorite: false },
  { id: 'beta', name: 'Beta', cwd: 'D:\\Work\\Beta', createdAt: 1700000000, favorite: false },
  { id: 'empty', name: 'Empty', cwd: 'D:\\Work\\Empty', createdAt: 1700000000, favorite: false },
];
const owned = [
  { id: 'a1', title: 'Alpha first', cwd: 'd:/work/ALPHA/' },
  { id: 'b1', title: 'Beta first', cwd: projects[1].cwd },
  { id: 'a2', title: 'Alpha second', cwd: '\\\\?\\D:\\Work\\Alpha' },
  { id: 'parent', title: 'Parent session', cwd: 'D:\\Work' },
  { id: 'child', title: 'Child session', cwd: 'D:\\Work\\Alpha\\child' },
  { id: 'sibling', title: 'Sibling session', cwd: 'D:\\Work\\Alphabet' },
].map(session => ({ ...session, generation: 7, state: 'idle', active: true, threadId: `thread-${session.id}` }));

test.afterEach(async ({ page }, testInfo) => {
  if (testInfo.status !== testInfo.expectedStatus) {
    await testInfo.attach('workspace-document.html', { body: await page.content(), contentType: 'text/html' });
  }
});

async function workspace(page: Page, options: { capacity?: boolean } = {}) {
  const failures: string[] = [];
  page.on('pageerror', error => failures.push(`스크립트 오류: ${error.message}`));
  page.on('console', message => { if (message.type() === 'error') failures.push(`콘솔 오류: ${message.text()}`); });
  page.on('requestfailed', request => failures.push(`요청 실패: ${request.method()} ${request.url()} ${request.failure()?.errorText}`));
  page.on('response', response => { if (response.status() >= 400) failures.push(`응답 오류: ${response.status()} ${response.url()}`); });
  await page.addInitScript(({ projects, owned, options }) => {
    const state = window as any;
    state.isTauri = true;
    let callback = 0;
    const callbacks = new Map<number, (event: any) => void>();
    const listeners = new Map<string, (event: any) => void>();
    const existing = JSON.parse(localStorage.getItem('tabs-sessions') || 'null') ?? (options.capacity ? Array.from({ length: 31 }, (_, index) => ({ ...owned[0], id: 'capacity-' + index, threadId: 'capacity-thread-' + index })) : owned);
    const projectList = () => JSON.parse(localStorage.getItem('tabs-projects') || JSON.stringify(projects));
    const saveProjects = (value: any[]) => localStorage.setItem('tabs-projects', JSON.stringify(value));
    let ownershipReads = 0;
    state.tabsFixture = { calls: [], nodes: [], holdNextWrite: false, holdStart: false, holdOwnership: false, catalog: false, catalogOwned: false, releaseOwnership: () => {}, releaseWrite: () => {}, releaseStart: () => {},
      emit(event: string, payload: any) { listeners.get(event)!({ event, id: 1, payload }); },
    };
    state.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
    state.__TAURI_INTERNALS__ = {
      transformCallback(handler: (event: any) => void) { callbacks.set(++callback, handler); return callback; },
      unregisterCallback() {},
      async invoke(command: string, args: any = {}) {
        state.tabsFixture.calls.push({ command, args });
        if (command === 'plugin:event|listen') { listeners.set(args.event, callbacks.get(args.handler)!); return callback; }
        if (command.startsWith('plugin:event|')) return callback;
        if (command === 'conversation_request' && args.request.type === 'contextStatus') {
          if (state.tabsFixture.contextFailure) throw new Error('PRIVATE_CONTEXT_ERROR');
          return state.tabsFixture.contextSnapshots?.[args.id] ?? { revision: 0, connected: true, guiConnected: false, tokenUsageFresh: false, compacting: false, active: false, canCompact: false };
        }
        if (command === 'get_ui_language') return 'ko';
        if (command === 'set_ui_language') return { show: args.language === 'en' ? 'Open OpenDIY Desktop' : 'OpenDIY Desktop 열기', exit: args.language === 'en' ? 'Exit…' : '종료…' };
        if (command === 'load_layout') return { version: 1, columns: 2, sessions: options.capacity ? [{ title: 'Pending slot 32', cwd: projects[0].cwd, threadId: 'pending-slot32' }] : [] };
        if (command === 'list_sessions') {
          if (ownershipReads++ > 0 && state.tabsFixture.holdOwnership) await new Promise<void>(resolve => { state.tabsFixture.releaseOwnership = resolve; });
          return existing;
        }
        if (command === 'list_projects') {
          const snapshot = projectList();
          if (state.tabsFixture.holdNextProjects) { state.tabsFixture.holdNextProjects = false; await new Promise<void>(resolve => { state.tabsFixture.releaseProjects = resolve; }); }
          return snapshot;
        }
        if (command === 'choose_project_folder') return state.tabsFixture.chosenFolder ?? null;
        if (command === 'create_project') { const project = { id: 'project-' + projectList().length, name: args.name, cwd: args.cwd, createdAt: 1700000000, favorite: false }; saveProjects([...projectList(), project]); return project; }
        if (command === 'rename_project' || command === 'set_project_favorite') { if (command === 'set_project_favorite' && state.tabsFixture.failFavoriteOnce) { state.tabsFixture.failFavoriteOnce = false; throw new Error('즐겨찾기 저장 실패'); } const values = projectList(); const item = values.find((project: any) => project.id === args.id); if (!item) throw new Error('Project not found'); if (command === 'rename_project') item.name = args.name; else item.favorite = args.favorite; saveProjects(values); return item; }
        if (command === 'delete_project') {
          saveProjects(projectList().filter((project: any) => project.id !== args.id));
          for (const session of existing) if (session.projectId === args.id || (args.id === 'alpha' && ['a1', 'a2'].includes(session.id) && !session.projectAssigned)) { session.projectId = null; session.projectAssigned = true; session.projectRevision = (session.projectRevision ?? 0) + 1; }
          localStorage.setItem('tabs-sessions', JSON.stringify(existing)); return null;
        }
        if (command === 'move_session_to_project') {
          if (state.tabsFixture.failMove) throw new Error('Generation changed');
          const session = existing.find((item: any) => item.id === args.id && item.generation === args.generation); if (!session) throw new Error('Generation changed');
          session.projectId = args.projectId; session.projectAssigned = true; session.projectRevision = (session.projectRevision ?? 0) + 1; localStorage.setItem('tabs-sessions', JSON.stringify(existing)); return { ...session };
        }
        if (command === 'save_layout') { state.tabsFixture.savedLayout = args.layout; return null; }
        if (command === 'choose_project') {
          const imported = { id: 'new', name: 'Imported', cwd: 'D:\\Work\\Imported', createdAt: 1700000000, favorite: false };
          localStorage.setItem('tabs-projects', JSON.stringify([...projects, imported])); return imported;
        }
        if (command === 'catalog_list') return { sessions: state.tabsFixture.catalog ? [{ id: state.tabsFixture.catalogOwned ? owned[0].threadId : 'new-alpha-thread', title: 'Catalog Alpha', cwd: projects[0].cwd, createdAt: 1700000000, updatedAt: 1700000000, status: 'idle' }] : [], nextCursor: null };
        if (command === 'catalog_preview') return { messages: [], nextCursor: null };
        if (command === 'session_write' && state.tabsFixture.holdNextWrite) {
          state.tabsFixture.holdNextWrite = false;
          await new Promise<void>(resolve => { state.tabsFixture.releaseWrite = resolve; });
        }
        if (command === 'start_session') {
          if (state.tabsFixture.holdStart) await new Promise<void>(resolve => { state.tabsFixture.releaseStart = resolve; });
          return { ...args.request, ...(Object.hasOwn(args.request, 'projectId') ? { projectAssigned: true } : {}), id: 'started-alpha', generation: 1, state: 'idle', active: true };
        }
        return null;
      },
    };
  }, { projects, owned, options });
  const response = await page.goto('/');
  expect(response?.ok(), '작업 공간 문서 응답').toBe(true);
  try {
    await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  } finally {
    await test.info().attach('workspace-load.json', { body: JSON.stringify({ url: page.url(), failures }, null, 2), contentType: 'application/json' });
  }
  await expect(tab(page, 'Alpha')).toBeVisible();
}
const tab = (page: Page, name: string) => page.locator('#project-tabs [role=tab]').filter({ hasText: new RegExp(`^${name}(?:\\s|$)`) });
const visibleCards = (page: Page) => page.locator('.terminal-card:visible');
const visibleIds = (page: Page) => visibleCards(page).evaluateAll(cards => cards.map(card => (card as HTMLElement).dataset.id));
const card = (page: Page, id: string) => page.locator(`.terminal-card[data-id="${id}"]`);

test('project tabs filter exact Windows paths, counts, sidebar and empty state without process actions', async ({ page }, testInfo) => {
  await workspace(page);
  await expect(visibleCards(page)).toHaveCount(6);
  await tab(page, 'Alpha').click();
  await expect(tab(page, 'Alpha')).toHaveAttribute('aria-selected', 'true');
  await expect.poll(() => visibleIds(page)).toEqual(['a1', 'a2']);
  await expect(page.locator('#total-count')).toHaveText('2개 세션');
  await expect(page.locator('#running-count')).toHaveText('2');
  await expect(page.locator('#side-count')).toHaveText('2');
  await expect(page.locator('#session-list .session-link')).toHaveCount(2);
  await tab(page, 'Beta').click();
  await expect.poll(() => visibleIds(page)).toEqual(['b1']);
  await tab(page, 'Empty').click();
  await expect(visibleCards(page)).toHaveCount(0);
  await expect(page.locator('#empty')).toBeVisible();
  await expect(page.locator('#session-list .session-link')).toHaveCount(0);
  await page.locator('#project-tabs [role=tab]').first().click();
  await expect(visibleCards(page)).toHaveCount(6);
  await expect(page.locator('.terminal-card')).toHaveCount(6);
  const actions = await page.evaluate(() => (window as any).tabsFixture.calls.filter((call: any) => ['start_session', 'session_stop', 'session_write'].includes(call.command)));
  expect(actions).toEqual([]);
  await tab(page, 'Alpha').click();
  await page.screenshot({ path: testInfo.outputPath('project-tabs.png'), fullPage: true });
});

test('hidden terminals keep their DOM, accept output and acknowledge it while project tabs switch', async ({ page }) => {
  await workspace(page);
  await page.evaluate(() => { (window as any).tabsFixture.nodes = [...document.querySelectorAll('.terminal-card')].map(card => [card, card.querySelector('.xterm')]); });
  await tab(page, 'Beta').click();
  await page.evaluate(() => (window as any).tabsFixture.emit('terminal-output', { id: 'a1', generation: 7, seq: 1, data: 'PROJECT_ALPHA_BACKGROUND_OUTPUT\r\n' }));
  await expect.poll(() => page.evaluate(() => (window as any).tabsFixture.calls.filter((call: any) => call.command === 'session_output_ack' && call.args.id === 'a1' && call.args.seq === 1).length)).toBe(1);
  for (let attempt = 0; attempt < 3; attempt++) { await tab(page, 'Alpha').click(); await tab(page, 'Beta').click(); }
  await tab(page, 'Alpha').click();
  expect(await page.evaluate(() => (window as any).tabsFixture.nodes.every(([card, terminal]: HTMLElement[]) => card.isConnected && card.querySelector('.xterm') === terminal))).toBe(true);
  await page.keyboard.press('Alt+1');
  await page.keyboard.press('Control+Shift+F');
  await page.locator('#terminal-search').fill('PROJECT_ALPHA_BACKGROUND_OUTPUT');
  await expect(page.locator('#search-result')).toHaveText('결과 찾음');
  await expect(card(page, 'a1')).toHaveClass(/focused/);
});

test('session shortcuts, maximize and reorder stay within the selected project', async ({ page }) => {
  await workspace(page);
  await card(page, 'b1').getByLabel('세션 최대화').click();
  await tab(page, 'Alpha').click();
  await expect.poll(() => visibleIds(page)).toEqual(['a1', 'a2']);
  await page.keyboard.press('Alt+2');
  await expect(card(page, 'a2')).toHaveClass(/focused/);
  await page.keyboard.press('Control+Tab');
  await expect(card(page, 'a1')).toHaveClass(/focused/);
  await page.keyboard.press('Control+Shift+Tab');
  await expect(card(page, 'a2')).toHaveClass(/focused/);
  await card(page, 'a1').getByLabel('뒤로 이동').click();
  await expect.poll(() => visibleIds(page)).toEqual(['a2', 'a1']);
  await page.locator('#project-tabs [role=tab]').first().click();
  await expect.poll(() => visibleIds(page)).toEqual(['a2', 'b1', 'a1', 'parent', 'child', 'sibling']);
});

test('terminals have no shared send controls and direct input reaches only its focused session', async ({ page }) => {
  await workspace(page);
  await expect(page.locator('.broadcast-panel, [id^=broadcast-], #clear-selection, #selected-count, .select-session, .terminal-header input[type=checkbox]')).toHaveCount(0);
  await tab(page, 'Alpha').click();
  await card(page, 'a1').locator('.xterm-helper-textarea').press('x');
  await page.keyboard.insertText('한글😀');
  await tab(page, 'Beta').click();
  await card(page, 'b1').locator('.xterm-helper-textarea').press('b');
  await expect.poll(() => page.evaluate(() => (window as any).tabsFixture.calls.filter((call: any) => call.command === 'session_write').map((call: any) => call.args))).toEqual([
    { id: 'a1', generation: 7, data: 'x' },
    { id: 'a1', generation: 7, data: '한글😀' },
    { id: 'b1', generation: 7, data: 'b' },
  ]);
});

test('selected project supplies new-session and catalog scope, import creates a selected persistent tab', async ({ page }) => {
  await workspace(page);
  await tab(page, 'Beta').click();
  await page.locator('#new-session').click();
  await expect(page.locator('#session-cwd')).toHaveValue(projects[1].cwd);
  await page.getByRole('button', { name: '취소', exact: true }).click();
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-project')).toHaveValue(projects[1].cwd);
  await expect.poll(() => page.evaluate(() => (window as any).tabsFixture.calls.filter((call: any) => call.command === 'catalog_list').at(-1)?.args.request.cwd)).toBe(projects[1].cwd);
  await page.keyboard.press('Escape');
  await page.locator('#project-import').click();
  await expect(page.locator('#catalog-dialog')).toBeVisible();
  await expect(tab(page, 'Imported')).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('Escape');
  await expect(visibleCards(page)).toHaveCount(0);
  await page.locator('#empty-add').click();
  await expect(page.locator('#session-cwd')).toHaveValue('D:\\Work\\Imported');
  await page.getByRole('button', { name: '취소', exact: true }).click();
  await page.reload();
  await expect(tab(page, 'Imported')).toBeVisible();
  await page.locator('#projects .project-row').filter({ hasText: 'Beta' }).click();
  await expect(tab(page, 'Beta')).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('#catalog-dialog')).toBeHidden();
  await expect.poll(() => visibleIds(page)).toEqual(['b1']);
});

test('project tabs offer arrow, Home and End keyboard selection with one roving focus target', async ({ page }) => {
  await workspace(page);
  const tabs = page.locator('#project-tabs [role=tab]');
  await tabs.first().focus();
  await page.keyboard.press('ArrowRight');
  await expect(tab(page, 'Alpha')).toHaveAttribute('aria-selected', 'true');
  await expect(tab(page, 'Alpha')).toBeFocused();
  await page.keyboard.press('ArrowRight');
  await expect(tab(page, 'Beta')).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('End');
  await expect(tabs.last()).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('Home');
  await expect(tabs.first()).toHaveAttribute('aria-selected', 'true');
  await expect(tabs.first()).toBeFocused();
  await expect(page.locator('#project-tabs [role=tab][tabindex="0"]')).toHaveCount(1);
});

test('a delayed start cannot steal the project selected after its create popup closes', async ({ page }) => {
  await workspace(page);
  await tab(page, 'Alpha').click();
  await page.evaluate(() => { (window as any).tabsFixture.holdStart = true; });
  await page.locator('#new-session').click();
  await page.locator('#session-title').fill('Delayed Alpha');
  await page.locator('#create-submit').click();
  await expect.poll(() => page.evaluate(() => (window as any).tabsFixture.calls.some((call: any) => call.command === 'start_session'))).toBe(true);
  await page.locator('#create-dialog [data-close]').first().click();
  await tab(page, 'Beta').click();
  await page.evaluate(() => (window as any).tabsFixture.releaseStart());
  await expect(card(page, 'started-alpha')).toHaveCount(1);
  await expect(card(page, 'started-alpha')).toBeHidden();
  await expect(tab(page, 'Beta')).toHaveAttribute('aria-selected', 'true');
  await expect(card(page, 'b1')).toHaveClass(/focused/);
});

for (const back of [false, true]) test(`queued direct input stays cancelled after switching ${back ? 'away and back' : 'away'}`, async ({ page }) => {
  await workspace(page);
  await tab(page, 'Alpha').click();
  await page.evaluate(() => { (window as any).tabsFixture.holdNextWrite = true; });
  await card(page, 'a1').locator('.xterm-helper-textarea').press('x');
  await expect.poll(() => page.evaluate(() => (window as any).tabsFixture.calls.filter((call: any) => call.command === 'session_write').length)).toBe(1);
  await card(page, 'a1').locator('.xterm-helper-textarea').press('y');
  await tab(page, 'Beta').click();
  if (back) await tab(page, 'Alpha').click();
  await page.evaluate(() => (window as any).tabsFixture.releaseWrite());
  await expect(card(page, 'a1')).toHaveAttribute('data-input-pending', '0');
  const writes = await page.evaluate(() => (window as any).tabsFixture.calls.filter((call: any) => call.command === 'session_write').map((call: any) => call.args.data));
  expect(writes).toEqual(['x']);
});

for (const ownedThread of [false, true]) test(`late catalog ${ownedThread ? 'owned refresh' : 'new start'} cannot change the project chosen after close`, async ({ page }) => {
  await workspace(page);
  await tab(page, 'Alpha').click();
  await page.evaluate(ownedThread => { const fixture = (window as any).tabsFixture; fixture.catalog = true; fixture.catalogOwned = ownedThread; fixture.holdStart = !ownedThread; fixture.holdOwnership = ownedThread; }, ownedThread);
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-resume')).toBeEnabled();
  await page.locator('#catalog-resume').click();
  await expect.poll(() => page.evaluate(ownedThread => (window as any).tabsFixture.calls.filter((call: any) => call.command === (ownedThread ? 'list_sessions' : 'start_session')).length, ownedThread)).toBe(ownedThread ? 2 : 1);
  await page.locator('#catalog-close').click();
  await tab(page, 'Beta').click();
  await page.evaluate(ownedThread => { const fixture = (window as any).tabsFixture; ownedThread ? fixture.releaseOwnership() : fixture.releaseStart(); }, ownedThread);
  if (!ownedThread) await expect(card(page, 'started-alpha')).toHaveCount(1);
  await expect(tab(page, 'Beta')).toHaveAttribute('aria-selected', 'true');
  await expect(card(page, 'b1')).toHaveClass(/focused/);
  await expect.poll(() => visibleIds(page)).toEqual(['b1']);
  expect(await page.evaluate(() => (window as any).tabsFixture.calls.filter((call: any) => call.command === 'start_session').length)).toBe(ownedThread ? 0 : 1);
});

test('global capacity includes 31 hidden sessions and a pending start when an empty tab requests slot 33', async ({ page }) => {
  await workspace(page, { capacity: true });
  await expect(page.locator('.terminal-card')).toHaveCount(31);
  await page.evaluate(() => { (window as any).tabsFixture.holdStart = true; });
  await page.locator('#restore-open').click();
  await page.locator('#restore-list').getByRole('button', { name: '이 대화 재개 →' }).click();
  await expect.poll(() => page.evaluate(() => (window as any).tabsFixture.calls.filter((call: any) => call.command === 'start_session').length)).toBe(1);
  await page.locator('#restore-dialog [data-close]').click();
  await tab(page, 'Beta').click();
  await expect(visibleCards(page)).toHaveCount(0);
  await page.locator('#new-session').click();
  await page.locator('#session-title').fill('Must refuse slot33');
  await page.locator('#create-submit').click();
  await expect(page.locator('#create-error')).toContainText('최대 32개');
  expect(await page.evaluate(() => (window as any).tabsFixture.calls.filter((call: any) => call.command === 'start_session').length)).toBe(1);
  await page.getByRole('button', { name: '취소', exact: true }).click();
  await page.evaluate(() => (window as any).tabsFixture.releaseStart());
  await expect(page.locator('.terminal-card')).toHaveCount(32);
  await expect(visibleCards(page)).toHaveCount(0);
  await expect(tab(page, 'Beta')).toHaveAttribute('aria-selected', 'true');
});

async function createProject(page: Page, name = '논리 프로젝트') {
  await page.locator('#project-create').click();
  await page.locator('#project-name').fill(name);
  await page.locator('#project-save').click();
  await expect(page.locator('#project-dialog')).toBeHidden();
  return page.locator('#project-tabs [data-project-key="id:project-3"]');
}

test('project folder selection and cancellation never import a project; folderless creation supplies explicit session membership', async ({ page }) => {
  await workspace(page);
  await page.locator('#project-create').click();
  await page.evaluate(() => { (window as any).tabsFixture.chosenFolder = 'D:\\한글 작업\\테스트'; });
  await page.locator('#project-folder-choose').click();
  await expect(page.locator('#project-folder')).toHaveValue('D:\\한글 작업\\테스트');
  await page.locator('#project-dialog-close').click();
  expect(await page.evaluate(() => (window as any).tabsFixture.calls.filter((item: any) => ['create_project', 'choose_project'].includes(item.command)))).toEqual([]);
  const created = await createProject(page);
  await expect(created).toHaveAttribute('aria-selected', 'true');
  await expect(visibleCards(page)).toHaveCount(0);
  await page.locator('#new-session').click();
  await expect(page.locator('#session-cwd')).toHaveValue('');
  await page.locator('#session-title').fill('폴더와 묶음은 별개');
  await page.locator('#session-cwd').fill('D:\\별도 실행 폴더');
  await page.locator('#create-submit').click();
  await expect(card(page, 'started-alpha')).toBeVisible();
  const request = await page.evaluate(() => (window as any).tabsFixture.calls.find((item: any) => item.command === 'start_session').args.request);
  expect(request).toMatchObject({ projectId: 'project-3', cwd: 'D:\\별도 실행 폴더' });
  await expect(created).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('.terminal-card')).toHaveCount(7);
});

test('project names are text, favorites sort first and repeated management keeps one dialog', async ({ page }) => {
  await workspace(page);
  const name = '<img src=x onerror="window.injected=true"> 묶음';
  await createProject(page, name);
  const manage = page.locator('#projects [data-project-id="project-3"]');
  await manage.click();
  await page.locator('#project-name').fill('이름 변경');
  await page.locator('#project-favorite').check();
  await page.locator('#project-save').click();
  const tabs = page.locator('#project-tabs [role=tab]');
  await expect(tabs.nth(1)).toContainText('이름 변경');
  await expect(tabs.nth(1)).toContainText('★');
  await expect(tabs.nth(1)).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('#projects img')).toHaveCount(0);
  expect(await page.evaluate(() => (window as any).injected)).toBeUndefined();
  for (let attempt = 0; attempt < 3; attempt++) { await manage.click(); await page.locator('#project-dialog-close').click(); }
  await expect(page.locator('#project-dialog')).toHaveCount(1);
  await expect(page.locator('#project-delete-dialog')).toHaveCount(1);
  await page.locator('#settings-open').click();
  await page.locator('#language-select').selectOption('en');
  await page.locator('#settings-close').click();
  await manage.click();
  await expect(page.locator('#project-dialog-title')).toHaveText('Manage project');
  await expect(page.getByLabel('Project name', { exact: true })).toHaveValue('이름 변경');
  await expect(page.locator('#project-delete')).toHaveText('Delete project');
});

test('moving a running session preserves its terminal, generation and actual cwd; explicit unassigned wins over cwd', async ({ page }) => {
  await workspace(page);
  await createProject(page);
  await page.locator('#project-tabs [role=tab]').first().click();
  await page.evaluate(() => { (window as any).tabsFixture.movedNode = document.querySelector('.terminal-card[data-id="a1"] .xterm'); });
  await card(page, 'a1').getByLabel('프로젝트로 이동', { exact: true }).click();
  await page.locator('#project-move-target').selectOption('project-3');
  await page.locator('#project-move-confirm').click();
  await expect(card(page, 'a1')).toBeVisible();
  await expect(page.locator('#project-tabs [data-project-key="id:project-3"]')).toHaveAttribute('aria-selected', 'true');
  await expect(card(page, 'a1').locator('.terminal-path')).toContainText(owned[0].cwd);
  expect(await page.evaluate(() => (window as any).tabsFixture.movedNode === document.querySelector('.terminal-card[data-id="a1"] .xterm'))).toBe(true);
  await card(page, 'a1').getByLabel('프로젝트로 이동', { exact: true }).click();
  await page.locator('#project-move-target').selectOption('');
  await page.locator('#project-move-confirm').click();
  await expect(page.locator('#project-tabs [data-project-key="unassigned"]')).toHaveAttribute('aria-selected', 'true');
  await tab(page, 'Alpha').click();
  await expect.poll(() => visibleIds(page)).toEqual(['a2']);
  const calls = await page.evaluate(() => (window as any).tabsFixture.calls);
  expect(calls.filter((item: any) => item.command === 'move_session_to_project').map((item: any) => item.args)).toEqual([{ id: 'a1', generation: 7, projectId: 'project-3' }, { id: 'a1', generation: 7, projectId: null }]);
  expect(calls.filter((item: any) => ['session_stop', 'start_session', 'session_write'].includes(item.command))).toEqual([]);
  await expect.poll(() => page.evaluate(() => (window as any).tabsFixture.savedLayout?.sessions.find((item: any) => item.threadId === 'thread-a1'))).toMatchObject({ projectId: null, projectAssigned: true, cwd: owned[0].cwd });
  await page.reload();
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await tab(page, 'Alpha').click();
  await expect.poll(() => visibleIds(page)).toEqual(['a2']);
});

test('project deletion requires confirmation and preserves running sessions, code paths and conversations', async ({ page }) => {
  await workspace(page);
  await tab(page, 'Alpha').click();
  await page.locator('#projects [data-project-id="alpha"]').click();
  await page.locator('#project-delete').click();
  await expect(page.locator('#project-delete-dialog')).toContainText('코드 폴더와 대화는 보존');
  await page.locator('#project-delete-cancel').click();
  expect(await page.evaluate(() => (window as any).tabsFixture.calls.filter((item: any) => item.command === 'delete_project'))).toEqual([]);
  await page.locator('#project-delete').click();
  await page.locator('#project-delete-confirm').click();
  await expect(page.locator('#project-delete-dialog')).toBeHidden();
  await expect(page.locator('#project-tabs [data-project-key="id:alpha"]')).toHaveCount(0);
  await expect(page.locator('#project-tabs [role=tab]').first()).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('.terminal-card')).toHaveCount(6);
  await page.locator('#project-tabs [data-project-key="unassigned"]').click();
  await expect.poll(() => visibleIds(page)).toEqual(['a1', 'a2']);
  const calls = await page.evaluate(() => (window as any).tabsFixture.calls);
  expect(calls.filter((item: any) => item.command === 'delete_project').map((item: any) => item.args)).toEqual([{ id: 'alpha' }]);
  expect(calls.filter((item: any) => ['session_stop', 'start_session', 'session_write', 'delete_conversation', 'delete_folder'].includes(item.command))).toEqual([]);
  const registry = await page.evaluate(() => JSON.parse(localStorage.getItem('tabs-sessions')!));
  expect(registry.find((item: any) => item.id === 'a1')).toMatchObject({ cwd: owned[0].cwd, generation: 7, active: true, threadId: 'thread-a1', projectId: null, projectAssigned: true });
});

test('retry after a favorite save failure updates the created project without duplicating it', async ({ page }) => {
  await workspace(page);
  await page.locator('#project-create').click();
  await page.locator('#project-name').fill('부분 저장 재시도');
  await page.locator('#project-favorite').check();
  await page.evaluate(() => { (window as any).tabsFixture.failFavoriteOnce = true; });
  await page.locator('#project-save').click();
  await expect(page.locator('#project-error')).toContainText('즐겨찾기 저장 실패');
  await page.locator('#project-save').click();
  await expect(page.locator('#project-dialog')).toBeHidden();
  await expect(page.locator('#project-tabs [role=tab]').nth(1)).toContainText('★ 부분 저장 재시도');
  expect(await page.evaluate(() => (window as any).tabsFixture.calls.filter((item: any) => item.command === 'create_project').length)).toBe(1);
});

test('a delayed status snapshot cannot reverse a completed project move', async ({ page }) => {
  await workspace(page);
  await tab(page, 'Alpha').click();
  await card(page, 'a1').getByLabel('프로젝트로 이동', { exact: true }).click();
  await page.locator('#project-move-target').selectOption('beta');
  await page.locator('#project-move-confirm').click();
  await expect(tab(page, 'Beta')).toHaveAttribute('aria-selected', 'true');
  await expect(card(page, 'a1')).toBeVisible();
  await page.evaluate(session => (window as any).tabsFixture.emit('session-changed', { ...session, state: 'waiting', projectRevision: 0 }), owned[0]);
  await expect(card(page, 'a1')).toBeVisible();
  await expect(card(page, 'a1').locator('.session-state')).toContainText('응답 대기');
  await tab(page, 'Alpha').click();
  await expect.poll(() => visibleIds(page)).toEqual(['a2']);
  await expect.poll(() => page.evaluate(() => (window as any).tabsFixture.savedLayout?.sessions.find((item: any) => item.threadId === 'thread-a1'))).toMatchObject({ projectId: 'beta', projectAssigned: true, projectRevision: 1 });
});


test('an older catalog registry read cannot remove a project created while that read was pending', async ({ page }) => {
  await workspace(page);
  await page.evaluate(() => { (window as any).tabsFixture.holdNextProjects = true; });
  await page.locator('#catalog-open').click();
  await expect.poll(() => page.evaluate(() => typeof (window as any).tabsFixture.releaseProjects)).toBe('function');
  await page.locator('#catalog-close').click();
  const created = await createProject(page);
  await expect(created).toHaveAttribute('aria-selected', 'true');
  await page.evaluate(() => (window as any).tabsFixture.releaseProjects());
  await expect(created).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('#projects [data-project-id="project-3"]')).toBeVisible();
});

for (const scenario of ['return', 'hidden-output', 'move', 'same-project'] as const) test(`project selection shows the latest terminal conversation after ${scenario}`, async ({ page }) => {
  await workspace(page);
  await tab(page, 'Alpha').click();
  const rows = card(page, 'a1').locator('.xterm-rows');
  await page.evaluate(() => {
    const fixture = (window as any).tabsFixture;
    fixture.latestNode = document.querySelector('.terminal-card[data-id="a1"] .xterm');
    fixture.emit('terminal-output', { id: 'a1', generation: 7, seq: 1, data: Array.from({ length: 120 }, (_, index) => `HISTORY_${String(index).padStart(3, '0')}\r\n`).join('') + 'LATEST_BEFORE' });
  });
  await expect(rows).toContainText('LATEST_BEFORE');
  await card(page, 'a1').locator('.xterm-helper-textarea').press('Shift+PageUp');
  await expect(rows).toContainText('HISTORY_');
  await expect(rows).not.toContainText('LATEST_BEFORE');
  let latest = 'LATEST_BEFORE';
  if (scenario === 'return' || scenario === 'hidden-output') {
    await tab(page, 'Beta').click();
    if (scenario === 'hidden-output') {
      latest = 'LATEST_WHILE_HIDDEN';
      await page.evaluate(() => (window as any).tabsFixture.emit('terminal-output', { id: 'a1', generation: 7, seq: 2, data: '\r\n' + 'HIDDEN_HISTORY\r\n'.repeat(80) + 'LATEST_WHILE_HIDDEN' }));
      await expect.poll(() => page.evaluate(() => (window as any).tabsFixture.calls.some((call: any) => call.command === 'session_output_ack' && call.args.id === 'a1' && call.args.seq === 2))).toBe(true);
    }
    await tab(page, 'Alpha').click();
  } else {
    await card(page, 'a1').getByLabel('프로젝트로 이동', { exact: true }).click();
    await page.locator('#project-move-target').selectOption(scenario === 'move' ? 'beta' : 'alpha');
    await page.locator('#project-move-confirm').click();
    await expect(page.locator('#project-move-dialog')).toBeHidden();
  }
  await expect(rows).toContainText(latest);
  expect(await page.evaluate(() => (window as any).tabsFixture.latestNode === document.querySelector('.terminal-card[data-id="a1"] .xterm'))).toBe(true);
  expect(await page.evaluate(() => (window as any).tabsFixture.calls.filter((call: any) => ['start_session', 'session_stop', 'session_write', 'catalog_preview'].includes(call.command)))).toEqual([]);
});


test('project activity lights follow live ownership independently of the selected tab and stale events', async ({ page }) => {
  await workspace(page);
  const light = (name: string) => tab(page, name).locator('.project-activity');
  await expect(light('Alpha')).toHaveClass(/is-active/);
  await expect(light('Beta')).toHaveClass(/is-active/);
  await expect(light('Empty')).not.toHaveClass(/is-active/);
  await expect(light('Alpha')).toHaveAttribute('aria-label', '활성 세션 있음');
  await tab(page, 'Empty').click();
  await expect(tab(page, 'Empty')).toHaveAttribute('aria-selected', 'true');
  await expect(light('Empty')).not.toHaveClass(/is-active/);
  await expect(light('Alpha')).toHaveClass(/is-active/);
  const change = async (index: number, active: boolean, generation = 7) => page.evaluate(({ session, active, generation }) => (window as any).tabsFixture.emit('session-changed', { ...session, active, generation, state: active ? 'idle' : 'exited' }), { session: owned[index], active, generation });
  await change(0, false);
  await expect(light('Alpha')).toHaveClass(/is-active/);
  await change(2, false);
  await expect(light('Alpha')).not.toHaveClass(/is-active/);
  await expect(light('Alpha')).toHaveAttribute('aria-label', '활성 세션 없음');
  await change(2, true);
  await expect(light('Alpha')).not.toHaveClass(/is-active/);
  await change(2, true, 8);
  await expect(light('Alpha')).toHaveClass(/is-active/);
  for (let index = 0; index < owned.length; index++) await change(index, false, index === 2 ? 8 : 7);
  await expect(page.locator('#project-tabs .project-activity.is-active')).toHaveCount(0);
  await page.locator('#settings-open').click();
  await page.locator('#language-select').selectOption('en');
  await expect(light('Alpha')).toHaveAttribute('aria-label', 'No active sessions');
});

test('moving active sessions updates project activity lights without restarting their processes', async ({ page }) => {
  await workspace(page);
  await tab(page, 'Alpha').click();
  for (const id of ['a1', 'a2']) {
    if (id === 'a2') await tab(page, 'Alpha').click();
    await card(page, id).getByLabel('프로젝트로 이동', { exact: true }).click();
    await page.locator('#project-move-target').selectOption('empty');
    await page.locator('#project-move-confirm').click();
  }
  await expect(tab(page, 'Alpha').locator('.project-activity')).not.toHaveClass(/is-active/);
  await expect(tab(page, 'Empty').locator('.project-activity')).toHaveClass(/is-active/);
  await expect(page.locator('#project-tabs [role=tab]').first().locator('.project-activity')).toHaveClass(/is-active/);
  expect(await page.evaluate(() => (window as any).tabsFixture.calls.filter((call: any) => ['start_session', 'session_stop', 'session_write'].includes(call.command)))).toEqual([]);
});


test('session ID copy uses the latest full thread ID with availability and clipboard failure feedback', async ({ page, context }, testInfo) => {
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  await workspace(page);
  const copy = (id: string) => card(page, id).locator('.session-id-copy');
  await expect(copy('a1')).toHaveAccessibleName('세션 ID 복사');
  await copy('a1').click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe('thread-a1');
  await expect(page.locator('#toast-area')).toContainText('세션 ID를 복사했습니다.');
  await copy('b1').click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe('thread-b1');
  await page.evaluate(session => (window as any).tabsFixture.emit('session-changed', { ...session, threadId: undefined }), owned[0]);
  await expect(copy('a1')).toBeDisabled();
  const fullId = '01234567-89ab-7cde-8fab-0123456789ab';
  await page.evaluate(({ session, fullId }) => (window as any).tabsFixture.emit('session-changed', { ...session, generation: 8, threadId: fullId }), { session: owned[0], fullId });
  await expect(copy('a1')).toBeEnabled();
  await page.locator('#settings-open').click();
  await page.locator('#language-select').selectOption('en');
  await page.locator('#settings-close').click();
  await expect(copy('a1')).toHaveAccessibleName('Copy session ID');
  await copy('a1').click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe(fullId);
  await expect(page.locator('#toast-area')).toContainText('Session ID copied.');
  await page.evaluate(({ session, fullId }) => (window as any).tabsFixture.emit('session-changed', { ...session, generation: 8, threadId: fullId, active: false, state: 'exited' }), { session: owned[0], fullId });
  await expect(copy('a1')).toBeEnabled();
  await page.evaluate(() => { navigator.clipboard.writeText = async () => { throw new Error('PRIVATE_CLIPBOARD_FAILURE'); }; });
  await copy('a1').click();
  await expect(page.locator('#toast-area')).toContainText('Could not copy the session ID. Try again.');
  await expect(page.locator('#toast-area')).not.toContainText('PRIVATE_CLIPBOARD_FAILURE');
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe(fullId);
  await page.screenshot({ path: testInfo.outputPath('session-id-copy.png') });
});


test('세션 상단은 확인된 압축 진행 상태만 표시하고 완료·단절·새 세대에서 기존 상태로 돌아온다', async ({ page }, testInfo) => {
  await workspace(page);
  const a = card(page, 'a1');
  const badge = a.locator('.session-compaction');
  const state = a.locator('.session-state');
  const refresh = () => a.locator('.context-usage').click();
  await expect(state).toHaveText('입력 가능');
  await page.evaluate(() => { (window as any).tabsFixture.contextSnapshots = { a1: { revision: 10, connected: true, guiConnected: true, tokenUsageFresh: false, compacting: true, active: true, canCompact: false } }; });
  await refresh();
  await expect(badge).toBeVisible();
  await expect(badge).toHaveText('압축 중');
  await expect(badge).toHaveAttribute('role', 'status');
  await expect(state).toBeHidden();
  await expect(card(page, 'b1').locator('.session-compaction')).toBeHidden();
  await expect(card(page, 'b1').locator('.session-state')).toHaveText('입력 가능');
  await page.evaluate(session => (window as any).tabsFixture.emit('session-changed', { ...session, state: 'waiting' }), owned[0]);
  await expect(state).toBeHidden();
  await expect(badge).toHaveText('압축 중');
  await page.screenshot({ path: testInfo.outputPath('session-compacting.png') });
  await page.evaluate(async () => { const { setLanguage } = await import('/src/i18n.ts'); setLanguage('en'); });
  await expect(badge).toHaveText('Compacting');
  await page.evaluate(() => { const s = (window as any).tabsFixture.contextSnapshots.a1; s.revision++; s.compacting = false; });
  await refresh();
  await expect(badge).toBeHidden();
  await expect(state).toBeVisible();
  await expect(state).toHaveText('Waiting for response');
  await page.evaluate(() => { const s = (window as any).tabsFixture.contextSnapshots.a1; s.revision++; s.compacting = true; });
  await refresh();
  await expect(badge).toBeVisible();
  await page.evaluate(() => { (window as any).tabsFixture.contextFailure = true; });
  await refresh();
  await expect(badge).toBeHidden();
  await expect(state).toBeVisible();
  await page.evaluate(() => { const f = (window as any).tabsFixture; f.contextFailure = false; const s = f.contextSnapshots.a1; s.revision++; s.connected = false; });
  await refresh();
  await expect(badge).toBeHidden();
  await page.evaluate(() => { const s = (window as any).tabsFixture.contextSnapshots.a1; s.revision++; s.connected = true; });
  await refresh();
  await expect(badge).toBeVisible();
  await page.evaluate(session => { const f = (window as any).tabsFixture; delete f.contextSnapshots.a1; f.emit('session-changed', { ...session, generation: 8 }); }, owned[0]);
  await expect(badge).toBeHidden();
  await expect(state).toHaveText('Ready');
  expect(await page.evaluate(() => (window as any).tabsFixture.calls.filter((c: any) => c.command === 'conversation_request' && c.args.request.type !== 'contextStatus'))).toEqual([]);
});


test('Windows 창은 내부 드래그를 허용하고 점 손잡이 이동이 프로젝트 범위·저장·터미널을 보존한다', async ({ page }) => {
  await workspace(page);
  await tab(page, 'Alpha').click();
  await page.evaluate(() => { (window as any).tabsFixture.nodes = [...document.querySelectorAll('.terminal-card')].map(node => [node, node.querySelector('.xterm')]); });
  await card(page, 'a1').locator('.drag-handle').dragTo(card(page, 'a2').locator('.terminal-header'));
  await expect.poll(() => visibleIds(page)).toEqual(['a2', 'a1']);
  await expect(page.locator('#session-list .session-link > span:nth-child(2)')).toHaveText(['Alpha second', 'Alpha first']);
  await expect.poll(() => page.evaluate(() => (window as any).tabsFixture.savedLayout?.sessions.map((item: any) => item.sessionId))).toEqual(['a2', 'b1', 'a1', 'parent', 'child', 'sibling']);
  expect(await page.evaluate(() => (window as any).tabsFixture.nodes.every(([node, terminal]: HTMLElement[]) => node.isConnected && node.querySelector('.xterm') === terminal))).toBe(true);
  await card(page, 'a2').locator('.drag-handle').dragTo(card(page, 'a1').locator('.terminal-header'));
  await expect.poll(() => visibleIds(page)).toEqual(['a1', 'a2']);
  const source = (await card(page, 'a1').locator('.drag-handle').boundingBox())!;
  const target = (await card(page, 'a2').locator('.terminal-header').boundingBox())!;
  await page.mouse.move(source.x + source.width / 2, source.y + source.height / 2);
  await page.mouse.down();
  await page.mouse.move(target.x + target.width / 2, target.y + target.height / 2, { steps: 8 });
  await page.keyboard.press('Escape');
  await page.mouse.up();
  await expect(page.locator('.terminal-card.dragging')).toHaveCount(0);
  await expect.poll(() => visibleIds(page)).toEqual(['a1', 'a2']);
  expect(await page.evaluate(() => (window as any).tabsFixture.calls.filter((call: any) => ['start_session', 'session_stop', 'session_write'].includes(call.command)))).toEqual([]);
  const config = JSON.parse(await readFile(new URL('../src-tauri/tauri.conf.json', import.meta.url), 'utf8'));
  expect(config.app.windows.find((window: any) => window.label === 'main').dragDropEnabled, 'Windows에서는 기본 파일 드래그 처리가 화면 내부 드래그를 가로채면 안 됩니다.').toBe(false);
});
