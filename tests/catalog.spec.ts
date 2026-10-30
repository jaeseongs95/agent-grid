import { test, expect, type Page } from '@playwright/test';

const alpha = { id: '01900000-0000-7000-8000-000000000001', title: 'API 구조 개선', cwd: 'D:\\프로젝트\\api', createdAt: 1700000000, updatedAt: 1700000200, status: 'idle' };
const beta = { ...alpha, id: '01900000-0000-7000-8000-000000000002', title: '회귀 테스트 점검', updatedAt: 1700000300 };

async function mockNative(page: Page, options: { holdSearch?: boolean; holdPreview?: boolean; holdStart?: boolean; failStartOnce?: boolean; holdInitialEvent?: boolean; holdOwnershipRefresh?: boolean; holdFailureEvent?: boolean; failStatusRefresh?: boolean; finishStartEarly?: boolean; owned?: boolean; ownedState?: string; saved?: boolean; pages?: boolean; titleOverride?: string; historyPages?: number } = {}) {
  await page.addInitScript(({ alpha, beta, options }) => {
    const state = window as any;
    state.isTauri = true;
    state.fixture = { calls: [], events: [], active: 0, maxActive: 0, chooseCount: 0, created: 0, ownershipReads: 0, releaseOwnership: () => {}, releaseStarting: () => {}, releaseSearch: () => {}, releasePreview: () => {}, releaseStart: () => {}, releaseFailure: () => {} };
    let callback = 0;
    const callbacks = new Map<number, (event: any) => void>();
    const listeners = new Map<string, (event: any) => void>();
    state.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
    state.__TAURI_INTERNALS__ = {
      transformCallback(handler: (event: any) => void) { callbacks.set(++callback, handler); return callback; },
      unregisterCallback() {},
      async invoke(command: string, args: any = {}) {
        const fixture = state.fixture;
        fixture.calls.push({ command, args });
        if (command === 'plugin:event|listen') { listeners.set(args.event, callbacks.get(args.handler)!); return callback; }
        if (command.startsWith('plugin:event|')) return callback;
        if (command === 'get_ui_language') return 'ko';
        if (command === 'set_ui_language') return { show: args.language === 'en' ? 'Open OpenDIY Desktop' : 'OpenDIY Desktop 열기', exit: args.language === 'en' ? 'Exit…' : '종료…' };
        if (command === 'load_layout') return { version: 1, columns: 2, sessions: options.saved ? [{ title: alpha.title, cwd: alpha.cwd, threadId: alpha.id }] : [] };
        if (command === 'list_sessions') {
          if (fixture.created === 1 && options.failStartOnce) {
            if (fixture.ownershipReads++ > 0 && options.holdOwnershipRefresh) await new Promise<void>(resolve => { fixture.releaseOwnership = resolve; });
            if (options.failStatusRefresh) throw new Error('상태 조회 연결 오류');
            return [{ id: 'created-1', generation: 1, title: alpha.title, cwd: alpha.cwd, threadId: alpha.id, state: 'error', active: false }];
          }
          return options.owned ? [{ id: 'owned-alpha', generation: 1, title: alpha.title, cwd: alpha.cwd, threadId: alpha.id, state: options.ownedState ?? 'idle', active: true }] : [];
        }
        if (command === 'list_projects') return JSON.parse(localStorage.getItem('fixture-projects') || '[]');
        if (command === 'choose_project') {
          fixture.chooseCount++;
          if (fixture.chooseCount === 1) return null;
          if (fixture.chooseCount === 2) throw new Error('폴더를 읽을 수 없습니다');
          const project = { id: 'imported', name: '새 프로젝트', cwd: 'D:\\프로젝트\\새 프로젝트', createdAt: 1700000000 };
          localStorage.setItem('fixture-projects', JSON.stringify([project])); return project;
        }
        if (command === 'catalog_list' || command === 'catalog_preview') {
          fixture.active++; fixture.maxActive = Math.max(fixture.maxActive, fixture.active);
          try {
            if (command === 'catalog_list') {
              if (options.holdSearch && args.request.query === 'slow') await new Promise<void>(resolve => { fixture.releaseSearch = resolve; });
              if (args.request.cwd?.includes('새 프로젝트')) return { sessions: [], nextCursor: null };
              if (args.request.query === 'missing') return { sessions: [], nextCursor: null };
              if (args.request.query === 'failure') throw new Error('조회 연결 오류');
              if (args.request.query === 'latest') return { sessions: [{ ...beta, title: '최종 검색 결과' }] };
              if (options.historyPages) {
                const page = Number(args.request.cursor || 0);
                return { sessions: Array.from({ length: 50 }, (_, index) => ({ ...alpha, id: '01900000-0000-7000-8000-' + String(page * 50 + index + 1).padStart(12, '0'), title: `세션 ${page * 50 + index + 1}` })), nextCursor: page + 1 < options.historyPages ? String(page + 1) : null };
              }
              if (args.request.cursor) return { sessions: [beta] };
              const first = { ...alpha, title: options.titleOverride ?? alpha.title };
              return { sessions: options.pages ? [first] : [first, beta], nextCursor: options.pages ? 'next-list' : null };
            }
            if (options.holdPreview && args.threadId === alpha.id) await new Promise<void>(resolve => { fixture.releasePreview = resolve; });
            if (options.historyPages) {
              const page = Number(args.cursor || 0);
              return { messages: Array.from({ length: 50 }, (_, index) => ({ id: `${args.threadId}-${page}-${index}`, role: 'assistant', text: `PAGE ${page} MESSAGE ${index} ` + '한😀'.repeat(180) })), nextCursor: page + 1 < options.historyPages ? String(page + 1) : null };
            }
            const text = args.threadId === alpha.id ? 'ALPHA ONLY' : '최근 답변 <img src=x onerror="window.injected=true">';
            return { messages: [{ id: args.threadId + (args.cursor || ''), role: 'assistant', text: args.cursor ? '이전 대화 내용' : text }], nextCursor: args.threadId === beta.id && !args.cursor ? 'older' : null };
          } finally { fixture.active--; }
        }
        if (command === 'start_session') {
          if (new TextEncoder().encode(args.request.title).byteLength > 512 || /\p{Cc}/u.test(args.request.title)) throw new Error('title must be at most 512 bytes without control characters');
          const created = ++fixture.created;
          if (options.finishStartEarly) {
            const payload = { ...args.request, id: 'created-' + created, generation: 1, state: 'starting', active: true };
            listeners.get('session-changed')!({ event: 'session-changed', id: 1, payload });
            return { ...payload, state: 'exited', active: false };
          }
          if (options.failStartOnce && created === 1) {
            const emit = (sessionState: string, active: boolean) => {
              const payload = { ...args.request, id: 'created-1', generation: 1, state: sessionState, active };
              fixture.events.push(payload);
              listeners.get('session-changed')!({ event: 'session-changed', id: 1, payload });
            };
            fixture.releaseStarting = () => emit('starting', true);
            if (!options.holdInitialEvent) emit('starting', true);
            fixture.releaseFailure = () => emit('error', false);
            if (!options.holdFailureEvent) {
              emit('error', false);
              emit('idle', true); // An earlier status snapshot arrives after the cleanup event.
            }
            throw new Error('실행 파일을 시작하지 못했습니다');
          }
          if (options.holdStart && created === 1) await new Promise<void>(resolve => { fixture.releaseStart = resolve; });
          return { ...args.request, id: 'created-' + created, generation: 1, state: 'idle', active: true, threadId: args.request.threadId || '01900000-0000-7000-8000-000000000003' };
        }
        return null;
      },
    };
  }, { alpha, beta, options });
  await page.goto('/');
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
}

