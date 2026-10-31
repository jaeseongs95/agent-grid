import { test, expect, type Page } from '@playwright/test';

// 실제 화면/세션 시작 경계 검사. 준비·저장·세션 명령만 대체하며 실제 서버 실행은 검증하지 않는다.
const savedSessions = [
  { title: '이전 첫 작업', cwd: 'D:\\startup-test', threadId: 'saved-a', resumeOnStartup: true },
  { title: '이전 둘째 작업', cwd: 'D:\\startup-test', threadId: 'saved-b', resumeOnStartup: true },
];

async function workspace(page: Page, options: { hold?: boolean; fail?: number; holdRetry?: boolean; saved?: boolean; holdSelected?: boolean; holdMutation?: boolean; failSelected?: number; failExit?: boolean; selectionResult?: 'cancel' | 'reject' } = {}) {
  await page.addInitScript(({ options, savedSessions }) => {
    const state = window as any; state.isTauri = true;
    let callback = 0;
    const callbacks = new Map<number, (value: any) => void>();
    const listeners = new Map<string, (value: any) => void>();
    const releases = new Map<string, () => void>();
    const fixture = state.startupFixture = {
      calls: [] as { command: string; args: any }[], live: [] as any[], preparations: 0, preparing: 0, maxPreparing: 0,
      failures: options.fail ?? 0, release: () => {},
      selected: 'A', prepared: [] as string[], startedWith: [] as string[], usageWith: [] as string[], completed: [] as string[],
      selectedFailures: options.failSelected ?? 0, releaseMutation: () => {},
      failExit: options.failExit ?? false, exitPreparing: [] as number[],
      releaseFor(executable: string) { releases.get(executable)?.(); },
      visibility(visible: boolean) { listeners.get('application-visibility')?.({ event: 'application-visibility', id: 1, payload: visible }); },
    };
    const settings = () => ({ savedPath: `D:\\runtime-${fixture.selected}.exe`, resolvedPath: `D:\\runtime-${fixture.selected}.exe`, source: 'saved', error: null });
    state.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
    state.__TAURI_INTERNALS__ = {
      transformCallback(handler: (value: any) => void) { callbacks.set(++callback, handler); return callback; }, unregisterCallback() {},
      async invoke(command: string, args: any = {}) {
        fixture.calls.push({ command, args });
        if (command === 'plugin:event|listen') { listeners.set(args.event, callbacks.get(args.handler)!); return callback; }
        if (command.startsWith('plugin:event|')) return callback;
        if (command === 'get_ui_language') return 'ko';
        if (command === 'get_codex_settings') return settings();
        if (command === 'choose_codex_executable' || command === 'set_codex_executable') {
          if (!options.selectionResult) fixture.selected = 'B';
          if (options.holdMutation) await new Promise<void>(resolve => { fixture.releaseMutation = resolve; });
          if (options.selectionResult === 'cancel') return null;
          if (options.selectionResult === 'reject') throw '선택 저장 시험 오류';
          return settings();
        }
        if (command === 'list_projects') return [];
        if (command === 'list_sessions') return fixture.live;
        if (command === 'load_layout') return { version: 1, columns: 2, sessions: options.saved === false ? [] : savedSessions };
        if (command === 'save_layout') { localStorage.setItem('startup-layout', JSON.stringify(args.layout)); return null; }
        if (command === 'weekly_usage') { fixture.usageWith.push(fixture.selected); return { executable: 'startup-test', observedAt: Date.now(), limitName: null, weekly: null }; }
        if (command === 'prepare_runtime') {
          const attempt = ++fixture.preparations;
          const executable = fixture.selected; fixture.prepared.push(executable);
          fixture.preparing++; fixture.maxPreparing = Math.max(fixture.maxPreparing, fixture.preparing);
          try {
            if ((attempt === 1 && options.hold) || (attempt > 1 && options.holdRetry) || (executable === 'B' && options.holdSelected)) await new Promise<void>(resolve => { fixture.release = resolve; releases.set(executable, resolve); });
            if (fixture.failures > 0) { fixture.failures--; throw '서버 준비 시험 오류'; }
            if (executable === 'B' && fixture.selectedFailures > 0) { fixture.selectedFailures--; throw '새 실행 파일 준비 오류'; }
            return null; // Tauri의 인자 없는 성공 void 응답.
          } finally { fixture.preparing--; fixture.completed.push(executable); }
        }
        if (command === 'start_session') {
          fixture.startedWith.push(fixture.selected);
          if (fixture.preparing && !fixture.completed.includes(fixture.selected)) throw new Error('준비 완료 전 세션 시작');
          const info = { ...args.request, id: `started-${fixture.live.length + 1}`, generation: 1, state: 'idle', active: true };
          fixture.live.push(info); listeners.get('session-changed')?.({ event: 'session-changed', id: 1, payload: info }); return info;
        }
        if (command === 'catalog_list') return { sessions: [{ id: 'catalog-a', title: '불러온 작업', cwd: 'D:\\startup-test', createdAt: 1, updatedAt: 2, status: 'idle' }] };
        if (command === 'catalog_preview') return { messages: [] };
        if (command === 'complete_exit') { fixture.exitPreparing.push(fixture.preparing); if (fixture.failExit) throw '종료 시험 오류'; return null; }
        return null;
      },
    };
  }, { options, savedSessions });
  await page.goto('/');
}

