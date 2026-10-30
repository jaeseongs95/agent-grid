import { test, expect } from '@playwright/test';

test.beforeEach(async ({ page }) => {
  await page.addInitScript(() => {
    const state = window as any; state.isTauri = true;
    const callbacks = new Map<number, any>(); let callback = 0;
    state.chatCalls = [];
    state.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
    state.__TAURI_INTERNALS__ = {
      transformCallback(handler: any) { callbacks.set(++callback, handler); return callback; }, unregisterCallback() {},
      async invoke(command: string, args: any = {}) {
        state.chatCalls.push({ command, args });
        if (command === 'plugin:event|listen') return callback;
        if (command.startsWith('plugin:event|')) return null;
        if (command === 'get_ui_language') return 'ko';
        if (command === 'load_layout') return { version: 1, columns: 1, sessions: [] };
        if (command === 'list_projects') return [];
        if (command === 'list_sessions') return [{ id: 'chat-session', generation: 1, title: '기존 세션', cwd: 'D:\\Work', threadId: 'original-thread', state: 'idle', active: true }];
        if (command === 'cron_request') return state.cronSnapshot ?? { supported: false, connected: false, scope: 'cliLifetime', jobs: [], hasMore: false, reason: 'unsupported' };
        if (command === 'conversation_request') {
          if (args.request.type === 'attach' && state.attachFailure) throw new Error(state.failureRaw ?? 'fixture attach failure');
          if (args.request.type === 'handoffToTerminal' && state.handoffFailure) throw new Error(state.failureRaw ?? 'fixture handoff failure');
          if (args.request.type === 'contextStatus') return state.contextSnapshot ?? {};
          if (args.request.type === 'compact') {
            if (state.compactFailure) {
              Object.assign(state.contextSnapshot, { canCompact: false, tokenUsageFresh: false, reason: 'unknownResult', revision: 3 });
              throw Error('PRIVATE_COMPACT_TOKEN_PATH');
            }
            Object.assign(state.contextSnapshot, { compacting: true, canCompact: false, tokenUsageFresh: false, reason: 'compacting', revision: 3 });
            return { status: 'started', turn: { id: 'compact-turn' } };
          }
          if (args.request.type === 'attach') {
            if (state.contextSnapshot) Object.assign(state.contextSnapshot, { guiConnected: true, canCompact: true, reason: null, revision: 2 });
            return { threadId: 'original-thread', model: 'available-model', reasoningEffort: 'high' };
          }
          if (args.request.type === 'page' || args.request.type === 'items' || args.request.type === 'models') return { data: [], nextCursor: null };
          if (args.request.type === 'poll') return { events: (state.chatQueue ?? []).splice(0), pendingRequests: [], connected: true, gap: false };
          return {};
        }
        return null;
      },
    };
  });
});

for (const startingView of ['terminal', 'conversation'] as const) {
  test(`세션 상단 압축은 ${startingView}에서 같은 세션에 연결하고 초안을 보존한다`, async ({ page }) => {
    await page.goto('/');
    const card = page.locator('.terminal-card[data-id="chat-session"]');
    await expect(card).toBeVisible();
    await page.evaluate(() => { (window as any).contextSnapshot = { revision: 1, threadId: 'original-thread', connected: true, guiConnected: false, active: false, canCompact: false, reason: 'guiDisconnected', compacting: false, tokenUsageFresh: true, tokenUsage: { last: { totalTokens: 40000 }, modelContextWindow: 100000 } }; });
    await card.locator('.context-usage').click();
    await expect(card.locator('.context-usage')).toHaveText('40%');
    if (startingView === 'conversation') {
      await card.locator('.conversation-toggle').click();
      await card.locator('.conversation-input').fill('압축 후에도 보존할 초안');
    }
    await card.locator('.context-compact').click();
    await expect(card.locator('.conversation-host')).toBeVisible();
    await expect(card.locator('.terminal-host')).toBeHidden();
    await expect(card.locator('.conversation-status')).toContainText('서버의 완료 확인');
    await expect(card.locator('.context-compact')).toHaveAttribute('aria-busy', 'true');
    await expect(card.locator('.conversation-send')).toBeDisabled();
    if (startingView === 'conversation') await expect(card.locator('.conversation-input')).toHaveValue('압축 후에도 보존할 초안');
    const calls = await page.evaluate(() => (window as any).chatCalls);
    const compact = calls.filter((entry: any) => entry.command === 'conversation_request' && entry.args.request.type === 'compact');
    expect(compact).toHaveLength(1);
    expect(compact[0].args).toMatchObject({ id: 'chat-session', generation: 1 });
    expect(calls.some((entry: any) => entry.command === 'start_session' || (entry.command === 'conversation_request' && entry.args.request.type === 'send'))).toBe(false);
  });
}

