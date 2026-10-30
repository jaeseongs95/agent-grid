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
        if (command === 'conversation_request') {
          if (args.request.type === 'attach' && state.attachFailure) throw new Error('fixture attach failure');
          if (args.request.type === 'handoffToTerminal' && state.handoffFailure) throw new Error('fixture handoff failure');
          if (args.request.type === 'attach') return { threadId: 'original-thread', model: 'available-model', reasoningEffort: 'high' };
          if (args.request.type === 'page' || args.request.type === 'items' || args.request.type === 'models') return { data: [], nextCursor: null };
          if (args.request.type === 'poll') return { events: [], pendingRequests: [], connected: true, gap: false };
          return {};
        }
        return null;
      },
    };
  });
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
  await expect(card.locator('.conversation-status')).toContainText('fixture attach failure');
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
  await expect(card.locator('.conversation-status')).toContainText('fixture handoff failure');
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
