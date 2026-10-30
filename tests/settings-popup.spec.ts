import { test, expect, type Page } from '@playwright/test';

async function workspace(page: Page) {
  await page.addInitScript(() => {
    const state = window as any;
    state.isTauri = true;
    let callback = 0;
    const projects = Array.from({ length: 20 }, (_, index) => ({ id: `project-${index}`, name: `프로젝트 ${index + 1}`, cwd: `D:\\시험\\${index}`, createdAt: 1700000000, favorite: false }));
    state.settingsPopupCalls = [];
    state.__TAURI_INTERNALS__ = {
      transformCallback: () => ++callback, unregisterCallback() {},
      async invoke(command: string, args: any = {}) {
        state.settingsPopupCalls.push({ command, args });
        if (command.startsWith('plugin:event|')) return callback;
        if (command === 'get_ui_language') return 'ko';
        if (command === 'set_ui_language') return { show: 'OpenDIY Desktop 열기', exit: '종료…' };
        if (command === 'load_layout') return { version: 1, columns: 2, sessions: [] };
        if (command === 'list_projects') return projects;
        if (command === 'list_sessions') return Array.from({ length: 12 }, (_, index) => ({ id: `session-${index}`, title: `열린 작업 ${index + 1}`, cwd: projects[0].cwd, generation: 1, state: 'idle', active: true }));
        if (command === 'list_external') return [];
        if (command === 'get_codex_settings') return { savedPath: null, resolvedPath: 'D:\\도구\\codex.exe', source: 'path', error: null };
        if (command === 'weekly_usage') return { executable: 'D:\\도구\\codex.exe', observedAt: Date.now(), limitName: '시험 계정', weekly: { usedPercent: 25, resetsAt: 1900000000 } };
        return null;
      },
    };
  });
  await page.goto('/');
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
}

test('설정 팝업으로 진입하고 하위 창을 닫으면 키보드 초점이 돌아온다', async ({ page }, testInfo) => {
  await workspace(page);
  for (const selector of ['#codex-settings-open', '#extensions-open', '#mobile-control-open', '#external-toggle', '#language-select']) {
    await expect(page.locator(`.sidebar ${selector}`)).toHaveCount(0);
    await expect(page.locator(`#settings-dialog ${selector}`)).toBeHidden();
  }
  await page.locator('#settings-open').focus();
  await page.keyboard.press('Enter');
  const settings = page.getByRole('dialog', { name: '설정', exact: true });
  await expect(settings).toBeVisible();
  for (const [button, dialog, close] of [
    ['#codex-settings-open', '#codex-settings-dialog', '#codex-settings-close'],
    ['#extensions-open', '#extensions-dialog', '#extensions-close'],
    ['#mobile-control-open', '#mobile-control-dialog', '#mobile-control-close'],
    ['#external-toggle', '#external-dialog', '#external-dialog [data-close]'],
  ]) {
    await settings.locator(button).focus();
    await page.keyboard.press('Enter');
    await expect(page.locator(dialog)).toBeVisible();
    // Escape dismisses only the topmost native modal.
    await page.keyboard.press('Escape');
    await expect(page.locator(dialog)).toBeHidden();
    await expect(settings).toBeVisible();
    await expect(settings.locator(button)).toBeFocused();
    await settings.locator(button).click();
    await expect(page.locator(dialog)).toBeVisible();
    await page.locator(close).click();
    await expect(page.locator(dialog)).toBeHidden();
    await expect(settings.locator(button)).toBeFocused();
  }
  await page.screenshot({ path: testInfo.outputPath('settings-popup.png'), fullPage: true });
  await page.keyboard.press('Escape');
  await expect(settings).toBeHidden();
  await expect(page.locator('#settings-open')).toBeFocused();
  expect(await page.evaluate(() => (window as any).settingsPopupCalls.filter((call: any) => ['session_stop', 'start_session', 'complete_exit'].includes(call.command)))).toEqual([]);
});

test('설정 안의 언어 선택과 닫기 버튼을 키보드로 사용한다', async ({ page }) => {
  await workspace(page);
  await page.locator('#settings-open').click();
  await page.locator('#language-select').selectOption('en');
  await expect(page.getByRole('dialog', { name: 'Settings', exact: true })).toBeVisible();
  await expect(page.locator('#settings-open')).toHaveAccessibleName('Open settings');
  await expect(page.locator('#settings-close')).toHaveAccessibleName('Close settings');
  await expect(page.locator('#language-select')).toHaveAccessibleName('Language');
  await page.locator('#settings-close').focus();
  await page.keyboard.press('Enter');
  await expect(page.locator('#settings-dialog')).toBeHidden();
  await expect(page.locator('#settings-open')).toBeFocused();
  await page.locator('#settings-open').click();
  await expect(page.locator('#language-select')).toHaveValue('en');
});

test('최소 창에서 목록과 하단 사용량을 유지하고 외부 세션은 설정에서 연다', async ({ page }, testInfo) => {
  await page.setViewportSize({ width: 800, height: 540 });
  await workspace(page);
  await expect(page.locator('.workspace-label, .workspace-choice, .sidebar-bottom, #connection, #footer-state, .workspace-title')).toHaveCount(0);
  await expect(page.locator('.sidebar .brand + .sidebar-lists')).toBeVisible();
  await expect(page.locator('#projects .project-row')).toHaveCount(20);
  await expect(page.locator('#session-list .session-link')).toHaveCount(12);
  const geometry = await page.evaluate(() => {
    const side = document.querySelector('.sidebar')!;
    return { sidebarOverflow: side.scrollHeight - side.clientHeight, width: document.documentElement.scrollWidth,
      lists: ['projects', 'session-list'].map(id => { const node = document.getElementById(id)!; return { height: node.clientHeight, scrollable: node.scrollHeight > node.clientHeight, max: node.scrollHeight - node.clientHeight }; }) };
  });
  expect(geometry.sidebarOverflow).toBeLessThanOrEqual(1);
  expect(geometry.width).toBe(800);
  for (const list of geometry.lists) { expect(list.height).toBeGreaterThanOrEqual(55); expect(list.scrollable).toBe(true); }
  const fixed = await page.locator('#weekly-usage').boundingBox();
  for (const id of ['projects', 'session-list']) {
    await page.locator(`#${id}`).evaluate(node => { node.scrollTop = node.scrollHeight; });
    expect(await page.locator(`#${id}`).evaluate(node => node.scrollTop)).toBeGreaterThan(0);
  }
  expect(await page.locator('#weekly-usage').boundingBox()).toEqual(fixed);
  for (const selector of ['#weekly-usage', '#settings-open', '#exit']) {
    await expect(page.locator(selector)).toBeVisible();
    const box = (await page.locator(selector).boundingBox())!;
    expect(box.y + box.height).toBeLessThanOrEqual(540);
    expect(box.x + box.width).toBeLessThanOrEqual(800);
  }
  await page.screenshot({ path: testInfo.outputPath('sidebar-minimum-window.png'), fullPage: true });
  await expect(page.locator('.sidebar #external-toggle')).toHaveCount(0);
  expect(await page.evaluate(() => (window as any).settingsPopupCalls.filter((call: any) => call.command === 'list_external'))).toEqual([]);
  await page.locator('#settings-open').click();
  await page.locator('#external-toggle').click();
  await expect(page.locator('#external-dialog')).toBeVisible();
  await expect(page.locator('#external-list')).toHaveText('발견된 외부 기록이 없습니다.');
  expect(await page.evaluate(() => (window as any).settingsPopupCalls.filter((call: any) => call.command === 'list_external').length)).toBe(1);
});