const calls = (page: Page, command: string) => page.evaluate(command => (window as any).startupFixture.calls.filter((item: any) => item.command === command), command);
const persisted = (page: Page) => page.evaluate(() => JSON.parse(localStorage.getItem('startup-layout') || 'null'));

async function selectExecutable(page: Page, reset = false) {
  await page.locator('#settings-open').click(); await page.locator('#codex-settings-open').click();
  await expect(page.locator('#codex-executable-choose')).toBeEnabled();
  await page.locator(reset ? '#codex-executable-reset' : '#codex-executable-choose').click();
  await expect.poll(() => page.evaluate(() => (window as any).startupFixture.selected)).toBe('B');
}

async function closeSettings(page: Page) {
  await page.locator('#codex-settings-close').click(); await page.locator('#settings-close').click();
}

test('공유서버 준비가 끝난 뒤에만 저장 세션을 복원하고 기본 void 응답을 허용한다', async ({ page }) => {
  await workspace(page, { hold: true });
  await expect(page.locator('#runtime-banner')).toContainText('공유서버를 준비하는 중');
  expect(await calls(page, 'start_session')).toEqual([]);
  expect(await calls(page, 'save_layout')).toEqual([]);
  await page.evaluate(() => { const f = (window as any).startupFixture; f.visibility(false); f.visibility(true); document.querySelector<HTMLButtonElement>('#weekly-usage-refresh')!.click(); });
  expect(await calls(page, 'weekly_usage')).toEqual([]);
  const sequence = await page.evaluate(() => (window as any).startupFixture.calls.map((item: any) => item.command));
  expect(sequence.indexOf('get_ui_language')).toBeLessThan(sequence.indexOf('load_layout'));
  expect(sequence.indexOf('load_layout')).toBeLessThan(sequence.indexOf('prepare_runtime'));
  expect((await calls(page, 'plugin:event|listen')).map((item: any) => item.args.event)).toEqual(expect.arrayContaining(['terminal-output', 'session-changed', 'exit-requested']));
  await page.locator('#settings-open').click(); await expect(page.locator('#settings-dialog')).toBeVisible();
  await page.locator('#settings-close').click();
  await page.evaluate(() => (window as any).startupFixture.release());
  await expect(page.locator('.terminal-title')).toHaveText(savedSessions.map(item => item.title));
  await expect(page.locator('#runtime-banner')).toBeHidden();
  expect(await calls(page, 'weekly_usage')).toHaveLength(1);
  expect((await calls(page, 'prepare_runtime')).map((item: any) => item.args)).toEqual([{}]);
  expect((await calls(page, 'start_session')).map((item: any) => item.args.request.threadId)).toEqual(['saved-a', 'saved-b']);
});

test('준비 실패는 복원 없이 저장 의도를 보존하고 단일 재시도로 초기화를 반복하지 않는다', async ({ page }) => {
  await workspace(page, { fail: 1, holdRetry: true });
  await expect(page.locator('#runtime-banner')).toContainText('서버 준비 시험 오류');
  await expect(page.locator('#runtime-retry')).toBeEnabled();
  expect(await calls(page, 'start_session')).toEqual([]);
  await expect.poll(async () => (await persisted(page))?.sessions.map((item: any) => item.resumeOnStartup)).toEqual([true, true]);
  await page.evaluate(() => { const f = (window as any).startupFixture; f.visibility(false); f.visibility(true); document.querySelector<HTMLButtonElement>('#weekly-usage-refresh')!.click(); });
  expect(await calls(page, 'weekly_usage')).toEqual([]);
  const listensBefore = await calls(page, 'plugin:event|listen');
  await page.locator('#runtime-retry').evaluate((button: HTMLButtonElement) => { button.click(); button.click(); button.click(); });
  await expect.poll(async () => (await calls(page, 'prepare_runtime')).length).toBe(2);
  expect(await calls(page, 'start_session')).toEqual([]);
  expect(await calls(page, 'weekly_usage')).toEqual([]);
  await page.evaluate(() => (window as any).startupFixture.release());
  await expect(page.locator('.terminal-title')).toHaveText(savedSessions.map(item => item.title));
  expect(await calls(page, 'weekly_usage')).toHaveLength(1);
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
  expect(await calls(page, 'weekly_usage')).toEqual([]);
});

