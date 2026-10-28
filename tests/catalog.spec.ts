import { test, expect, type Page } from '@playwright/test';

const alpha = { id: '01900000-0000-7000-8000-000000000001', title: 'API 구조 개선', cwd: 'D:\\프로젝트\\api', createdAt: 1700000000, updatedAt: 1700000200, status: 'idle' };
const beta = { ...alpha, id: '01900000-0000-7000-8000-000000000002', title: '회귀 테스트 점검', updatedAt: 1700000300 };

async function mockNative(page: Page, options: { holdSearch?: boolean; holdPreview?: boolean; holdStart?: boolean; failStartOnce?: boolean; owned?: boolean; ownedState?: string; saved?: boolean; pages?: boolean; titleOverride?: string } = {}) {
  await page.addInitScript(({ alpha, beta, options }) => {
    const state = window as any;
    state.isTauri = true;
    state.fixture = { calls: [], active: 0, maxActive: 0, chooseCount: 0, created: 0, releaseSearch: () => {}, releasePreview: () => {}, releaseStart: () => {} };
    let callback = 0;
    state.__TAURI_INTERNALS__ = {
      transformCallback: () => ++callback,
      unregisterCallback() {},
      async invoke(command: string, args: any = {}) {
        const fixture = state.fixture;
        fixture.calls.push({ command, args });
        if (command.startsWith('plugin:event|')) return callback;
        if (command === 'load_layout') return { version: 1, columns: 2, sessions: options.saved ? [{ title: alpha.title, cwd: alpha.cwd, threadId: alpha.id }] : [] };
        if (command === 'list_sessions') return options.owned ? [{ id: 'owned-alpha', generation: 1, title: alpha.title, cwd: alpha.cwd, threadId: alpha.id, state: options.ownedState ?? 'idle' }] : [];
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
              if (args.request.cursor) return { sessions: [beta] };
              const first = { ...alpha, title: options.titleOverride ?? alpha.title };
              return { sessions: options.pages ? [first] : [first, beta], nextCursor: options.pages ? 'next-list' : null };
            }
            if (options.holdPreview && args.threadId === alpha.id) await new Promise<void>(resolve => { fixture.releasePreview = resolve; });
            const text = args.threadId === alpha.id ? 'ALPHA ONLY' : '최근 답변 <img src=x onerror="window.injected=true">';
            return { messages: [{ id: args.threadId + (args.cursor || ''), role: 'assistant', text: args.cursor ? '이전 대화 내용' : text }], nextCursor: args.threadId === beta.id && !args.cursor ? 'older' : null };
          } finally { fixture.active--; }
        }
        if (command === 'start_session') {
          if (new TextEncoder().encode(args.request.title).byteLength > 512 || /\p{Cc}/u.test(args.request.title)) throw new Error('title must be at most 512 bytes without control characters');
          const created = ++fixture.created;
          if (options.failStartOnce && created === 1) throw new Error('실행 파일을 시작하지 못했습니다');
          if (options.holdStart && created === 1) await new Promise<void>(resolve => { fixture.releaseStart = resolve; });
          return { ...args.request, id: 'created-' + created, generation: 1, state: 'idle', threadId: args.request.threadId || '01900000-0000-7000-8000-000000000003' };
        }
        return null;
      },
    };
  }, { alpha, beta, options });
  await page.goto('/');
  await expect(page.locator('#connection')).toHaveText('로컬 연결됨');
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
    await page.locator('#catalog-open').click();
    await expect(page.locator('#catalog-messages')).toContainText('ALPHA ONLY');
    await page.locator('#catalog-resume').click();
    await expect(page.locator('#catalog-dialog')).toBeHidden();
    expect(await page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'start_session'))).toEqual([]);
    await expect(page.locator('.terminal-card')).toHaveCount(1);
    await expect(page.locator('.terminal-card[data-id="owned-alpha"]')).toHaveClass(/focused/);
  });
}

test('saved workspace and catalog share the owned-thread guard across repeated restore clicks', async ({ page }) => {
  await mockNative(page, { owned: true, ownedState: 'error', saved: true });
  for (let attempt = 0; attempt < 2; attempt++) {
    await page.locator('#restore-open').click();
    await page.locator('#restore-list').getByRole('button', { name: '이 대화 재개 →' }).click();
    expect(await page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'start_session'))).toEqual([]);
    await page.locator('#restore-dialog').getByRole('button', { name: '닫기', exact: true }).click();
  }
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-messages')).toContainText('ALPHA ONLY');
  await page.locator('#catalog-resume').click();
  await expect(page.locator('#catalog-dialog')).toBeHidden();
  expect(await page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'start_session'))).toEqual([]);
  await expect(page.locator('.terminal-card')).toHaveCount(1);
  await expect(page.locator('.terminal-card[data-id="owned-alpha"]')).toHaveClass(/focused/);
});

test('failed resume releases the pending thread so the same UUID can be retried', async ({ page }) => {
  await mockNative(page, { failStartOnce: true });
  await page.locator('#catalog-open').click();
  await expect(page.locator('#catalog-resume')).toBeEnabled();
  await page.locator('#catalog-resume').click();
  await expect(page.locator('#catalog-resume-error')).toContainText('실행 파일을 시작하지 못했습니다');
  await expect(page.locator('#catalog-resume')).toBeEnabled();
  await expect(page.locator('.terminal-card')).toHaveCount(0);
  await page.locator('#catalog-resume').click();
  await expect(page.locator('#catalog-dialog')).toBeHidden();
  await expect(page.locator('.terminal-card')).toHaveCount(1);
  const requests = await page.evaluate(() => (window as any).fixture.calls.filter((call: any) => call.command === 'start_session').map((call: any) => call.args.request));
  expect(requests).toHaveLength(2);
  expect(requests.map((request: any) => request.threadId)).toEqual([alpha.id, alpha.id]);
  expect(requests.map((request: any) => request.cwd)).toEqual([alpha.cwd, alpha.cwd]);
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