test('압축 결과 미확인에서는 초안과 입력 소유권을 보존하고 자동 재전송하지 않는다', async ({ page }) => {
  await page.goto('/');
  const card = page.locator('.terminal-card[data-id="chat-session"]');
  await expect(card).toBeVisible();
  await page.evaluate(() => { const s = window as any; s.compactFailure = true; s.contextSnapshot = { revision: 1, connected: true, guiConnected: false, active: false, canCompact: false, reason: 'guiDisconnected', compacting: false, tokenUsageFresh: true }; });
  await card.locator('.context-usage').click();
  await card.locator('.conversation-toggle').click();
  await card.locator('.conversation-input').fill('결과 미확인에서도 보존할 초안');
  await card.locator('.context-compact').click();
  await expect(card.locator('.conversation-input')).toHaveValue('결과 미확인에서도 보존할 초안');
  await expect(card.locator('.conversation-input')).toBeDisabled();
  await expect(card.locator('.context-compact')).toBeDisabled();
  await expect(card.locator('.conversation-status')).toContainText('자동으로 다시 요청하지 않습니다');
  await expect(card.locator('.terminal-host')).toBeHidden();
  expect(await page.locator('body').evaluate(node => node.outerHTML)).not.toContain('PRIVATE_COMPACT_TOKEN_PATH');
  const calls = await page.evaluate(() => (window as any).chatCalls);
  expect(calls.filter((entry: any) => entry.command === 'conversation_request' && entry.args.request.type === 'compact')).toHaveLength(1);
  expect(calls.some((entry: any) => entry.command === 'session_write' || (entry.command === 'conversation_request' && entry.args.request.type === 'send'))).toBe(false);
});

test('same workspace session switches views without starting a thread and preserves its draft', async ({ page }) => {
  await page.goto('/');
  const card = page.locator('.terminal-card[data-id="chat-session"]');
  await expect(card).toBeVisible();
  await card.locator('.conversation-toggle').click();
  await expect(card.locator('.conversation-host')).toBeVisible();
  await expect(card.locator('.terminal-host')).toBeHidden();
  await expect(card.locator('.conversation-input')).toBeEnabled();
  await card.locator('.conversation-input').fill('전송하지 않은 한글 초안');
  await card.locator('.conversation-toggle').click();
  await expect(card.locator('.terminal-host')).toBeVisible();
  await card.locator('.conversation-toggle').click();
  await expect(card.locator('.conversation-input')).toHaveValue('전송하지 않은 한글 초안');
  const calls = await page.evaluate(() => (window as any).chatCalls);
  expect(calls.some((entry: any) => entry.command === 'start_session')).toBe(false);
  expect(calls.filter((entry: any) => entry.command === 'conversation_request').every((entry: any) => entry.args.id === 'chat-session' && entry.args.generation === 1)).toBe(true);
  expect(calls.some((entry: any) => entry.command === 'conversation_request' && entry.args.request.type === 'send')).toBe(false);
  expect(calls.filter((entry: any) => entry.command === 'conversation_request' && entry.args.request.type === 'handoffToTerminal')).toHaveLength(1);
  expect(calls.some((entry: any) => entry.command === 'conversation_request' && entry.args.request.type === 'detach')).toBe(false);
});


test('failed attach keeps the owned conversation visible and terminal input blocked', async ({ page }) => {
  await page.goto('/');
  await page.evaluate(() => { (window as any).attachFailure = true; });
  const card = page.locator('.terminal-card[data-id="chat-session"]');
  await card.locator('.conversation-toggle').click();
  await expect(card.locator('.conversation-host')).toBeVisible();
  await expect(card.locator('.terminal-host')).toBeHidden();
  await expect(card.locator('.conversation-toggle')).toHaveAttribute('aria-pressed', 'true');
  await expect(card.locator('.conversation-input')).toBeDisabled();
  await expect(card.locator('.conversation-status')).not.toContainText('fixture attach failure');
  await expect(card.locator('.conversation-status')).not.toBeEmpty();
  const calls = await page.evaluate(() => (window as any).chatCalls);
  expect(calls.some((entry: any) => entry.command === 'session_write')).toBe(false);
});


test('명시 터미널 인계가 거절되면 대화 화면과 초안을 유지하고 성공 후에만 터미널 입력을 연다', async ({ page }) => {
  await page.goto('/');
  const card = page.locator('.terminal-card[data-id="chat-session"]');
  await card.locator('.conversation-toggle').click();
  await expect(card.locator('.conversation-input')).toBeEnabled();
  await card.locator('.conversation-input').fill('보존해야 하는 인계 초안');
  await page.evaluate(() => { (window as any).handoffFailure = true; });
  await card.locator('.conversation-toggle').click();
  await expect(card.locator('.conversation-host')).toBeVisible();
  await expect(card.locator('.terminal-host')).toBeHidden();
  await expect(card.locator('.conversation-toggle')).toHaveAttribute('aria-pressed', 'true');
  await expect(card.locator('.conversation-input')).toHaveValue('보존해야 하는 인계 초안');
  await expect(card.locator('.conversation-status')).not.toContainText('fixture handoff failure');
  await expect(card.locator('.conversation-status')).not.toBeEmpty();
  expect(await page.evaluate(() => (window as any).chatCalls.some((entry: any) => entry.command === 'session_write'))).toBe(false);
  await page.evaluate(() => { (window as any).handoffFailure = false; });
  await card.locator('.conversation-toggle').click();
  await expect(card.locator('.terminal-host')).toBeVisible();
  await card.locator('.xterm-helper-textarea').press('z');
  await expect.poll(() => page.evaluate(() => (window as any).chatCalls.some((entry: any) => entry.command === 'session_write' && entry.args.id === 'chat-session' && entry.args.generation === 1 && entry.args.data === 'z'))).toBe(true);
  const requests = await page.evaluate(() => (window as any).chatCalls.filter((entry: any) => entry.command === 'conversation_request').map((entry: any) => entry.args.request.type));
  expect(requests.filter((type: string) => type === 'handoffToTerminal')).toHaveLength(2);
  expect(requests).not.toContain('detach');
});