test('선택 저장 응답 전 이전 준비가 끝나도 새 실행 파일을 준비 없이 사용하지 않는다', async ({ page }) => {
  await workspace(page, { hold: true, holdSelected: true, holdMutation: true });
  await expect.poll(async () => (await calls(page, 'prepare_runtime')).length).toBe(1);
  await selectExecutable(page);
  await page.evaluate(() => (window as any).startupFixture.releaseFor('A'));
  await expect.poll(() => page.evaluate(() => (window as any).startupFixture.completed)).toEqual(['A']);
  expect(await calls(page, 'start_session')).toEqual([]); expect(await calls(page, 'weekly_usage')).toEqual([]);
  await page.evaluate(() => (window as any).startupFixture.releaseMutation());
  await expect.poll(() => page.evaluate(() => (window as any).startupFixture.prepared)).toEqual(['A', 'B']);
  expect(await calls(page, 'start_session')).toEqual([]); expect(await calls(page, 'weekly_usage')).toEqual([]);
  await page.evaluate(() => (window as any).startupFixture.releaseFor('B'));
  await expect(page.locator('.terminal-title')).toHaveText(savedSessions.map(item => item.title));
  expect(await page.evaluate(() => (window as any).startupFixture.startedWith)).toEqual(['B', 'B']);
  expect(await page.evaluate(() => (window as any).startupFixture.usageWith)).toEqual(['B']);
});

test('새 선택은 이전 준비 명령의 정산을 기다리되 이전 완료만으로 조회와 복원을 열지 않는다', async ({ page }) => {
  await workspace(page, { hold: true, holdSelected: true });
  await expect.poll(async () => (await calls(page, 'prepare_runtime')).length).toBe(1);
  await selectExecutable(page);
  expect(await page.evaluate(() => (window as any).startupFixture.prepared)).toEqual(['A']);
  await page.evaluate(() => (window as any).startupFixture.releaseFor('A'));
  await expect.poll(() => page.evaluate(() => (window as any).startupFixture.prepared)).toEqual(['A', 'B']);
  expect(await calls(page, 'start_session')).toEqual([]); expect(await calls(page, 'weekly_usage')).toEqual([]);
  await page.evaluate(() => (window as any).startupFixture.releaseFor('B'));
  await expect(page.locator('.terminal-title')).toHaveText(savedSessions.map(item => item.title));
  expect(await page.evaluate(() => (window as any).startupFixture.completed)).toEqual(['A', 'B']);
  expect(await page.evaluate(() => (window as any).startupFixture.startedWith)).toEqual(['B', 'B']);
  expect(await page.evaluate(() => (window as any).startupFixture.usageWith)).toEqual(['B']);
  expect(await page.evaluate(() => (window as any).startupFixture.maxPreparing)).toBe(1);
  await expect(page.locator('#runtime-banner')).toBeHidden();
});

test('준비된 실행 파일을 변경하면 사용량과 새 세션을 잠그고 새 준비 하나를 공유한다', async ({ page }) => {
  await workspace(page, { saved: false, holdSelected: true });
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  expect(await page.evaluate(() => (window as any).startupFixture.usageWith)).toEqual(['A']);
  await selectExecutable(page, true); await closeSettings(page);
  await expect.poll(() => page.evaluate(() => (window as any).startupFixture.prepared)).toEqual(['A', 'B']);
  await page.locator('#new-session').click(); await page.locator('#session-title').fill('선택 후 작업'); await page.locator('#session-cwd').fill('D:\\startup-test');
  await page.locator('#create-submit').click(); await expect(page.locator('#create-submit')).toBeDisabled();
  expect(await calls(page, 'start_session')).toEqual([]);
  expect(await page.evaluate(() => (window as any).startupFixture.usageWith)).toEqual(['A']);
  await page.evaluate(() => (window as any).startupFixture.releaseFor('B'));
  await expect(page.locator('.terminal-title')).toHaveText(['선택 후 작업']);
  expect(await page.evaluate(() => (window as any).startupFixture.usageWith)).toEqual(['A', 'B']);
  expect(await page.evaluate(() => (window as any).startupFixture.startedWith)).toEqual(['B']);
  expect(await calls(page, 'prepare_runtime')).toHaveLength(2);
});

