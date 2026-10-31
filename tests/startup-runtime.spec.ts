import { test, expect, type Page } from '@playwright/test';

// 실제 화면/세션 시작 경계 검사. 준비·저장·세션 명령만 대체하며 실제 서버 실행은 검증하지 않는다.
const savedSessions = [
  { title: '이전 첫 작업', cwd: 'D:\\startup-test', threadId: 'saved-a', resumeOnStartup: true },
  { title: '이전 둘째 작업', cwd: 'D:\\startup-test', threadId: 'saved-b', resumeOnStartup: true },
];

async function workspace(page: Page, options: { hold?: boolean; fail?: number; holdRetry?: boolean; saved?: boolean } = {}) {
  await page.addInitScript(({ options, savedSessions }) => {
    const state = window as any; state.isTauri = true;
    let callback = 0;
    const callbacks = new Map<number, (value: any) => void>();
    const listeners = new Map<string, (value: any) => void>();
    const fixture = state.startupFixture = {
      calls: [] as { command: string; args: any }[], live: [] as any[], preparations: 0, preparing: 0, maxPreparing: 0,
      failures: options.fail ?? 0, release: () => {},
    };
    state.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
    state.__TAURI_INTERNALS__ = {
      transformCallback(handler: (value: any) => void) { callbacks.set(++callback, handler); return callback; }, unregisterCallback() {},
      async invoke(command: string, args: any = {}) {
        fixture.calls.push({ command, args });
        if (command === 'plugin:event|listen') { listeners.set(args.event, callbacks.get(args.handler)!); return callback; }
        if (command.startsWith('plugin:event|')) return callback;
        if (command === 'get_ui_language') return 'ko';
        if (command === 'list_projects') return [];
        if (command === 'list_sessions') return fixture.live;
        if (command === 'load_layout') return { version: 1, columns: 2, sessions: options.saved === false ? [] : savedSessions };
        if (command === 'save_layout') { localStorage.setItem('startup-layout', JSON.stringify(args.layout)); return null; }
        if (command === 'prepare_runtime') {
          const attempt = ++fixture.preparations;
          fixture.preparing++; fixture.maxPreparing = Math.max(fixture.maxPreparing, fixture.preparing);
          try {
            if ((attempt === 1 && options.hold) || (attempt > 1 && options.holdRetry)) await new Promise<void>(resolve => { fixture.release = resolve; });
            if (fixture.failures > 0) { fixture.failures--; throw '서버 준비 시험 오류'; }
            return null; // Tauri의 인자 없는 성공 void 응답.
          } finally { fixture.preparing--; }
        }
        if (command === 'start_session') {
          if (fixture.preparing) throw new Error('준비 완료 전 세션 시작');
          const info = { ...args.request, id: `started-${fixture.live.length + 1}`, generation: 1, state: 'idle', active: true };
          fixture.live.push(info); listeners.get('session-changed')?.({ event: 'session-changed', id: 1, payload: info }); return info;
        }
        if (command === 'catalog_list') return { sessions: [{ id: 'catalog-a', title: '불러온 작업', cwd: 'D:\\startup-test', createdAt: 1, updatedAt: 2, status: 'idle' }] };
        if (command === 'catalog_preview') return { messages: [] };
        if (command === 'complete_exit') return null;
        return null;
      },
    };
  }, { options, savedSessions });
  await page.goto('/');
}

const calls = (page: Page, command: string) => page.evaluate(command => (window as any).startupFixture.calls.filter((item: any) => item.command === command), command);
const persisted = (page: Page) => page.evaluate(() => JSON.parse(localStorage.getItem('startup-layout') || 'null'));

test('공유서버 준비가 끝난 뒤에만 저장 세션을 복원하고 기본 void 응답을 허용한다', async ({ page }) => {
  await workspace(page, { hold: true });
  await expect(page.locator('#runtime-banner')).toContainText('공유서버를 준비하는 중');
  expect(await calls(page, 'start_session')).toEqual([]);
  expect(await calls(page, 'save_layout')).toEqual([]);
  const sequence = await page.evaluate(() => (window as any).startupFixture.calls.map((item: any) => item.command));
  expect(sequence.indexOf('get_ui_language')).toBeLessThan(sequence.indexOf('load_layout'));
  expect(sequence.indexOf('load_layout')).toBeLessThan(sequence.indexOf('prepare_runtime'));
  expect((await calls(page, 'plugin:event|listen')).map((item: any) => item.args.event)).toEqual(expect.arrayContaining(['terminal-output', 'session-changed', 'exit-requested']));
  await page.locator('#settings-open').click(); await expect(page.locator('#settings-dialog')).toBeVisible();
  await page.locator('#settings-close').click();
  await page.evaluate(() => (window as any).startupFixture.release());
  await expect(page.locator('.terminal-title')).toHaveText(savedSessions.map(item => item.title));
  await expect(page.locator('#runtime-banner')).toBeHidden();
  expect((await calls(page, 'prepare_runtime')).map((item: any) => item.args)).toEqual([{}]);
  expect((await calls(page, 'start_session')).map((item: any) => item.args.request.threadId)).toEqual(['saved-a', 'saved-b']);
});

