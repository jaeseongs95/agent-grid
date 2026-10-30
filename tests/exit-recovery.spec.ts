import { test, expect, type Page } from '@playwright/test';

async function workspace(page: Page, available: boolean | string = true, queryError = false) {
  await page.addInitScript(({ available, queryError }) => {
    const state = window as any; state.isTauri = true;
    let callback = 0;
    const callbacks = new Map<number, (value: any) => void>();
    const listeners = new Map<string, (value: any) => void>();
    state.exitFixture = { calls: [], available, queryError, exitFailures: 10, holdExit: false, holdQuery: false, holdSave: false,
      releaseExit: () => {}, releaseQuery: () => {}, releaseSave: () => {},
      emitExit() { listeners.get('exit-requested')!({ event: 'exit-requested', id: 1, payload: null }); },
    };
    state.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
    state.__TAURI_INTERNALS__ = {
      transformCallback(handler: (value: any) => void) { callbacks.set(++callback, handler); return callback; }, unregisterCallback() {},
      async invoke(command: string, args: any = {}) {
        const fixture = state.exitFixture; fixture.calls.push({ command, args });
        if (command === 'plugin:event|listen') { listeners.set(args.event, callbacks.get(args.handler)!); return callback; }
        if (command.startsWith('plugin:event|')) return callback;
        if (command === 'get_ui_language') return 'ko';
        if (command === 'list_sessions' || command === 'list_projects') return [];
        if (command === 'load_layout') return { version: 1, columns: 2, sessions: [] };
        if (command === 'save_layout' && fixture.holdSave) await new Promise<void>(resolve => { fixture.releaseSave = () => { fixture.holdSave = false; resolve(); }; });
        if (command === 'complete_exit') {
          if (fixture.holdExit) await new Promise<void>(resolve => { fixture.releaseExit = () => { fixture.holdExit = false; resolve(); }; });
          if (fixture.exitFailures-- > 0) throw new Error('REMOTE_STOP_UNKNOWN');
        }
        if (command === 'unconfirmed_exit_available') {
          if (fixture.holdQuery) await new Promise<void>(resolve => { fixture.releaseQuery = () => { fixture.holdQuery = false; resolve(); }; });
          if (fixture.queryError) throw new Error('AVAILABILITY_QUERY_FAILED');
          return fixture.available;
        }
        return null;
      },
    };
  }, { available, queryError });
  await page.goto('/'); await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await page.clock.install(); await page.locator('#exit').click();
}

const calls = (page: Page, command: string) => page.evaluate(command => (window as any).exitFixture.calls.filter((item: any) => item.command === command), command);
const strict = (page: Page) => page.locator('#confirm-exit');
const fallback = (page: Page) => page.locator('#confirm-unconfirmed-exit');
const warning = (page: Page) => page.locator('#unconfirmed-exit-warning');
async function failStrict(page: Page, count = 1) {
  await strict(page).click();
  await expect.poll(async () => { await page.clock.runFor(500); return (await calls(page, 'unconfirmed_exit_available')).length; }).toBe(count);
  await expect(strict(page)).toBeEnabled();
}

test('일반 종료는 엄격한 기본 인자만 보내며 실패 후 가용성 true일 때만 별도 경고를 표시한다', async ({ page }) => {
  await workspace(page); await expect(fallback(page)).toBeHidden();
  expect(await calls(page, 'unconfirmed_exit_available')).toEqual([]);
  await failStrict(page); await expect(fallback(page)).toBeVisible(); await expect(warning(page)).toContainText('원격 작업이 계속');
  await page.clock.runFor(1000);
  expect((await calls(page, 'complete_exit')).map((item: any) => item.args)).toEqual([{ confirmed: true }]);
  await failStrict(page, 2);
  expect((await calls(page, 'complete_exit')).map((item: any) => item.args)).toEqual([{ confirmed: true }, { confirmed: true }]);
});

for (const [name, available, queryError] of [['false', false, false], ['조회 오류', true, true], ['비불리언 응답', 'true', false]] as const) {
  test(`가용성 ${name}에서는 복구 버튼을 숨기고 원래 종료 오류를 보존한다`, async ({ page }) => {
    await workspace(page, available, queryError); await failStrict(page);
    await expect(fallback(page)).toBeHidden(); await expect(warning(page)).toBeHidden();
    await expect(page.locator('.toast.error')).toContainText('REMOTE_STOP_UNKNOWN');
    await expect(page.locator('.toast.error')).not.toContainText('AVAILABILITY_QUERY_FAILED');
    expect((await calls(page, 'complete_exit')).map((item: any) => item.args)).toEqual([{ confirmed: true }]);
  });
}