test('새 선택 준비 실패는 명시 재시도 전까지 조회와 세션 시작을 다시 열지 않는다', async ({ page }) => {
  await workspace(page, { hold: true, failSelected: 1 });
  await expect.poll(async () => (await calls(page, 'prepare_runtime')).length).toBe(1);
  await selectExecutable(page); await closeSettings(page);
  await page.evaluate(() => (window as any).startupFixture.releaseFor('A'));
  await expect(page.locator('#runtime-banner')).toContainText('새 실행 파일 준비 오류');
  await expect.poll(() => page.evaluate(() => (window as any).startupFixture.completed.length)).toBe(2);
  expect(await calls(page, 'weekly_usage')).toEqual([]); expect(await calls(page, 'start_session')).toEqual([]);
  expect(await calls(page, 'prepare_runtime')).toHaveLength(2);
  await page.locator('#runtime-retry').click();
  await expect(page.locator('.terminal-title')).toHaveText(savedSessions.map(item => item.title));
  expect(await page.evaluate(() => (window as any).startupFixture.prepared)).toEqual(['A', 'B', 'B']);
  expect(await page.evaluate(() => (window as any).startupFixture.usageWith)).toEqual(['B']);
});

test('선택 변경 준비 중 종료하면 이전 명령 정산 뒤 새 준비와 조회를 시작하지 않는다', async ({ page }) => {
  await workspace(page, { hold: true, holdSelected: true });
  await expect.poll(async () => (await calls(page, 'prepare_runtime')).length).toBe(1);
  await selectExecutable(page); await closeSettings(page);
  await page.locator('#exit').click(); await page.locator('#confirm-exit').click();
  expect(await calls(page, 'complete_exit')).toEqual([]);
  await page.evaluate(() => (window as any).startupFixture.releaseFor('A'));
  await expect.poll(async () => (await calls(page, 'complete_exit')).length).toBe(1);
  expect(await page.evaluate(() => (window as any).startupFixture.completed)).toEqual(['A']);
  expect(await calls(page, 'prepare_runtime')).toHaveLength(1);
  expect(await calls(page, 'start_session')).toEqual([]); expect(await calls(page, 'weekly_usage')).toEqual([]);
});

for (const failPrepare of [false, true]) test(`최초 완료 후 선택 준비 ${failPrepare ? '실패' : '성공'}도 실제 명령 정산 뒤에만 종료한다`, async ({ page }) => {
  await workspace(page, { saved: false, holdSelected: true, failSelected: failPrepare ? 1 : 0 });
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await selectExecutable(page); await closeSettings(page);
  await expect.poll(() => page.evaluate(() => (window as any).startupFixture.prepared)).toEqual(['A', 'B']);
  await page.locator('#exit').click(); await page.locator('#confirm-exit').click();
  await expect(page.locator('#confirm-exit')).toBeDisabled();
  expect(await calls(page, 'complete_exit')).toEqual([]);
  await page.evaluate(() => (window as any).startupFixture.releaseFor('B'));
  await expect.poll(async () => (await calls(page, 'complete_exit')).length).toBe(1);
  expect(await page.evaluate(() => (window as any).startupFixture.exitPreparing)).toEqual([0]);
  expect(await page.evaluate(() => (window as any).startupFixture.completed)).toEqual(['A', 'B']);
  expect(await page.evaluate(() => (window as any).startupFixture.usageWith)).toEqual(['A']);
  expect(await calls(page, 'start_session')).toEqual([]);
  if (failPrepare) await expect(page.locator('#runtime-banner')).toContainText('새 실행 파일 준비 오류');
  else await expect(page.locator('#runtime-banner')).toBeHidden();
});