test('project import handles cancel/error and opens an empty filtered popup; registry survives reload', async ({ page }) => {
  await mockNative(page);
  await page.locator('#project-import').click();
  await expect(page.locator('#catalog-dialog')).toBeHidden();
  await page.locator('#project-import').click();
  await expect(page.locator('.toast.error')).toContainText('폴더를 읽을 수 없습니다');
  await page.locator('#project-import').click();
  await expect(page.locator('#catalog-dialog')).toBeVisible();
  await expect(page.locator('#catalog-project')).toHaveValue('D:\\프로젝트\\새 프로젝트');
  await expect(page.locator('#catalog-list-status')).toContainText('검색 결과가 없습니다');
  await page.locator('#catalog-new').click();
  await expect(page.locator('#create-dialog')).toBeVisible();
  await expect(page.locator('#session-cwd')).toHaveValue('D:\\프로젝트\\새 프로젝트');
  await expect(page.locator('#session-thread')).toHaveValue('');
  await page.getByRole('button', { name: '취소', exact: true }).click();
  await page.reload();
  await expect(page.locator('#projects .project-row')).toContainText('새 프로젝트');
  await expect(page.locator('.terminal-card')).toHaveCount(0);
  await page.locator('#projects .project-row').click();
  await expect(page.locator('#catalog-dialog')).toBeHidden();
  await expect(page.locator('#project-tabs [role=tab][aria-selected="true"]')).toContainText('새 프로젝트');
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-project')).toHaveValue('D:\\프로젝트\\새 프로젝트');
});

