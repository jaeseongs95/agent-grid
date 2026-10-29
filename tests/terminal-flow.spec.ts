import { test, expect, type Page } from '@playwright/test';

const sessions = [
  { id: 'flow-a', title: 'Flow Alpha', cwd: 'D:\\Work\\Alpha', generation: 7, state: 'idle', active: true, threadId: 'flow-thread-a' },
  { id: 'flow-b', title: 'Flow Beta', cwd: 'D:\\Work\\Beta', generation: 7, state: 'idle', active: true, threadId: 'flow-thread-b' },
];
const card = (page: Page, id = 'flow-a') => page.locator(`.terminal-card[data-id="${id}"]`);
const input = (page: Page) => card(page).locator('.xterm-helper-textarea');

async function workspace(page: Page) {
  await page.addInitScript(sessions => {
    const state = window as any; state.isTauri = true;
    let callback = 0;
    const callbacks = new Map<number, (value: any) => void>();
    const listeners = new Map<string, (value: any) => void>();
    const writes: (() => void)[] = []; const resizes: (() => void)[] = [];
    state.flow = { calls: [], sent: [], holdWrites: false, holdResize: false, activeWrites: 0, maxWrites: 0, activeResizes: 0, maxResizes: 0,
      releaseWrites() { this.holdWrites = false; for (const release of writes.splice(0)) release(); },
      releaseResize() { this.holdResize = false; for (const release of resizes.splice(0)) release(); },
      emit(info: any) { Object.assign(sessions.find(item => item.id === info.id)!, info); listeners.get('session-changed')!({ event: 'session-changed', id: 1, payload: sessions.find(item => item.id === info.id) }); },
    };
    state.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
    state.__TAURI_INTERNALS__ = {
      transformCallback(handler: (value: any) => void) { callbacks.set(++callback, handler); return callback; },
      unregisterCallback() {},
      async invoke(command: string, args: any = {}) {
        const flow = state.flow; flow.calls.push({ command, args });
        if (command === 'plugin:event|listen') { listeners.set(args.event, callbacks.get(args.handler)!); return callback; }
        if (command.startsWith('plugin:event|')) return callback;
        if (command === 'get_ui_language') return 'ko';
        if (command === 'set_ui_language') return { show: 'OpenDIY Desktop 열기', exit: '종료…' };
        if (command === 'load_layout') return { version: 1, columns: 2, sessions: [] };
        if (command === 'list_sessions') return sessions;
        if (command === 'list_projects') return sessions.map(item => ({ id: item.id, name: item.title, cwd: item.cwd, createdAt: 1 }));
        if (command === 'session_write') {
          if (new TextEncoder().encode(args.data).byteLength > 64 * 1024) throw Error('Native input limit');
          flow.activeWrites++; flow.maxWrites = Math.max(flow.maxWrites, flow.activeWrites);
          try { if (flow.holdWrites && args.id === 'flow-a') await new Promise<void>(resolve => writes.push(resolve)); flow.sent.push(args); }
          finally { flow.activeWrites--; }
        }
        if (command === 'session_resize' && args.id === 'flow-a') {
          flow.activeResizes++; flow.maxResizes = Math.max(flow.maxResizes, flow.activeResizes);
          try { if (flow.holdResize) await new Promise<void>(resolve => resizes.push(resolve)); }
          finally { flow.activeResizes--; }
        }
        return null;
      },
    };
  }, sessions);
  await page.goto('/');
  await expect(page.locator('#connection')).toHaveText('로컬 연결됨');
  await expect(card(page)).toBeVisible();
  await input(page).focus();
}

test('held native input retains at most128 operations, preserves order and resumes stdin after drain', async ({ page }) => {
  await workspace(page);
  await page.evaluate(() => { (window as any).flow.holdWrites = true; });
  const typed = 'abcdefghij'.repeat(20);
  await input(page).pressSequentially(typed);
  expect(await page.evaluate(() => (window as any).flow.calls.filter((call: any) => call.command === 'session_write').length)).toBe(1);
  await page.evaluate(() => (window as any).flow.releaseWrites());
  await expect.poll(() => page.evaluate(() => (window as any).flow.sent.length)).toBe(128);
  expect(await page.evaluate(() => (window as any).flow.sent.map((item: any) => item.data).join(''))).toBe(typed.slice(0, 128));
  await expect(card(page)).toHaveAttribute('data-input-paused', 'false');
  await expect(card(page)).toHaveAttribute('data-input-pending', '0');
  await input(page).press('z');
  await expect.poll(() => page.evaluate(() => (window as any).flow.sent.at(-1)?.data)).toBe('z');
  expect(await page.evaluate(() => (window as any).flow.maxWrites)).toBe(1);
});