for (const failPrepare of [false, true]) test(`최초 완료 후 선택 준비 ${failPrepare ? '실패' : '성공'} 상태는 종료 실패와 취소 뒤에도 보존한다`, async ({ page }) => {
  await workspace(page, { saved: false, holdSelected: true, failSelected: failPrepare ? 1 : 0, failExit: true });
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await selectExecutable(page); await closeSettings(page);
  await expect.poll(() => page.evaluate(() => (window as any).startupFixture.prepared)).toEqual(['A', 'B']);
  await page.locator('#exit').click(); await page.locator('#confirm-exit').click();
  await expect(page.locator('#confirm-exit')).toBeDisabled();
  expect(await calls(page, 'complete_exit')).toEqual([]);
  await page.evaluate(() => (window as any).startupFixture.releaseFor('B'));
  await expect(page.locator('.toast.error').last()).toContainText('종료 시험 오류');
  await expect(page.locator('#confirm-exit')).toBeEnabled();
  expect(await page.evaluate(() => (window as any).startupFixture.exitPreparing)).toEqual([0]);
  await page.locator('#exit-dialog [data-close]').last().click();
  if (failPrepare) {
    await expect(page.locator('#runtime-banner')).toContainText('새 실행 파일 준비 오류');
    expect(await page.evaluate(() => (window as any).startupFixture.usageWith)).toEqual(['A']);
    await page.locator('#new-session').click(); await page.locator('#session-title').fill('실패 뒤 작업'); await page.locator('#session-cwd').fill('D:\\startup-test');
    await page.locator('#create-submit').click(); await expect(page.locator('#create-error')).toContainText('새 실행 파일 준비 오류');
    expect(await calls(page, 'prepare_runtime')).toHaveLength(2);
    await page.locator('#create-dialog [data-close]').last().click();
    await page.locator('#runtime-retry').click();
    await expect.poll(async () => (await calls(page, 'prepare_runtime')).length).toBe(3);
    await page.evaluate(() => (window as any).startupFixture.releaseFor('B'));
  }
  await expect(page.locator('#runtime-banner')).toBeHidden();
  await expect.poll(() => page.evaluate(() => (window as any).startupFixture.usageWith)).toEqual(['A', 'B']);
  await page.locator('#new-session').click(); await page.locator('#session-title').fill('복구 작업'); await page.locator('#session-cwd').fill('D:\\startup-test');
  await page.locator('#create-submit').click(); await expect(page.locator('.terminal-title')).toHaveText(['복구 작업']);
  expect(await calls(page, 'prepare_runtime')).toHaveLength(failPrepare ? 3 : 2);
});

test('최초 완료 후 선택 준비 중 종료 창을 취소하면 준비와 조회가 정상 재개한다', async ({ page }) => {
  await workspace(page, { saved: false, holdSelected: true });
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await selectExecutable(page); await closeSettings(page);
  await expect.poll(() => page.evaluate(() => (window as any).startupFixture.prepared)).toEqual(['A', 'B']);
  await page.locator('#exit').click(); await page.locator('#exit-dialog [data-close]').last().click();
  await page.evaluate(() => (window as any).startupFixture.releaseFor('B'));
  await expect(page.locator('#runtime-banner')).toBeHidden();
  await expect.poll(() => page.evaluate(() => (window as any).startupFixture.usageWith)).toEqual(['A', 'B']);
  expect(await calls(page, 'complete_exit')).toEqual([]); expect(await calls(page, 'prepare_runtime')).toHaveLength(2);
});

for (const failPrepare of [false, true]) test(`최초 실행 파일 준비 ${failPrepare ? '실패는 명시 재시도' : '성공은 자동 복원'}로 종료 실패 뒤 작업을 회복한다`, async ({ page }) => {
  await workspace(page, { hold: true, fail: failPrepare ? 1 : 0, failExit: true });
  await expect.poll(async () => (await calls(page, 'prepare_runtime')).length).toBe(1);
  await page.locator('#exit').click(); await page.locator('#confirm-exit').click();
  expect(await calls(page, 'complete_exit')).toEqual([]);
  await page.evaluate(() => (window as any).startupFixture.releaseFor('A'));
  await expect(page.locator('.toast.error').last()).toContainText('종료 시험 오류');
  await expect(page.locator('#confirm-exit')).toBeEnabled();
  await page.locator('#exit-dialog [data-close]').last().click();
  if (failPrepare) {
    await expect(page.locator('#runtime-banner')).toContainText('서버 준비 시험 오류');
    expect(await calls(page, 'start_session')).toEqual([]); expect(await calls(page, 'weekly_usage')).toEqual([]);
    await page.locator('#runtime-retry').click();
  }
  await expect(page.locator('.terminal-title')).toHaveText(savedSessions.map(item => item.title));
  await expect(page.locator('#runtime-banner')).toBeHidden();
  expect(await page.evaluate(() => (window as any).startupFixture.usageWith)).toEqual(['A']);
  expect(await calls(page, 'prepare_runtime')).toHaveLength(failPrepare ? 2 : 1);
});