test('준비 실패는 복원 없이 저장 의도를 보존하고 단일 재시도로 초기화를 반복하지 않는다', async ({ page }) => {
  await workspace(page, { fail: 1, holdRetry: true });
  await expect(page.locator('#runtime-banner')).toContainText('서버 준비 시험 오류');
  await expect(page.locator('#runtime-retry')).toBeEnabled();
  expect(await calls(page, 'start_session')).toEqual([]);
  await expect.poll(async () => (await persisted(page))?.sessions.map((item: any) => item.resumeOnStartup)).toEqual([true, true]);
  const listensBefore = await calls(page, 'plugin:event|listen');
  await page.locator('#runtime-retry').evaluate((button: HTMLButtonElement) => { button.click(); button.click(); button.click(); });
  await expect.poll(async () => (await calls(page, 'prepare_runtime')).length).toBe(2);
  expect(await calls(page, 'start_session')).toEqual([]);
  await page.evaluate(() => (window as any).startupFixture.release());
  await expect(page.locator('.terminal-title')).toHaveText(savedSessions.map(item => item.title));
  expect(await calls(page, 'load_layout')).toHaveLength(1);
  expect(await calls(page, 'get_ui_language')).toHaveLength(1);
  expect(await calls(page, 'plugin:event|listen')).toEqual(listensBefore);
  expect(await page.evaluate(() => (window as any).startupFixture.maxPreparing)).toBe(1);
});

for (const entry of ['새 세션', '불러오기', '저장 복원']) test(`${entry}도 준비 중에는 세션을 시작하지 않는다`, async ({ page }) => {
  await workspace(page, { hold: true, saved: entry === '저장 복원' });
  await expect.poll(async () => (await calls(page, 'prepare_runtime')).length).toBe(1);
  if (entry === '새 세션') {
    await page.locator('#new-session').click();
    await page.locator('#session-title').fill('새 작업'); await page.locator('#session-cwd').fill('D:\\startup-test');
    await page.locator('#create-submit').click(); await expect(page.locator('#create-submit')).toBeDisabled();
  } else if (entry === '불러오기') {
    await page.locator('#catalog-open').click(); await expect(page.locator('#catalog-resume')).toBeEnabled();
    await page.locator('#catalog-resume').click(); await expect(page.locator('#catalog-resume')).toBeDisabled();
  } else {
    await page.locator('#restore-open').click();
    await page.locator('#restore-list .restore-item').first().getByRole('button', { name: '이 대화 재개 →' }).click();
    await expect(page.locator('#restore-list .restore-item').first().getByRole('button', { name: '이 대화 재개 →' })).toBeDisabled();
  }
  expect(await calls(page, 'start_session')).toEqual([]);
  expect(await calls(page, 'prepare_runtime')).toHaveLength(1);
  await page.evaluate(() => (window as any).startupFixture.release());
  await expect.poll(async () => (await calls(page, 'start_session')).length).toBe(entry === '저장 복원' ? 2 : 1);
  expect(await calls(page, 'prepare_runtime')).toHaveLength(1);
});

for (const retry of [false, true]) test(`${retry ? '재시도' : '최초 준비'} 중 종료하면 늦은 준비 성공도 세션을 복원하지 않는다`, async ({ page }) => {
  await workspace(page, retry ? { fail: 1, holdRetry: true } : { hold: true });
  if (retry) { await expect(page.locator('#runtime-retry')).toBeEnabled(); await page.locator('#runtime-retry').click(); }
  await expect.poll(async () => (await calls(page, 'prepare_runtime')).length).toBe(retry ? 2 : 1);
  await page.locator('#exit').click(); await page.locator('#confirm-exit').click();
  expect(await calls(page, 'complete_exit')).toEqual([]);
  await page.evaluate(() => (window as any).startupFixture.release());
  await expect.poll(async () => (await calls(page, 'complete_exit')).length).toBe(1);
  expect(await calls(page, 'start_session')).toEqual([]);
  expect((await persisted(page)).sessions.map((item: any) => item.resumeOnStartup)).toEqual([true, true]);
});