test('popup previews selection safely, loads older messages and resumes its exact identity', async ({ page }, testInfo) => {
  await mockNative(page, { holdPreview: true });
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-list [role=option]')).toHaveCount(2);
  await expect.poll(() => page.evaluate(() => (window as any).fixture.calls.some((call: any) => call.command === 'catalog_preview'))).toBe(true);
  await page.locator('#catalog-list [role=option]').nth(1).click();
  await page.evaluate(() => (window as any).fixture.releasePreview());
  await expect(page.locator('#catalog-session-title')).toHaveText(beta.title);
  await expect(page.locator('#catalog-messages')).toContainText('최근 답변 <img');
  await expect(page.locator('#catalog-messages')).not.toContainText('ALPHA ONLY');
  await expect(page.locator('#catalog-messages img')).toHaveCount(0);
  expect(await page.evaluate(() => (window as any).injected)).toBeUndefined();
  await page.locator('#catalog-preview-more').click();
  await expect(page.locator('#catalog-messages article').first()).toContainText('이전 대화 내용');
  await page.screenshot({ path: testInfo.outputPath('session-library.png'), fullPage: true });
  await page.locator('#catalog-resume').click();
  await expect(page.locator('#catalog-dialog')).toBeHidden();
  const requests = await page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'start_session'));
  expect(requests).toHaveLength(1);
  expect(requests[0].args.request).toMatchObject({ threadId: beta.id, cwd: beta.cwd, title: beta.title });
  expect(await page.evaluate(() => (window as any).fixture.maxActive)).toBe(1);
});

test('search coalesces intermediate queries and rejects stale results; close fences pending preview', async ({ page }) => {
  await mockNative(page, { holdSearch: true });
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-messages')).toContainText('ALPHA ONLY');
  await page.locator('#catalog-search').fill('slow');
  await expect.poll(() => page.evaluate(() => (window as any).fixture.calls.some((call: any) => call.args.request?.query === 'slow'))).toBe(true);
  await page.locator('#catalog-search').fill('middle');
  await page.waitForTimeout(300); // The production debounce is 250ms; the held IPC is released explicitly below.
  await page.locator('#catalog-search').fill('latest');
  await page.waitForTimeout(300);
  await page.evaluate(() => (window as any).fixture.releaseSearch());
  await expect(page.locator('#catalog-list')).toContainText('최종 검색 결과');
  await expect(page.locator('#catalog-list')).not.toContainText(alpha.title);
  const queries = await page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'catalog_list').map((call: any) => call.args.request.query));
  expect(queries).not.toContain('middle');
  expect(await page.evaluate(() => (window as any).fixture.maxActive)).toBe(1);
  await page.keyboard.press('Escape');
  await expect(page.locator('#catalog-dialog')).toBeHidden();
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-search')).toHaveValue('');
});

test('list paging, errors and keyboard selection stay inside the popup; an owned thread is focused', async ({ page }) => {
  await mockNative(page, { owned: true, pages: true });
  await expect(page.locator('.terminal-card')).toHaveCount(1);
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-more')).toBeVisible();
  await page.locator('#catalog-more').click();
  await expect(page.locator('#catalog-list [role=option]')).toHaveCount(2);
  await page.locator('#catalog-search').focus();
  await page.keyboard.press('ArrowDown');
  await expect(page.locator('#catalog-session-title')).toHaveText(beta.title);
  await page.keyboard.press('ArrowUp');
  await page.keyboard.press('Enter');
  await expect(page.locator('#catalog-dialog')).toBeHidden();
  await expect(page.locator('.terminal-card')).toHaveCount(1);
  expect(await page.evaluate(() => (window as any).fixture.created)).toBe(0);
  await page.locator('#catalog-open').click();
  await page.locator('#catalog-search').fill('failure');
  await expect(page.locator('#catalog-list-status')).toContainText('조회 연결 오류');
  await page.locator('#catalog-search').fill('missing');
  await expect(page.locator('#catalog-list-status')).toContainText('검색 결과가 없습니다');
  await expect(page.locator('#catalog-resume')).toBeDisabled();
});