for (const language of ['ko', 'en'] as const) for (const failure of ['attach', 'handoff'] as const) {
  test(language + ' 전환 오류를 상위 알림과 전체 화면에 원문으로 노출하지 않는다: ' + failure, async ({ page }) => {
    await page.goto('/');
    const card = page.locator('.terminal-card[data-id="chat-session"]');
    await expect(card).toBeVisible();
    await page.evaluate(async value => { const { setLanguage } = await import('/src/i18n.ts'); setLanguage(value); }, language);
    const markers = ['private_fixture_path_701', 'token_fixture_NEVER_REAL_701', 'INTERNAL_PRIVATE_FAILURE_701'];
    await page.evaluate(values => { (window as any).failureRaw = 'D:/private_fixture_path_701/helper.exe ' + values.join(' '); }, markers);
    if (failure === 'attach') await page.evaluate(() => { (window as any).attachFailure = true; });
    else {
      await card.locator('.conversation-toggle').click();
      await expect(card.locator('.conversation-input')).toBeEnabled();
      await card.locator('.conversation-input').fill('오류에서도 보존할 초안');
      await page.evaluate(() => {
        const state = window as any; state.handoffFailure = true;
        state.chatQueue = [{ seq: 1, method: 'turn/started', params: { turn: { id: 'public-turn', status: 'inProgress' } } }, { seq: 2, method: 'item/completed', params: { turnId: 'public-turn', item: { id: 'public-output', type: 'agentMessage', text: '사용자가 볼 실제 대화 표식' } } }];
      });
      await expect(card.locator('.conversation-messages')).toContainText('사용자가 볼 실제 대화 표식');
    }
    await card.locator('.conversation-toggle').click();
    await expect(card.locator('.conversation-host')).toBeVisible();
    await expect(card.locator('.terminal-host')).toBeHidden();
    await expect(card.locator('.conversation-toggle')).toHaveAttribute('aria-pressed', 'true');
    const html = await page.locator('body').evaluate(node => node.outerHTML);
    for (const marker of markers) expect(html).not.toContain(marker);
    await expect(page.locator('#toast-area .toast.error')).toContainText(language === 'ko' ? '현재 화면과 작성 중인 내용을 확인' : 'Check the current view and your draft');
    if (failure === 'handoff') {
      await expect(card.locator('.conversation-input')).toHaveValue('오류에서도 보존할 초안');
      await expect(card.locator('.conversation-messages')).toContainText('사용자가 볼 실제 대화 표식');
    }
    const calls = await page.evaluate(() => (window as any).chatCalls);
    expect(calls.filter((entry: any) => entry.command === 'session_write')).toHaveLength(0);
    expect(calls.filter((entry: any) => entry.command === 'conversation_request' && entry.args.request.type === 'send')).toHaveLength(0);
  });
}


test('세션 상단 예약 아이콘은 원래 세션 목록만 조회하고 터미널 입력이나 새 세션을 만들지 않는다', async ({ page }) => {
  await page.goto('/');
  const card = page.locator('.terminal-card[data-id="chat-session"]');
  const icon = card.getByRole('button', { name: '예약 작업', exact: true });
  await expect(icon).toBeVisible();
  expect(await page.evaluate(() => (window as any).chatCalls.filter((c: any) => c.command === 'cron_request'))).toHaveLength(0);
  await icon.click();
  const dialog = page.locator('#cron-dialog');
  await expect(dialog).toBeVisible();
  await expect(dialog.locator('.cron-session')).toHaveText('세션: 기존 세션');
  await expect(dialog.locator('.cron-status')).toContainText('지원하지 않습니다');
  await expect(dialog.locator('.cron-add')).toBeDisabled();
  const calls = await page.evaluate(() => (window as any).chatCalls);
  expect(calls.filter((c: any) => c.command === 'cron_request')).toEqual([{ command: 'cron_request', args: { id: 'chat-session', generation: 1, request: { type: 'list' } } }]);
  expect(calls.some((c: any) => c.command === 'session_write' || c.command === 'start_session')).toBe(false);
  await dialog.locator('.cron-close').click();
  await expect(dialog).not.toBeVisible();
});