test('held Unicode pastes are bounded by256KiB including the inflight payload without truncating accepted text', async ({ page }) => {
  await workspace(page);
  const payload = '한😀'.repeat(9362) + 'ab'; // 65,536 UTF-8 bytes, whole code points.
  expect(new TextEncoder().encode(payload).byteLength).toBe(64 * 1024);
  await page.evaluate(() => { (window as any).flow.holdWrites = true; });
  for (let index = 0; index < 6; index++) await page.keyboard.insertText(payload);
  await page.evaluate(() => (window as any).flow.releaseWrites());
  await expect.poll(() => page.evaluate(() => (window as any).flow.sent.length)).toBe(4);
  const sent = await page.evaluate(() => (window as any).flow.sent.map((item: any) => item.data));
  expect(sent).toEqual([payload, payload, payload, payload]);
  await expect(card(page)).toHaveAttribute('data-input-bytes', '0');
});

test('oversized input is rejected in full before IPC and a smaller retry remains usable', async ({ page }) => {
  await workspace(page);
  await page.keyboard.insertText('한😀'.repeat(9363));
  expect(await page.evaluate(() => (window as any).flow.calls.filter((call: any) => call.command === 'session_write'))).toEqual([]);
  await expect(page.locator('.toast.error')).toContainText('한 번에 64KiB 이하');
  await page.keyboard.insertText('작은 입력');
  await expect.poll(() => page.evaluate(() => (window as any).flow.sent.map((item: any) => item.data))).toEqual(['작은 입력']);
});

test('project and generation changes discard pending input and cannot write to hidden or replacement sessions', async ({ page }) => {
  await workspace(page);
  await page.evaluate(() => { (window as any).flow.holdWrites = true; });
  await input(page).press('x'); await input(page).press('q');
  await page.locator('#project-tabs [role=tab]').filter({ hasText: 'Flow Beta' }).click();
  await page.evaluate(() => (window as any).flow.releaseWrites());
  await expect.poll(() => page.evaluate(() => (window as any).flow.sent.map((item: any) => item.data))).toEqual(['x']);
  await expect(card(page)).toBeHidden();
  await page.locator('#project-tabs [role=tab]').filter({ hasText: 'Flow Alpha' }).click();
  await input(page).focus();
  await page.evaluate(() => { (window as any).flow.holdWrites = true; });
  await input(page).press('y'); await input(page).press('o');
  await page.evaluate(() => (window as any).flow.emit({ id: 'flow-a', generation: 8 }));
  await input(page).press('n');
  await page.evaluate(() => (window as any).flow.releaseWrites());
  await expect.poll(() => page.evaluate(() => (window as any).flow.sent.map((item: any) => [item.generation, item.data]))).toEqual([[7, 'x'], [7, 'y'], [8, 'n']]);
});

test('disposing a terminal clears pending input while its one inflight request completes', async ({ page }) => {
  await workspace(page);
  await page.evaluate(() => { (window as any).flow.holdWrites = true; });
  await input(page).press('x'); await input(page).press('q');
  await page.evaluate(() => (window as any).flow.emit({ id: 'flow-a', state: 'stopped', active: false }));
  await card(page).getByLabel('이 세션 종료').click();
  await expect(card(page)).toHaveCount(0);
  await page.evaluate(() => (window as any).flow.releaseWrites());
  await expect.poll(() => page.evaluate(() => (window as any).flow.activeWrites)).toBe(0);
  expect(await page.evaluate(() => (window as any).flow.sent.map((item: any) => item.data))).toEqual(['x']);
});

