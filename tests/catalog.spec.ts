import { test, expect, type Page } from '@playwright/test';

const alpha = { id: '01900000-0000-7000-8000-000000000001', title: 'API 구조 개선', cwd: 'D:\\프로젝트\\api', createdAt: 1700000000, updatedAt: 1700000200, status: 'idle' };
const beta = { ...alpha, id: '01900000-0000-7000-8000-000000000002', title: '회귀 테스트 점검', updatedAt: 1700000300 };

async function mockNative(page: Page, options: { holdSearch?: boolean; holdPreview?: boolean; owned?: boolean; pages?: boolean } = {}) {
  await page.addInitScript(({ alpha, beta, options }) => {
    const state = window as any;
    state.isTauri = true;
    state.fixture = { calls: [], active: 0, maxActive: 0, chooseCount: 0, created: 0, releaseSearch: () => {}, releasePreview: () => {} };
    let callback = 0;
    state.__TAURI_INTERNALS__ = {
      transformCallback: () => ++callback,
      unregisterCallback() {},
      async invoke(command: string, args: any = {}) {
        const fixture = state.fixture;
        fixture.calls.push({ command, args });
        if (command.startsWith('plugin:event|')) return callback;
        if (command === 'load_layout') return { version: 1, columns: 2, sessions: [] };
        if (command === 'list_sessions') return options.owned ? [{ id: 'owned-alpha', generation: 1, title: alpha.title, cwd: alpha.cwd, threadId: alpha.id, state: 'idle' }] : [];
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
              return { sessions: options.pages ? [alpha] : [alpha, beta], nextCursor: options.pages ? 'next-list' : null };
            }
            if (options.holdPreview && args.threadId === alpha.id) await new Promise<void>(resolve => { fixture.releasePreview = resolve; });
            const text = args.threadId === alpha.id ? 'ALPHA ONLY' : '최근 답변 <img src=x onerror="window.injected=true">';
            return { messages: [{ id: args.threadId + (args.cursor || ''), role: 'assistant', text: args.cursor ? '이전 대화 내용' : text }], nextCursor: args.threadId === beta.id && !args.cursor ? 'older' : null };
          } finally { fixture.active--; }
        }
        if (command === 'start_session') {
          fixture.created++;
          return { ...args.request, id: 'created-' + fixture.created, generation: 1, state: 'idle', threadId: args.request.threadId || '01900000-0000-7000-8000-000000000003' };
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
