import { test, expect, type Page } from '@playwright/test';

async function workspace(page: Page) {
  await page.addInitScript(() => {
    const state = window as any;
    state.isTauri = true;
    let callback = 0;
    state.mobileFixture = { calls: [], status: 'disabled', isolated: true, shared: false, hold: false, failure: false, release: () => {} };
    state.__TAURI_INTERNALS__ = {
      transformCallback: () => ++callback, unregisterCallback() {},
      async invoke(command: string, args: any = {}) {
        const fixture = state.mobileFixture; fixture.calls.push({ command, args });
        if (command.startsWith('plugin:event|')) return callback;
        if (command === 'get_ui_language') return 'ko';
        if (command === 'set_ui_language') return { show: 'OpenDIY Desktop 열기', exit: '종료…' };
        if (command === 'load_layout') return { version: 1, columns: 2, sessions: [] };
        if (command === 'list_projects') return [];
        if (command === 'list_sessions') return [
          { id: 'live', generation: 4, title: '실행 작업', cwd: 'D:\\fixture', active: true, state: 'idle' },
          { id: 'stopped', generation: 2, title: '종료 작업', cwd: 'D:\\fixture', active: false, state: 'exited' },
        ];
        if (command === 'mobile_control') {
          if (fixture.hold) { fixture.hold = false; await new Promise<void>(resolve => { fixture.release = resolve; }); }
          if (fixture.failure) throw new Error('이 실행 파일은 원격 제어를 지원하지 않습니다');
          if (args.action === 'enable') fixture.status = 'connected';
          if (args.action === 'disable') fixture.status = 'disabled';
          return { isolated: fixture.isolated, shared: fixture.shared, mutationsAllowed: fixture.isolated || fixture.shared, status: { status: fixture.status, serverName: '시험 서버', environmentId: 'test-environment' },
            pairing: args.action === 'pair' ? { manualCode: 'TEST-CODE-ONLY', expiresAt: Math.floor(Date.now() / 1000) + 120, environmentId: 'test-environment' } : null };
        }
        return null;
      },
    };
  });
  await page.goto('/');
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  if (await page.locator('#settings-open').count()) await page.locator('#settings-open').click();
  await page.locator('#mobile-control-open').click();
  await expect(page.locator('#mobile-control-session option')).toHaveCount(1);
}

test('mobile controls use the selected live generation and clear pairing codes on close and disable', async ({ page }, testInfo) => {
  await workspace(page);
  await expect(page.locator('#mobile-control-pair')).toBeDisabled();
  await page.locator('#mobile-control-enable').click();
  await expect(page.locator('#mobile-control-status')).toContainText('원격 서비스 연결됨');
  await page.locator('#mobile-control-pair').click();
  await expect(page.locator('#mobile-control-code')).toHaveText('TEST-CODE-ONLY');
  await page.screenshot({ path: testInfo.outputPath('mobile-control.png'), fullPage: true });
  await page.locator('#mobile-control-disable').click();
  await expect(page.locator('#mobile-control-code')).toBeEmpty();
  await expect(page.locator('#mobile-control-pairing')).toBeHidden();
  await expect(page.locator('#mobile-control-pair')).toBeDisabled();
  await page.locator('#mobile-control-close').click();
  await page.locator('#mobile-control-open').click();
  await expect(page.locator('#mobile-control-code')).toBeEmpty();
  expect(await page.evaluate(() => (window as any).mobileFixture.calls.filter((item: any) => item.command === 'mobile_control').map((item: any) => item.args))).toEqual([
    { id: 'live', generation: 4, action: 'status' },
    { id: 'live', generation: 4, action: 'enable' }, { id: 'live', generation: 4, action: 'pair' }, { id: 'live', generation: 4, action: 'disable' },
    { id: 'live', generation: 4, action: 'status' },
  ]);
});