for (const selectionResult of [undefined, 'cancel', 'reject'] as const) for (const failOriginal of [false, true]) test(`선택 ${selectionResult ?? '저장'}과 이전 준비 ${failOriginal ? '실패' : '성공'} 뒤 종료 실패는 건너뛴 현재 준비를 한 번 재개한다`, async ({ page }) => {
  await workspace(page, { hold: true, holdRetry: true, fail: failOriginal ? 1 : 0, failExit: true, selectionResult });
  await expect.poll(async () => (await calls(page, 'prepare_runtime')).length).toBe(1);
  await page.locator('#settings-open').click(); await page.locator('#codex-settings-open').click();
  await expect(page.locator('#codex-executable-choose')).toBeEnabled(); await page.locator('#codex-executable-choose').click();
  await expect.poll(async () => (await calls(page, 'choose_codex_executable')).length).toBe(1);
  await expect(page.locator('#codex-executable-choose')).toBeEnabled();
  if (selectionResult === 'reject') await expect(page.locator('#codex-settings-error')).toContainText('선택 저장 시험 오류');
  await closeSettings(page);
  expect(await page.evaluate(() => (window as any).startupFixture.prepared)).toEqual(['A']);
  await page.locator('#exit').click(); await page.locator('#confirm-exit').click();
  await expect(page.locator('#confirm-exit')).toBeDisabled(); expect(await calls(page, 'complete_exit')).toEqual([]);
  await page.evaluate(() => (window as any).startupFixture.releaseFor('A'));
  await expect(page.locator('.toast.error').last()).toContainText('종료 시험 오류');
  await expect(page.locator('#confirm-exit')).toBeEnabled();
  const selected = selectionResult ? 'A' : 'B';
  await expect.poll(() => page.evaluate(() => (window as any).startupFixture.prepared)).toEqual(['A', selected]);
  expect(await page.evaluate(() => (window as any).startupFixture.exitPreparing)).toEqual([0]);
  expect(await page.evaluate(() => (window as any).startupFixture.maxPreparing)).toBe(1);
  expect(await calls(page, 'start_session')).toEqual([]); expect(await calls(page, 'weekly_usage')).toEqual([]);
  await page.locator('#exit-dialog [data-close]').last().click();
  await page.evaluate(selected => (window as any).startupFixture.releaseFor(selected), selected);
  await expect(page.locator('.terminal-title')).toHaveText(savedSessions.map(item => item.title));
  await expect(page.locator('#runtime-banner')).toBeHidden();
  expect(await page.evaluate(() => (window as any).startupFixture.usageWith)).toEqual([selected]);
  expect(await page.evaluate(() => (window as any).startupFixture.startedWith)).toEqual([selected, selected]);
  expect(await calls(page, 'prepare_runtime')).toHaveLength(2);
});

test('선택 준비를 종료 실패 뒤 재개해도 실제 실패는 추가 자동 재시도 없이 보존한다', async ({ page }) => {
  await workspace(page, { hold: true, failExit: true, failSelected: 1 });
  await expect.poll(async () => (await calls(page, 'prepare_runtime')).length).toBe(1);
  await selectExecutable(page); await closeSettings(page);
  await page.locator('#exit').click(); await page.locator('#confirm-exit').click();
  expect(await calls(page, 'complete_exit')).toEqual([]);
  await page.evaluate(() => (window as any).startupFixture.releaseFor('A'));
  await expect(page.locator('#confirm-exit')).toBeEnabled();
  await expect(page.locator('#runtime-banner')).toContainText('새 실행 파일 준비 오류');
  expect(await calls(page, 'prepare_runtime')).toHaveLength(2);
  expect(await calls(page, 'start_session')).toEqual([]); expect(await calls(page, 'weekly_usage')).toEqual([]);
  await page.locator('#exit-dialog [data-close]').last().click(); await page.locator('#runtime-retry').click();
  await expect(page.locator('.terminal-title')).toHaveText(savedSessions.map(item => item.title));
  expect(await calls(page, 'prepare_runtime')).toHaveLength(3);
  expect(await page.evaluate(() => (window as any).startupFixture.usageWith)).toEqual(['B']);
});