test('Korean search composition keeps candidate keys inside the IME until committed', async ({ page }) => {
  await mockNative(page);
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-messages')).toContainText('ALPHA ONLY');
  const search = page.locator('#catalog-search');
  await search.evaluate((element: HTMLInputElement) => {
    element.focus(); element.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true }));
    element.value = '한글 검색';
    element.dispatchEvent(new InputEvent('input', { bubbles: true, data: '한글 검색', isComposing: true, inputType: 'insertCompositionText' }));
    for (const key of ['ArrowDown', 'Enter', 'Escape']) element.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, key, isComposing: true }));
  });
  await expect(page.locator('#catalog-dialog')).toBeVisible();
  await expect(page.locator('#catalog-session-title')).toHaveText(alpha.title);
  expect(await page.evaluate(() => (window as any).fixture.created)).toBe(0);
  expect(await page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'catalog_list').length)).toBe(1);
  await search.evaluate(element => element.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '한글 검색' })));
  await expect.poll(() => page.evaluate(() => (window as any).fixture.calls.some((call: any) => call.args.request?.query === '한글 검색'))).toBe(true);
  await expect(page.locator('#catalog-dialog')).toBeVisible();
});

for (const state of ['error', 'stopping'] as const) {
  test(`already owned ${state} session is focused without starting a duplicate`, async ({ page }) => {
    await mockNative(page, { owned: true, ownedState: state });
    await expect(page.locator('.terminal-card')).toHaveCount(1);
    await expect(page.locator('#running-count')).toHaveText('1');
    await page.locator('#catalog-open').click();
    await expect(page.locator('#catalog-messages')).toContainText('ALPHA ONLY');
    await page.locator('#catalog-resume').click();
    await expect(page.locator('#catalog-dialog')).toBeHidden();
    expect(await page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'start_session'))).toEqual([]);
    await expect(page.locator('.terminal-card')).toHaveCount(1);
    await expect(page.locator('.terminal-card[data-id="owned-alpha"]')).toHaveClass(/focused/);
    if (state === 'error') {
      await page.locator('.xterm-helper-textarea').press('x');
      await expect.poll(() => page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'session_write').length)).toBe(1);
    }
  });
}

test('saved workspace reconciles an already owned thread and catalog keeps the same guard', async ({ page }) => {
  await mockNative(page, { owned: true, ownedState: 'error', saved: true });
  await expect(page.locator('#restore-banner')).toBeHidden();
  expect(await page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'start_session'))).toEqual([]);
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-messages')).toContainText('ALPHA ONLY');
  await page.locator('#catalog-resume').click();
  await expect(page.locator('#catalog-dialog')).toBeHidden();
  expect(await page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'start_session'))).toEqual([]);
  await expect(page.locator('.terminal-card')).toHaveCount(1);
  await expect(page.locator('.terminal-card[data-id="owned-alpha"]')).toHaveClass(/focused/);
});

for (const holdFailureEvent of [false, true]) test(`failed resume events ${holdFailureEvent ? 'delayed until after rejection' : 'before rejection'} release active ownership so the same UUID can be retried`, async ({ page }) => {
  await mockNative(page, { failStartOnce: true, holdFailureEvent });
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-resume')).toBeEnabled();
  await page.locator('#catalog-resume').click();
  await expect(page.locator('#catalog-resume-error')).toContainText('실행 파일을 시작하지 못했습니다');
  await expect(page.locator('#catalog-resume')).toBeEnabled();
  await expect(page.locator('.terminal-card')).toHaveCount(1);
  await expect(page.locator('.terminal-card[data-id="created-1"] .session-state')).toHaveText('오류');
  expect(await page.evaluate(() => (window as any).fixture.events.map((event: any) => [event.state, event.active]))).toEqual(holdFailureEvent ? [['starting', true]] : [['starting', true], ['error', false], ['idle', true]]);
  await page.locator('#catalog-resume').click();
  await expect(page.locator('#catalog-dialog')).toBeHidden();
  const requests = await page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'start_session').map((call: any) => call.args.request));
  expect(requests).toHaveLength(2);
  expect(requests.map((request: any) => request.threadId)).toEqual([alpha.id, alpha.id]);
  expect(requests.map((request: any) => request.cwd)).toEqual([alpha.cwd, alpha.cwd]);
  await expect(page.locator('.terminal-card')).toHaveCount(2);
  await expect(page.locator('.terminal-card[data-id="created-2"]')).toHaveClass(/focused/);
  if (holdFailureEvent) await page.evaluate(() => (window as any).fixture.releaseFailure());
  await expect(page.locator('.terminal-card[data-id="created-1"] .session-state')).toHaveText('오류');
});

