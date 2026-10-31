import { test, expect, type Page } from '@playwright/test';

const chosenPath = 'D:\\도구\\Codex CLI\\codex.exe';
const automaticPath = 'C:\\tools\\codex.exe';
async function mockSettings(page: Page, options: { holdChoose?: boolean; unavailable?: boolean; getFailure?: boolean } = {}) {
  await page.addInitScript(({ chosenPath, automaticPath, options }) => {
    const state = window as any;
    state.isTauri = true;
    let callback = 0;
    state.settingsFixture = {
      calls: [], choices: 0, releaseChoose: () => {}, getFailure: options.getFailure,
      settings: options.unavailable ? { savedPath: null, resolvedPath: null, source: 'unavailable', error: 'Codex 실행 파일을 찾을 수 없습니다' }
        : { savedPath: null, resolvedPath: automaticPath, source: 'environment', error: null },
    };
    state.__TAURI_INTERNALS__ = {
      transformCallback: () => ++callback, unregisterCallback() {},
      async invoke(command: string, args: any = {}) {
        const fixture = state.settingsFixture;
        fixture.calls.push({ command, args });
        if (command.startsWith('plugin:event|')) return callback;
        if (command === 'get_ui_language') return 'ko';
        if (command === 'set_ui_language') return { show: args.language === 'en' ? 'Open OpenDIY Desktop' : 'OpenDIY Desktop 열기', exit: args.language === 'en' ? 'Exit…' : '종료…' };
        if (command === 'load_layout') return { version: 1, columns: 2, sessions: [] };
        if (command === 'list_projects') return [];
        if (command === 'list_sessions') return [{ id: 'live-owned', generation: 9, title: '열린 작업', cwd: 'D:\\fixture', threadId: '01900000-0000-7000-8000-000000000001', state: 'idle', active: true }];
        if (command === 'get_codex_settings') { if (fixture.getFailure) throw new Error('설정 파일을 읽을 수 없습니다'); return fixture.settings; }
        if (command === 'choose_codex_executable') {
          const choice = ++fixture.choices;
          if (options.holdChoose) await new Promise<void>(resolve => { fixture.releaseChoose = resolve; });
          else { if (choice === 1) return null; if (choice === 2) throw new Error('유효한 Windows 실행 파일이 아닙니다'); }
          fixture.settings = { savedPath: chosenPath, resolvedPath: chosenPath, source: 'saved', error: null };
          return fixture.settings;
        }
        if (command === 'set_codex_executable') {
          if (args.path !== null) throw new Error('expected automatic reset');
          fixture.settings = { savedPath: null, resolvedPath: automaticPath, source: 'path', error: null };
          return fixture.settings;
        }
        return null;
      },
    };
  }, { chosenPath, automaticPath, options });
  await page.goto('/');
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await page.locator('#settings-open').click();
  await page.locator('#codex-settings-open').click();
}

test('executable picker cancel, invalid file, saved selection and automatic reset preserve the live session', async ({ page }, testInfo) => {
  await mockSettings(page);
  await expect(page.locator('#codex-executable-source')).toHaveText('환경변수');
  await expect(page.locator('#codex-executable-resolved')).toHaveText(automaticPath);
  await expect(page.locator('#codex-executable-path')).toHaveAttribute('readonly', '');
  await page.locator('#codex-executable-choose').click();
  await expect(page.locator('#codex-settings-status')).toContainText('취소');
  await expect(page.locator('#codex-executable-resolved')).toHaveText(automaticPath);
  await page.locator('#codex-executable-choose').click();
  await expect(page.locator('#codex-settings-error')).toHaveText('유효한 Windows 실행 파일이 아닙니다');
  await expect(page.locator('#codex-executable-path')).toHaveValue('');
  await expect(page.locator('#codex-executable-resolved')).toHaveText(automaticPath);
  await page.locator('#codex-executable-choose').click();
  await expect(page.locator('#codex-executable-path')).toHaveValue(chosenPath);
  await expect(page.locator('#codex-executable-resolved')).toHaveText(chosenPath);
  await expect(page.locator('#codex-executable-source')).toHaveText('직접 지정');
  await expect(page.locator('#codex-settings-error')).toBeHidden();
  await page.screenshot({ path: testInfo.outputPath('executable-settings.png'), fullPage: true });
  await page.locator('#codex-executable-reset').click();
  await expect(page.locator('#codex-executable-path')).toHaveValue('');
  await expect(page.locator('#codex-executable-source')).toHaveText('PATH 자동 찾기');
  const calls = await page.evaluate(() => (window as any).settingsFixture.calls);
  expect(calls.filter((call: any) => call.command === 'set_codex_executable')).toEqual([{ command: 'set_codex_executable', args: { path: null } }]);
  expect(calls.filter((call: any) => ['start_session', 'session_stop', 'complete_exit'].includes(call.command))).toEqual([]);
  await page.keyboard.press('Escape');
  await expect(page.locator('#codex-settings-dialog')).toBeHidden();
  await expect(page.locator('.terminal-card')).toHaveCount(1);
  await expect(page.locator('.terminal-identity')).toHaveAttribute('title', '01900000-0000-7000-8000-000000000001');
});

test('reopening settings waits for the outstanding picker and reads its saved result without a second mutation', async ({ page }) => {
  await mockSettings(page, { holdChoose: true });
  await expect(page.locator('#codex-executable-choose')).toBeEnabled();
  await page.locator('#codex-executable-choose').click();
  await expect(page.locator('#codex-executable-choose')).toBeDisabled();
  await page.locator('#codex-settings-close').click();
  await page.locator('#codex-settings-open').click();
  await expect(page.locator('#codex-executable-choose')).toBeDisabled();
  expect(await page.evaluate(() => (window as any).settingsFixture.calls.filter((call: any) => call.command === 'get_codex_settings').length)).toBe(1);
  await page.evaluate(() => (window as any).settingsFixture.releaseChoose());
  await expect(page.locator('#codex-executable-choose')).toBeEnabled();
  await expect(page.locator('#codex-executable-path')).toHaveValue(chosenPath);
  await expect(page.locator('#codex-executable-source')).toHaveText('직접 지정');
  expect(await page.evaluate(() => (window as any).settingsFixture.choices)).toBe(1);
  await page.locator('#codex-settings-close').click();
  await page.evaluate(() => { (window as any).settingsFixture.getFailure = true; });
  await page.locator('#codex-settings-open').click();
  await expect(page.locator('#codex-settings-error')).toContainText('설정 파일을 읽을 수 없습니다');
  await expect(page.locator('#codex-executable-path')).toHaveValue('');
  await expect(page.locator('#codex-executable-resolved')).toHaveText('확인하지 못했습니다');
});

for (const options of [{ unavailable: true }, { getFailure: true }]) {
  test(`settings reports ${options.unavailable ? 'missing executable' : 'read failure'} and allows recovery through the native chooser`, async ({ page }) => {
    await mockSettings(page, { ...options, holdChoose: true });
    await expect(page.locator('#codex-settings-error')).toContainText(options.unavailable ? '실행 파일을 찾을 수 없습니다' : '설정 파일을 읽을 수 없습니다');
    await expect(page.locator('#codex-executable-choose')).toBeEnabled();
    await page.locator('#codex-executable-choose').click();
    await page.evaluate(() => (window as any).settingsFixture.releaseChoose());
    await expect(page.locator('#codex-executable-resolved')).toHaveText(chosenPath);
    await expect(page.locator('#codex-settings-error')).toBeHidden();
  });
}
