import { test, expect } from '@playwright/test';

test('same workspace session switches views without starting a thread and preserves its draft', async ({ page }) => {
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
          if (args.request.type === 'attach') return { threadId: 'original-thread', model: 'available-model', reasoningEffort: 'high' };
          if (args.request.type === 'page' || args.request.type === 'items' || args.request.type === 'models') return { data: [], nextCursor: null };
          if (args.request.type === 'poll') return { events: [], pendingRequests: [], connected: true, gap: false };
          return {};
        }
        return null;
      },
    };
  });
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
});