test('failed startup reports a failed authoritative refresh and preserves ownership until a cleanup event arrives', async ({ page }) => {
  await mockNative(page, { failStartOnce: true, holdFailureEvent: true, failStatusRefresh: true });
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-resume')).toBeEnabled();
  await page.locator('#catalog-resume').click();
  await expect(page.locator('#catalog-resume-error')).toContainText('실행 파일을 시작하지 못했습니다');
  await expect(page.locator('#catalog-resume-error')).toContainText('상태 조회 연결 오류');
  await expect(page.locator('.terminal-card[data-id="created-1"] .session-state')).toHaveText('시작 중');
  await page.locator('#catalog-resume').click();
  await expect(page.locator('#catalog-dialog')).toBeVisible();
  await expect(page.locator('#catalog-resume-error')).toContainText('실행 중 세션 확인 실패');
  expect(await page.evaluate(() => (window as any).fixture.created)).toBe(1);
  await page.evaluate(() => (window as any).fixture.releaseFailure());
  await expect(page.locator('#catalog-resume')).toBeEnabled();
  await page.locator('#catalog-resume').click();
  await expect(page.locator('#catalog-dialog')).toBeHidden();
  expect(await page.evaluate(() => (window as any).fixture.created)).toBe(2);
});

test('late starting after a rejected startup is checked against native ownership and that check is shared across popup reopen', async ({ page }) => {
  await mockNative(page, { failStartOnce: true, holdInitialEvent: true, holdFailureEvent: true, holdOwnershipRefresh: true });
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-resume')).toBeEnabled();
  await page.locator('#catalog-resume').click();
  await expect(page.locator('#catalog-resume-error')).toContainText('실행 파일을 시작하지 못했습니다');
  await expect(page.locator('.terminal-card')).toHaveCount(0);
  await page.evaluate(() => (window as any).fixture.releaseStarting());
  await expect(page.locator('.terminal-card[data-id="created-1"] .session-state')).toHaveText('시작 중');
  await page.locator('#catalog-resume').click();
  await expect.poll(() => page.evaluate(() => (window as any).fixture.ownershipReads)).toBe(2);
  await page.locator('#catalog-close').click();
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-resume')).toBeEnabled();
  await page.locator('#catalog-resume').click();
  expect(await page.evaluate(() => (window as any).fixture.ownershipReads)).toBe(2);
  expect(await page.evaluate(() => (window as any).fixture.created)).toBe(1);
  await page.evaluate(() => (window as any).fixture.releaseOwnership());
  await expect(page.locator('#catalog-dialog')).toBeHidden();
  const requests = await page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'start_session').map((call: any) => call.args.request));
  expect(requests).toHaveLength(2);
  expect(requests.map((request: any) => request.threadId)).toEqual([alpha.id, alpha.id]);
  expect(requests.map((request: any) => request.cwd)).toEqual([alpha.cwd, alpha.cwd]);
  await page.evaluate(() => (window as any).fixture.releaseFailure());
  await expect(page.locator('.terminal-card[data-id="created-1"] .session-state')).toHaveText('오류');
  await expect(page.locator('.terminal-card[data-id="created-2"]')).toHaveClass(/focused/);
});

