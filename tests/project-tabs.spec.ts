import { test, expect, type Page } from '@playwright/test';

const projects = [
  { id: 'alpha', name: 'Alpha', cwd: 'D:\\Work\\Alpha', createdAt: 1700000000 },
  { id: 'beta', name: 'Beta', cwd: 'D:\\Work\\Beta', createdAt: 1700000000 },
  { id: 'empty', name: 'Empty', cwd: 'D:\\Work\\Empty', createdAt: 1700000000 },
];
const owned = [
  { id: 'a1', title: 'Alpha first', cwd: 'd:/work/ALPHA/' },
  { id: 'b1', title: 'Beta first', cwd: projects[1].cwd },
  { id: 'a2', title: 'Alpha second', cwd: '\\\\?\\D:\\Work\\Alpha' },
  { id: 'parent', title: 'Parent session', cwd: 'D:\\Work' },
  { id: 'child', title: 'Child session', cwd: 'D:\\Work\\Alpha\\child' },
  { id: 'sibling', title: 'Sibling session', cwd: 'D:\\Work\\Alphabet' },
].map(session => ({ ...session, generation: 7, state: 'idle', active: true, threadId: `thread-${session.id}` }));

async function workspace(page: Page, options: { capacity?: boolean } = {}) {
  await page.addInitScript(({ projects, owned, options }) => {
    const state = window as any;
    state.isTauri = true;
    let callback = 0;
    const callbacks = new Map<number, (event: any) => void>();
    const listeners = new Map<string, (event: any) => void>();
    const existing = options.capacity ? Array.from({ length: 31 }, (_, index) => ({ ...owned[0], id: 'capacity-' + index, threadId: 'capacity-thread-' + index })) : owned;
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
        if (command === 'get_ui_language') return 'ko';
        if (command === 'set_ui_language') return { show: args.language === 'en' ? 'Open Agent Grid' : 'Agent Grid 열기', exit: args.language === 'en' ? 'Exit…' : '종료…' };
        if (command === 'load_layout') return { version: 1, columns: 2, sessions: options.capacity ? [{ title: 'Pending slot 32', cwd: projects[0].cwd, threadId: 'pending-slot32' }] : [] };
        if (command === 'list_sessions') {
          if (ownershipReads++ > 0 && state.tabsFixture.holdOwnership) await new Promise<void>(resolve => { state.tabsFixture.releaseOwnership = resolve; });
          return existing;
        }
        if (command === 'list_projects') return JSON.parse(localStorage.getItem('tabs-projects') || JSON.stringify(projects));
        if (command === 'choose_project') {
          const imported = { id: 'new', name: 'Imported', cwd: 'D:\\Work\\Imported', createdAt: 1700000000 };
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
          return { ...args.request, id: 'started-alpha', generation: 1, state: 'idle', active: true };
        }
        return null;
      },
    };
  }, { projects, owned, options });
  await page.goto('/');
  await expect(page.locator('#connection')).toHaveText('로컬 연결됨');
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

test('broadcast cannot send to a selection hidden by a project switch', async ({ page }) => {
  await workspace(page);
  await card(page, 'a1').getByLabel('선택 전송 대상', { exact: true }).check();
  await card(page, 'b1').getByLabel('선택 전송 대상', { exact: true }).check();
  await tab(page, 'Alpha').click();
  await page.locator('#broadcast-input').fill('alpha-only');
  await page.locator('#broadcast-send').click();
  await expect(page.locator('#broadcast-results')).toContainText('Alpha first: 전송됨');
  const writes = await page.evaluate(() => (window as any).tabsFixture.calls.filter((call: any) => call.command === 'session_write').map((call: any) => call.args));
  expect(writes).toEqual([{ id: 'a1', generation: 7, data: 'alpha-only\r' }]);
  await tab(page, 'Beta').click();
  await expect(page.locator('#broadcast-send')).toBeDisabled();
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

for (const back of [false, true]) test(`queued direct and broadcast input stays cancelled after switching ${back ? 'away and back' : 'away'}`, async ({ page }) => {
  await workspace(page);
  await tab(page, 'Alpha').click();
  await page.evaluate(() => { (window as any).tabsFixture.holdNextWrite = true; });
  await card(page, 'a1').locator('.xterm-helper-textarea').press('x');
  await expect.poll(() => page.evaluate(() => (window as any).tabsFixture.calls.filter((call: any) => call.command === 'session_write').length)).toBe(1);
  await card(page, 'a1').locator('.xterm-helper-textarea').press('y');
  await card(page, 'a1').getByLabel('선택 전송 대상', { exact: true }).check();
  await page.locator('#broadcast-input').fill('must-not-send');
  await page.locator('#broadcast-send').click();
  await tab(page, 'Beta').click();
  if (back) await tab(page, 'Alpha').click();
  await page.evaluate(() => (window as any).tabsFixture.releaseWrite());
  await expect(page.locator('#broadcast-results')).toContainText('실패');
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
  await page.locator('#restore-list button').click();
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