test('명시적 복구 클릭만 true를 보내고 재실패하면 버튼과 취소를 다시 사용할 수 있다', async ({ page }) => {
  await workspace(page); await failStrict(page);
  await page.evaluate(() => { (window as any).exitFixture.holdExit = true; });
  await fallback(page).click();
  await expect.poll(async () => { await page.clock.runFor(500); return (await calls(page, 'complete_exit')).length; }).toBe(2);
  expect((await calls(page, 'complete_exit')).at(-1)?.args).toEqual({ confirmed: true, allowUnconfirmedRemote: true });
  await expect(strict(page)).toBeDisabled(); await expect(page.locator('#exit-dialog [data-close]').first()).toBeDisabled();
  await page.keyboard.press('Escape'); await expect(page.locator('#exit-dialog')).toBeVisible();
  await page.evaluate(() => (window as any).exitFixture.releaseExit());
  await expect(fallback(page)).toBeVisible(); await expect(fallback(page)).toBeEnabled(); await expect(strict(page)).toBeEnabled();
  await expect(page.locator('#exit-dialog [data-close]').first()).toBeEnabled();
  expect(await calls(page, 'complete_exit')).toHaveLength(2);
});

test('재시도는 이전 가용성을 지우고 새 조회 결과를 기다린다', async ({ page }) => {
  await workspace(page); await failStrict(page); await expect(fallback(page)).toBeVisible();
  await page.evaluate(() => { const fixture = (window as any).exitFixture; fixture.available = false; fixture.holdQuery = true; });
  await strict(page).click(); await expect(fallback(page)).toBeHidden(); await expect(warning(page)).toBeHidden();
  await expect.poll(async () => { await page.clock.runFor(500); return (await calls(page, 'unconfirmed_exit_available')).length; }).toBe(2);
  await page.evaluate(() => (window as any).exitFixture.releaseQuery());
  await expect(strict(page)).toBeEnabled(); await expect(fallback(page)).toBeHidden();
  expect((await calls(page, 'complete_exit')).every((item: any) => item.args.allowUnconfirmedRemote === undefined)).toBe(true);
});

test('취소·재열기와 트레이 종료 요청은 이전 복구 선택을 초기화한다', async ({ page }) => {
  await workspace(page); await failStrict(page); await expect(fallback(page)).toBeVisible();
  await page.keyboard.press('Escape'); await expect(page.locator('#exit-dialog')).toBeHidden();
  await page.locator('#exit').click(); await expect(fallback(page)).toBeHidden(); await expect(warning(page)).toBeHidden();
  await failStrict(page, 2); await expect(fallback(page)).toBeVisible();
  await page.getByRole('button', { name: '계속 작업', exact: true }).click();
  await page.evaluate(() => (window as any).exitFixture.emitExit());
  await expect(page.locator('#exit-dialog')).toBeVisible(); await expect(fallback(page)).toBeHidden(); await expect(warning(page)).toBeHidden();
});

test('복구 경고와 버튼은 실제 설정 언어 전환을 반영한다', async ({ page }) => {
  await workspace(page); await failStrict(page); await expect(fallback(page)).toHaveText('중단 미확인 상태로 앱 닫기');
  for (const [language, label, text] of [['en', 'Close app with unconfirmed remote stop', 'Remote work may continue'], ['ko', '중단 미확인 상태로 앱 닫기', '원격 작업이 계속']] as const) {
    await page.keyboard.press('Escape'); await page.locator('#settings-open').click(); await page.locator('#language-select').selectOption(language); await page.locator('#settings-close').click();
    await page.locator('#exit').click(); await failStrict(page, language === 'en' ? 2 : 3);
    await expect(fallback(page)).toHaveText(label); await expect(warning(page)).toContainText(text);
  }
});

test('명시적 복구도 저장 완료를 기다린 뒤 종료 인자를 한 번만 보낸다', async ({ page }) => {
  await workspace(page); await failStrict(page);
  await page.evaluate(() => { const fixture = (window as any).exitFixture; fixture.holdSave = true; fixture.exitFailures = 0; });
  await fallback(page).click(); await page.clock.runFor(500);
  expect(await calls(page, 'complete_exit')).toHaveLength(1);
  await expect(strict(page)).toBeDisabled();
  await page.evaluate(() => (window as any).exitFixture.releaseSave());
  await expect.poll(() => calls(page, 'complete_exit').then(items => items.length)).toBe(2);
  expect((await calls(page, 'complete_exit')).at(-1)?.args).toEqual({ confirmed: true, allowUnconfirmedRemote: true });
});