test('a finished error card is excluded from live counts and cannot write or resize a reaped process', async ({ page }) => {
  await mockNative(page, { failStartOnce: true });
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-resume')).toBeEnabled();
  await page.locator('#catalog-resume').click();
  await expect(page.locator('#catalog-resume-error')).toBeVisible();
  await page.locator('#catalog-close').click();
  await expect(page.locator('#running-count')).toHaveText('0');
  await page.locator('.xterm-helper-textarea').press('x');
  await page.setViewportSize({ width: 1300, height: 900 });
  // Observe beyond the known 60ms resize debounce to prove no deferred native operation is sent.
  await page.waitForTimeout(120);
  expect(await page.evaluate(() => (window as any).fixture.calls.filter((call: any) => ['session_write', 'session_resize'].includes(call.command)))).toEqual([]);
});

test('a rapid exit in the start response finalizes an earlier starting event of the same generation', async ({ page }) => {
  await mockNative(page, { finishStartEarly: true });
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-resume')).toBeEnabled();
  await page.locator('#catalog-resume').click();
  await expect(page.locator('#catalog-dialog')).toBeHidden();
  await expect(page.locator('.terminal-card[data-id="created-1"] .session-state')).toHaveText('종료됨');
  await expect(page.locator('#running-count')).toHaveText('0');
});

test('pending resume is reused when the popup closes and reopens before startup completes', async ({ page }) => {
  await mockNative(page, { holdStart: true });
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-messages')).toContainText('ALPHA ONLY');
  await page.locator('#catalog-resume').click();
  await expect.poll(() => page.evaluate(() => (window as any).fixture.created)).toBe(1);
  await page.locator('#catalog-close').click();
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-messages')).toContainText('ALPHA ONLY');
  await page.locator('#catalog-resume').click();
  expect(await page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'start_session').length)).toBe(1);
  await page.evaluate(() => (window as any).fixture.releaseStart());
  await expect(page.locator('#catalog-dialog')).toBeHidden();
  await expect(page.locator('.terminal-card')).toHaveCount(1);
});

for (const [name, title, expectedTitle] of [
  ['long multiline Unicode', '\n\t' + '가🙂'.repeat(200) + '\u0085\r\n끝', '가🙂'.repeat(73)],
  ['control-only fallback', '\n\t\u0000\u0085\r', '이름 없는 세션'],
  ['513-byte ASCII truncation', 'x'.repeat(513), 'x'.repeat(512)],
  ['exact 512-byte Unicode unchanged', '가'.repeat(170) + 'ab', '가'.repeat(170) + 'ab'],
] as const) {
  test(`catalog title ${name} resumes within the native title boundary`, async ({ page }) => {
    await mockNative(page, { titleOverride: title });
    await page.locator('#catalog-open').click();
    await expect(page.locator('#catalog-list [role=option]')).toHaveCount(2);
    await page.locator('#catalog-resume').click();
    await expect(page.locator('#catalog-dialog')).toBeHidden();
    const request = await page.evaluate(() => (window as any).fixture.calls.find((call: any) => call.command === 'start_session').args.request);
    expect(request.threadId).toBe(alpha.id);
    expect(request.cwd).toBe(alpha.cwd);
    expect(new TextEncoder().encode(request.title).byteLength).toBeLessThanOrEqual(512);
    expect(request.title).not.toMatch(/\p{Cc}/u);
    expect(request.title.trim()).not.toBe('');
    expect(request.title).toBe(expectedTitle);
    expect(await page.evaluate(() => (window as any).fixture.created)).toBe(1);
  });
}


test('many explicit history pages retain a bounded window and refresh returns to recent records', async ({ page }) => {
  await mockNative(page, { historyPages: 12 });
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-list [role=option]')).toHaveCount(50);
  await expect(page.locator('#catalog-messages article')).toHaveCount(50);
  for (let index = 1; index < 8; index++) {
    await page.locator('#catalog-more').click();
    await expect.poll(() => page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'catalog_list').length)).toBe(index + 1);
    await expect(page.locator('#catalog-more')).toBeEnabled();
    await expect(page.locator('#catalog-list [role=option]')).toHaveCount(Math.min(250, (index + 1) * 50));
  }
  await expect(page.locator('#catalog-list [role=option]').first()).toContainText('세션 151');
  await expect(page.locator('#catalog-list [role=option]').last()).toContainText('세션 400');
  await expect(page.locator('#catalog-list-status')).toContainText('최대 250개');
  await page.locator('#catalog-refresh').click();
  await expect(page.locator('#catalog-list [role=option]').first()).toContainText('세션 1');
  await expect(page.locator('#catalog-messages article')).toHaveCount(50);
  for (let index = 1; index < 8; index++) {
    await page.locator('#catalog-preview-more').click();
    await expect(page.locator('#catalog-messages article').first()).toContainText(`PAGE ${index} MESSAGE 0`);
    await expect(page.locator('#catalog-messages article')).toHaveCount(100);
    const bytes = await page.locator('#catalog-messages').evaluate(element => [...element.querySelectorAll('p')].reduce((sum, message) => sum + new TextEncoder().encode(message.textContent ?? '').byteLength, 0));
    expect(bytes).toBeLessThanOrEqual(128 * 1024);
  }
  await expect(page.locator('#catalog-preview-status')).toContainText('최대 100개');
  await page.locator('#catalog-list [role=option]').first().click();
  await expect(page.locator('#catalog-messages article')).toHaveCount(50);
  await expect(page.locator('#catalog-messages article').first()).toContainText('PAGE 0 MESSAGE 0');
  expect(await page.evaluate(() => (window as any).fixture.maxActive)).toBe(1);
});