test('held native resize keeps one request and the latest pending geometry, not an IPC chain', async ({ page }) => {
  await page.clock.install();
  await workspace(page);
  await expect.poll(() => page.evaluate(() => (window as any).flow.calls.some((call: any) => call.command === 'session_resize' && call.args.id === 'flow-a'))).toBe(true);
  await page.clock.pauseAt(await page.evaluate(() => Date.now() + 1000));
  const fitViewport = async (width: number) => {
    await page.setViewportSize({ width, height: 1000 });
    // Observe real layout before advancing the fit frame and its60ms debounce.
    // Wall-clock sleeps do not establish either of those completion boundaries.
    await card(page).locator('.terminal-host').evaluate(host => new Promise<void>(resolve => {
      const observer = new ResizeObserver(() => { observer.disconnect(); resolve(); });
      observer.observe(host);
    }));
    await page.clock.runFor(80);
  };
  await fitViewport(1440);
  await page.evaluate(() => { const flow = (window as any).flow; flow.calls = []; flow.holdResize = true; flow.maxResizes = 0; });
  await fitViewport(1600);
  for (let index = 1; index <= 8; index++) {
    await fitViewport(1600 + index * 40);
    expect(await page.evaluate(() => (window as any).flow.calls.filter((call: any) => call.command === 'session_resize' && call.args.id === 'flow-a').length)).toBe(1);
  }
  // The other equal-width column sends freely and supplies the final geometry.
  expect(await card(page).locator('.terminal-host').evaluate(host => host.clientWidth)).toBe(await card(page, 'flow-b').locator('.terminal-host').evaluate(host => host.clientWidth));
  const latest = await page.evaluate(() => (window as any).flow.calls.filter((call: any) => call.command === 'session_resize' && call.args.id === 'flow-b').at(-1).args);
  await page.evaluate(() => (window as any).flow.releaseResize());
  await expect.poll(() => page.evaluate(() => (window as any).flow.activeResizes)).toBe(0);
  const sizes = await page.evaluate(() => (window as any).flow.calls.filter((call: any) => call.command === 'session_resize' && call.args.id === 'flow-a').map((call: any) => call.args));
  expect(sizes).toHaveLength(2);
  expect(sizes[1]).toEqual({ ...latest, id: 'flow-a' });
  expect(sizes[1].cols).toBeGreaterThan(sizes[0].cols);
  expect(sizes.map((item: any) => item.generation)).toEqual([7, 7]);
  expect(await page.evaluate(() => (window as any).flow.maxResizes)).toBe(1);
});

test('queue disposal releases cancelled payload objects before its inflight request finishes', async ({ page }) => {
  await workspace(page);
  const initial = await page.evaluate(async () => {
    const modulePath = '/src/terminal-flow.ts';
    const { TerminalInputQueue, TerminalResizeQueue } = await import(modulePath);
    const probe: any = { sent: [], resized: [] };
    const gate = new Promise<void>(resolve => { probe.release = resolve; });
    probe.input = new TerminalInputQueue(async (value: any) => { await gate; probe.sent.push(value.data); }, () => {});
    probe.active = probe.input.enqueue({ data: 'inflight' });
    // Return from a synchronous fixture scope before GC. A top-level async
    // loop can itself retain its final local, independently of the queue.
    function fill(queue: any) {
      const refs = []; const promises = [];
      for (let index = 0; index < 127; index++) { const value = { data: 'pending-' + index }; refs.push(new WeakRef(value)); promises.push(queue.enqueue(value)); }
      return { refs, promises };
    }
    const pending = fill(probe.input); probe.inputRefs = pending.refs;
    probe.input.dispose(); await Promise.allSettled(pending.promises);
    probe.resize = new TerminalResizeQueue(async (value: any) => { await gate; probe.resized.push(value); }, (error: unknown) => { throw error; });
    probe.resize.enqueue({ cols: 80, rows: 24 });
    function sizes(queue: any) {
      const refs = [];
      for (let index = 0; index < 1000; index++) { const value = { cols: 81 + index, rows: 24 }; refs.push(new WeakRef(value)); queue.enqueue(value); }
      return refs;
    }
    probe.resizeRefs = sizes(probe.resize); probe.resize.dispose();
    (window as any).queueLifetime = probe;
    return { size: probe.input.size, bytes: probe.input.byteLength };
  });
  expect(initial).toEqual({ size: 1, bytes: 8 });
  const cdp = await page.context().newCDPSession(page);
  await cdp.send('HeapProfiler.collectGarbage'); await cdp.send('HeapProfiler.collectGarbage');
  expect(await page.evaluate(() => {
    const probe = (window as any).queueLifetime;
    return [...probe.inputRefs, ...probe.resizeRefs].filter(reference => reference.deref() !== undefined).length;
  })).toBe(0);
  await page.evaluate(() => (window as any).queueLifetime.release());
  await expect.poll(() => page.evaluate(() => (window as any).queueLifetime.input.size)).toBe(0);
  expect(await page.evaluate(() => (window as any).queueLifetime.sent)).toEqual(['inflight']);
  expect(await page.evaluate(() => (window as any).queueLifetime.resized)).toEqual([{ cols: 80, rows: 24 }]);
  await cdp.detach();
});