test('unsupported mobile controls report the error without changing a running session', async ({ page }) => {
  await workspace(page);
  await page.evaluate(() => { (window as any).mobileFixture.failure = true; });
  await page.locator('#mobile-control-enable').click();
  await expect(page.locator('#mobile-control-error')).toContainText('지원하지 않습니다');
  await expect(page.locator('#mobile-control-enable')).toBeEnabled();
  await expect(page.locator('#mobile-control-pairing')).toBeHidden();
  expect(await page.evaluate(() => (window as any).mobileFixture.calls.filter((item: any) => ['start_session', 'session_stop', 'session_write'].includes(item.command)))).toEqual([]);
});

test('legacy CLI status explains isolation and prevents mobile mutations before a failed request', async ({ page }) => {
  await workspace(page);
  await page.evaluate(() => { (window as any).mobileFixture.isolated = false; });
  await page.locator('#mobile-control-refresh').click();
  await expect(page.locator('#mobile-control-compatibility')).toContainText('--remote-control-client-name');
  await expect(page.locator('#mobile-control-enable')).toBeDisabled();
  await expect(page.locator('#mobile-control-disable')).toBeDisabled();
  await expect(page.locator('#mobile-control-pair')).toBeDisabled();
  const actions = await page.evaluate(() => (window as any).mobileFixture.calls.filter((item: any) => item.command === 'mobile_control').map((item: any) => item.args.action));
  expect(actions.every((action: string) => action === 'status')).toBe(true);
  await page.locator('#mobile-control-cli-settings').click();
  await expect(page.locator('#mobile-control-dialog')).not.toBeVisible();
  await expect(page.locator('#codex-settings-dialog')).toBeVisible();
});

test('late pairing after close stays cleared and app exit waits for the pending mobile request', async ({ page }) => {
  await workspace(page);
  await page.locator('#mobile-control-enable').click();
  await expect(page.locator('#mobile-control-pair')).toBeEnabled();
  await page.evaluate(() => { (window as any).mobileFixture.hold = true; });
  await page.locator('#mobile-control-pair').click();
  await expect(page.locator('#mobile-control-open')).toBeDisabled();
  await page.locator('#mobile-control-close').click();
  await page.locator('#settings-close').click();
  await page.locator('#exit').click();
  await page.locator('#confirm-exit').click();
  await expect(page.locator('#confirm-exit')).toBeDisabled();
  expect(await page.evaluate(() => (window as any).mobileFixture.calls.some((item: any) => item.command === 'complete_exit'))).toBe(false);
  await page.evaluate(() => (window as any).mobileFixture.release());
  await expect.poll(() => page.evaluate(() => (window as any).mobileFixture.calls.some((item: any) => item.command === 'complete_exit'))).toBe(true);
  await expect(page.locator('#mobile-control-code')).toBeEmpty();
  await expect(page.locator('#mobile-control-pairing')).toBeHidden();
});

test('기존 공유 등록은 격리 여부와 별도로 제어를 허용하고 전체 서버 범위를 표시한다', async ({ page }) => {
  await workspace(page);
  await page.evaluate(() => { (window as any).mobileFixture.isolated = false; (window as any).mobileFixture.shared = true; });
  await page.locator('#mobile-control-refresh').click();
  await expect(page.locator('#mobile-control-compatibility')).toBeHidden();
  await expect(page.locator('#mobile-control-scope')).toContainText('모든 대화');
  await expect(page.locator('#mobile-control-scope')).toContainText('기존 원격 등록은 유지');
  await expect(page.locator('#mobile-control-enable')).toBeEnabled();
  await page.locator('#mobile-control-enable').click();
  await expect(page.locator('#mobile-control-pair')).toBeEnabled();
  await page.locator('#mobile-control-close').click();
  const actions = await page.evaluate(() => (window as any).mobileFixture.calls.filter((item: any) => item.command === 'mobile_control').map((item: any) => item.args.action));
  expect(actions).toEqual(['status', 'status', 'enable']);
});