test('closing releases history nodes and selected text across repeated open and close cycles', async ({ page }, testInfo) => {
  await mockNative(page, { historyPages: 2 });
  const cdp = await page.context().newCDPSession(page);
  for (let index = 0; index < 25; index++) {
    await page.locator('#catalog-open').click();
    await expect(page.locator('#catalog-messages article')).toHaveCount(50);
    await page.evaluate(() => {
      const sample = ['#catalog-list [role=option]', '#catalog-messages article', '#catalog-metadata dd'].map(selector => new WeakRef(document.querySelector(selector)!));
      const state = window as any; state.historyWeakReferences ??= []; state.historyWeakReferences.push(...sample);
    });
    await page.locator('#catalog-close').click();
    await expect(page.locator('#catalog-list [role=option]')).toHaveCount(0);
    await expect(page.locator('#catalog-messages article')).toHaveCount(0);
    await expect(page.locator('#catalog-metadata')).toBeEmpty();
    await expect(page.locator('#catalog-session-title')).toBeEmpty();
    await page.evaluate(() => { (window as any).fixture.calls = []; });
  }
  await cdp.send('HeapProfiler.collectGarbage');
  await cdp.send('HeapProfiler.collectGarbage');
  const retained = await page.evaluate(() => (window as any).historyWeakReferences.filter((reference: WeakRef<Element>) => reference.deref() !== undefined).length);
  expect(retained).toBe(0);
  const metrics = await cdp.send('Runtime.getHeapUsage');
  await testInfo.attach('catalog-lifetime.json', { body: JSON.stringify({ cycles: 25, sampledNodes: 75, retainedSampledNodes: retained, heap: metrics, limitation: 'Renderer fixture and sampled node lifetime only; not a native-process or long-duration leak proof.' }, null, 2), contentType: 'application/json' });
  await cdp.detach();
});


test('a preview completing after close cannot restore released history or overtake the reopened selection', async ({ page }) => {
  await mockNative(page, { holdPreview: true });
  await page.locator('#catalog-open').click();
  await expect.poll(() => page.evaluate(() => (window as any).fixture.active)).toBe(1);
  await page.locator('#catalog-close').click();
  await expect(page.locator('#catalog-list [role=option]')).toHaveCount(0);
  await page.evaluate(() => (window as any).fixture.releasePreview());
  await expect.poll(() => page.evaluate(() => (window as any).fixture.active)).toBe(0);
  await expect(page.locator('#catalog-list [role=option]')).toHaveCount(0);
  await expect(page.locator('#catalog-messages article')).toHaveCount(0);
  await expect(page.locator('#catalog-metadata')).toBeEmpty();
  await page.locator('#catalog-open').click();
  await expect.poll(() => page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'catalog_preview').length)).toBe(2);
  await page.locator('#catalog-list [role=option]').nth(1).click();
  await page.evaluate(() => (window as any).fixture.releasePreview());
  await expect(page.locator('#catalog-session-title')).toHaveText(beta.title);
  await expect(page.locator('#catalog-messages')).toContainText('최근 답변 <img');
  await expect(page.locator('#catalog-messages')).not.toContainText('ALPHA ONLY');
  expect(await page.evaluate(() => (window as any).fixture.maxActive)).toBe(1);
});
