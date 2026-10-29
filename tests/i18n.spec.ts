import { test, expect, type Page } from '@playwright/test';

const project = { id: 'user-project', name: '사용자 프로젝트', cwd: 'D:\\사용자\\프로젝트', createdAt: 1700000000 };
const session = { id: 'owned-i18n', generation: 9, title: '사용자 지정 제목', cwd: project.cwd, threadId: '01900000-0000-7000-8000-000000000001', state: 'idle', active: true };
const catalogRows = [0, 1].map(index => ({ id: `catalog-${index}`, title: `사용자 대화 ${index}`, cwd: project.cwd, createdAt: 1700000000, updatedAt: 1700000300, status: 'idle' }));
const savedPath = 'D:\\사용자\\CLI\\codex.exe';

async function nativeFixture(page: Page, options: { stored?: string; nativeLanguage?: 'ko' | 'en'; holdLanguageRead?: boolean; failLanguageRead?: boolean; empty?: boolean; holdSettings?: boolean; holdChooser?: boolean; holdList?: boolean; holdPreview?: boolean } = {}) {
  await page.addInitScript(({ project, session, catalogRows, savedPath, options }) => {
    const state = window as any;
    // Seed only the first load so a real reload must use the app's saved preference.
    if (!sessionStorage.getItem('i18n-fixture-seeded')) {
      if (options.stored !== undefined) localStorage.setItem('agent-grid-language', options.stored);
      // Native-storage substitute is deliberately separate from the WebView cache.
      // This is a browser integration fixture, not evidence of SQLite durability.
      sessionStorage.setItem('fixture-native-language', options.nativeLanguage ?? 'ko');
      sessionStorage.setItem('i18n-fixture-seeded', '1');
    }
    state.isTauri = true;
    let callback = 0;
    const callbacks = new Map<number, (value: any) => void>();
    const listeners = new Map<string, (value: any) => void>();
    state.i18nFixture = { calls: [], releaseSettings: () => {}, releaseChooser: () => {}, releaseList: () => {}, releasePreview: () => {}, releaseLanguageRead: () => {}, holdLanguageSaves: false, pendingLanguageSaves: [], completedLanguageSaves: [], activeLanguageSaves: 0, maxLanguageSaves: 0, holdCompleteExit: false, releaseCompleteExit: () => {},
      finishLanguageSave(fail = false) { const pending = this.pendingLanguageSaves.shift(); if (!pending) throw new Error('No pending language save'); if (fail) pending.reject(new Error('Fixture language database write failed')); else pending.resolve(); },
      emit(event: string, payload: any) { listeners.get(event)!({ event, id: 1, payload }); },
    };
    const settings = { savedPath, resolvedPath: savedPath, source: 'saved', error: null };
    state.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
    state.__TAURI_INTERNALS__ = {
      transformCallback(handler: (value: any) => void) { callbacks.set(++callback, handler); return callback; }, unregisterCallback() {},
      async invoke(command: string, args: any = {}) {
        const fixture = state.i18nFixture;
        fixture.calls.push({ command, args });
        if (command === 'plugin:event|listen') { listeners.set(args.event, callbacks.get(args.handler)!); return callback; }
        if (command.startsWith('plugin:event|')) return callback;
        if (command === 'load_layout') return { version: 1, columns: 2, sessions: [] };
        if (command === 'list_sessions') return options.empty ? [] : [session];
        if (command === 'list_projects') return [project];
        if (command === 'get_ui_language') {
          if (options.holdLanguageRead) await new Promise<void>(resolve => { fixture.releaseLanguageRead = resolve; });
          if (options.failLanguageRead) throw new Error('Fixture language database read failed');
          return sessionStorage.getItem('fixture-native-language');
        }
        if (command === 'set_ui_language') {
          if (!['ko', 'en'].includes(args.language)) throw new Error('Unsupported language');
          fixture.activeLanguageSaves++; fixture.maxLanguageSaves = Math.max(fixture.maxLanguageSaves, fixture.activeLanguageSaves);
          try {
            if (fixture.holdLanguageSaves) await new Promise<void>((resolve, reject) => { fixture.pendingLanguageSaves.push({ resolve, reject }); });
            sessionStorage.setItem('fixture-native-language', args.language); fixture.completedLanguageSaves.push(args.language);
            return null;
          } finally { fixture.activeLanguageSaves--; }
        }
        if (command === 'complete_exit' && fixture.holdCompleteExit) await new Promise<void>(resolve => { fixture.releaseCompleteExit = resolve; });
        if (command === 'get_codex_settings') { if (options.holdSettings) await new Promise<void>(resolve => { fixture.releaseSettings = resolve; }); return settings; }
        if (command === 'choose_codex_executable') { if (options.holdChooser) await new Promise<void>(resolve => { fixture.releaseChooser = resolve; }); return settings; }
        if (command === 'catalog_list') { if (options.holdList) await new Promise<void>(resolve => { fixture.releaseList = resolve; }); return { sessions: catalogRows, nextCursor: null }; }
        if (command === 'catalog_preview') { if (options.holdPreview) await new Promise<void>(resolve => { fixture.releasePreview = resolve; }); return { messages: [{ id: args.threadId + '-message', role: 'user', text: '사용자가 작성한 대화 원문' }], nextCursor: null }; }
        return null;
      },
    };
  }, { project, session, catalogRows, savedPath, options });
  await page.goto('/');
  if (options.holdLanguageRead) {
    await expect.poll(() => page.evaluate(() => (window as any).i18nFixture.calls.filter((call: any) => call.command === 'get_ui_language').length)).toBe(1);
    return;
  }
  const language = options.failLanguageRead ? options.stored === 'en' ? 'en' : 'ko' : options.nativeLanguage ?? 'ko';
  await expect(page.locator('#connection')).toHaveText(language === 'en' ? /connected/i : '로컬 연결됨');
  await expect(page.locator('#projects')).toContainText(project.name);
}

const commands = (page: Page, names: string[]) => page.evaluate(names => (window as any).i18nFixture.calls.filter((call: any) => names.includes(call.command)), names);
async function switchLanguage(page: Page, language: 'ko' | 'en') {
  await page.locator('#language-select').selectOption(language);
  await expect(page.locator('html')).toHaveAttribute('lang', language);
  await expect(page.locator('#language-select')).toHaveValue(language);
}
const processActions = ['start_session', 'session_stop', 'session_write', 'complete_exit', 'list_external', 'choose_project', 'set_codex_executable'];

for (const stored of [undefined, 'invalid-language', 'EN', '']) test(`language defaults to Korean for ${stored === undefined ? 'no preference' : JSON.stringify(stored)}`, async ({ page }) => {
  await nativeFixture(page, { stored });
  expect(await page.locator('#language-select option').evaluateAll(options => options.map(option => (option as HTMLOptionElement).value))).toEqual(['ko', 'en']);
  await expect(page.locator('#language-select')).toHaveValue('ko');
  await expect(page.locator('html')).toHaveAttribute('lang', 'ko');
  await expect(page.locator('#new-session')).toContainText('새 세션');
  await expect(page.locator('.session-state')).toHaveText('입력 가능');
});

test('switch applies immediately and the selected language survives a real reload', async ({ page }) => {
  await nativeFixture(page);
  await switchLanguage(page, 'en');
  await expect(page.locator('#new-session')).toContainText(/new session/i);
  expect(await page.evaluate(() => localStorage.getItem('agent-grid-language'))).toBe('en');
  await expect(page.locator('#language-error')).toBeHidden();
  await page.reload();
  await expect(page.locator('#language-select')).toHaveValue('en');
  await expect(page.locator('#connection')).toHaveText(/connected/i);
  await expect(page.locator('#new-session')).toContainText(/new session/i);
  await switchLanguage(page, 'ko');
  await expect(page.locator('#language-error')).toBeHidden();
  await page.reload();
  await expect(page.locator('#new-session')).toContainText('새 세션');
  expect(await page.evaluate(() => localStorage.getItem('agent-grid-language'))).toBe('ko');
});

test('live terminals, user content, selection and drafts survive repeated language changes without process actions', async ({ page }) => {
  await nativeFixture(page);
  const card = page.locator(`.terminal-card[data-id="${session.id}"]`);
  await page.evaluate(({ session }) => {
    const fixture = (window as any).i18nFixture;
    fixture.emit('terminal-output', { id: session.id, generation: session.generation, seq: 1, data: 'USER_OUTPUT 한글 원문 ENGLISH\r\n' });
    fixture.card = document.querySelector('.terminal-card'); fixture.xterm = fixture.card.querySelector('.xterm-screen');
  }, { session });
  await expect(card.locator('.xterm-rows')).toContainText('USER_OUTPUT');
  const output = await card.locator('.xterm-rows').innerText();
  await page.locator('#project-tabs [role=tab]').filter({ hasText: project.name }).click();
  await card.locator('input[type=checkbox]').check();
  await page.locator('#broadcast-input').fill('전송하지 않은 초안');
  await page.locator('#search-toggle').click();
  await page.locator('#terminal-search').fill('USER_OUTPUT');
  await expect(page.locator('#search-result')).toHaveText('결과 찾음');
  const before = await commands(page, processActions);
  for (const language of ['en', 'ko', 'en'] as const) await switchLanguage(page, language);
  await expect(card.locator('.session-state')).toHaveText(/ready|idle|input/i);
  await expect(card.locator('.maximize')).toHaveAttribute('aria-label', /maximize/i);
  await expect(card.locator('.stop')).toHaveAttribute('aria-label', /stop|close|end/i);
  await expect(card.locator('input')).toHaveAttribute('aria-label', /select|broadcast|send to this session/i);
  await expect(page.locator('#broadcast-input')).toHaveAttribute('placeholder', /session/i);
  await expect(page.locator('#terminal-search')).toHaveAttribute('placeholder', /search/i);
  await expect(page.locator('#search-result')).toHaveText(/found/i);
  await expect(page.locator('#project-tabs [role=tab]').first()).toContainText(/all/i);
  await expect(page.locator('#project-tabs [role=tab][aria-selected=true]')).toContainText(project.name);
  await expect(page.locator('#selected-count')).toContainText(/selected/i);
  await expect(card.locator('.terminal-title')).toHaveText(session.title);
  await expect(card.locator('.terminal-path')).toContainText(session.cwd);
  await expect(card.locator('.terminal-identity')).toHaveAttribute('title', session.threadId);
  await expect(card.locator('input')).toBeChecked();
  await expect(page.locator('#broadcast-input')).toHaveValue('전송하지 않은 초안');
  await expect(page.locator('#terminal-search')).toHaveValue('USER_OUTPUT');
  expect(await card.locator('.xterm-rows').innerText()).toBe(output);
  expect(await page.evaluate(() => { const f = (window as any).i18nFixture; return f.card.isConnected && f.xterm.isConnected && f.card.querySelector('.xterm-screen') === f.xterm; })).toBe(true);
  expect(await commands(page, processActions)).toEqual(before);
  expect((await commands(page, ['set_ui_language'])).at(-1)).toEqual({ command: 'set_ui_language', args: { language: 'en' } });
});

test('empty state and open new-session labels change while all user form values remain intact', async ({ page }) => {
  await nativeFixture(page, { empty: true });
  await switchLanguage(page, 'en');
  await expect(page.locator('#empty h2')).not.toHaveText(/[가-힣]/);
  await expect(page.locator('#empty-add')).toContainText(/session/i);
  await page.locator('#new-session').click();
  await page.locator('#session-title').fill('이름 초안');
  await page.locator('#session-cwd').fill(project.cwd);
  await page.locator('#session-thread').fill('사용자-스레드-초안');
  await switchLanguage(page, 'ko');
  await expect(page.locator('label[for=session-title]')).toHaveText('세션 이름');
  await switchLanguage(page, 'en');
  await expect(page.locator('label[for=session-title]')).toHaveText(/session name/i);
  await expect(page.locator('label[for=session-cwd]')).toHaveText(/project path/i);
  await expect(page.locator('#session-title')).toHaveAttribute('placeholder', /API/i);
  await expect(page.locator('#session-thread')).toHaveAttribute('placeholder', /new|empty|blank/i);
  await expect(page.locator('#create-submit')).toContainText(/start session/i);
  await expect(page.locator('#session-title')).toHaveValue('이름 초안');
  await expect(page.locator('#session-cwd')).toHaveValue(project.cwd);
  await expect(page.locator('#session-thread')).toHaveValue('사용자-스레드-초안');
  expect(await commands(page, processActions)).toEqual([]);
});

for (const nativeLanguage of ['en', 'ko'] as const) test(`native language ${nativeLanguage} wins over a stale WebView cache before the first native save`, async ({ page }) => {
  const stale = nativeLanguage === 'en' ? 'ko' : 'en';
  await nativeFixture(page, { stored: stale, nativeLanguage });
  await expect(page.locator('#language-select')).toHaveValue(nativeLanguage);
  await expect(page.locator('html')).toHaveAttribute('lang', nativeLanguage);
  expect(await page.evaluate(() => localStorage.getItem('agent-grid-language'))).toBe(nativeLanguage);
  const calls = await commands(page, ['get_ui_language', 'set_ui_language']);
  expect(calls[0].command).toBe('get_ui_language');
  expect(calls.filter((call: any) => call.command === 'set_ui_language')).toEqual([{ command: 'set_ui_language', args: { language: nativeLanguage } }]);
});

test('pending native startup language read cannot overwrite the durable preference with the cached default', async ({ page }) => {
  await nativeFixture(page, { stored: 'ko', nativeLanguage: 'en', holdLanguageRead: true });
  await expect(page.locator('#language-select')).toBeDisabled();
  expect(await commands(page, ['set_ui_language'])).toEqual([]);
  expect(await page.evaluate(() => sessionStorage.getItem('fixture-native-language'))).toBe('en');
  await page.evaluate(() => (window as any).i18nFixture.releaseLanguageRead());
  await expect(page.locator('#connection')).toHaveText(/connected/i);
  await expect(page.locator('#language-select')).toHaveValue('en');
  await expect(page.locator('#language-error')).toBeHidden();
  expect(await commands(page, ['set_ui_language'])).toEqual([{ command: 'set_ui_language', args: { language: 'en' } }]);
});

test('failed native startup language read is visible and does not automatically write; a deliberate selection recovers', async ({ page }) => {
  await nativeFixture(page, { stored: 'ko', nativeLanguage: 'en', failLanguageRead: true });
  await expect(page.locator('#language-error')).toBeVisible();
  await expect(page.locator('#language-error')).toContainText('Fixture language database read failed');
  await expect(page.locator('#language-select')).toBeEnabled();
  expect(await commands(page, ['set_ui_language'])).toEqual([]);
  expect(await page.evaluate(() => sessionStorage.getItem('fixture-native-language'))).toBe('en');
  await switchLanguage(page, 'en');
  await expect(page.locator('#language-error')).toBeHidden();
  expect(await commands(page, ['set_ui_language'])).toEqual([{ command: 'set_ui_language', args: { language: 'en' } }]);
  await expect(page.locator('#new-session')).toBeEnabled();
});

test('rapid English then Korean changes save captured values serially and leave the latest native preference', async ({ page }) => {
  await nativeFixture(page);
  await page.evaluate(() => { (window as any).i18nFixture.holdLanguageSaves = true; });
  await switchLanguage(page, 'en');
  await expect.poll(async () => (await commands(page, ['set_ui_language'])).length).toBe(2);
  await switchLanguage(page, 'ko');
  await expect(page.locator('#language-error')).toBeVisible();
  expect((await commands(page, ['set_ui_language'])).map((call: any) => call.args.language)).toEqual(['ko', 'en']);
  await page.evaluate(() => (window as any).i18nFixture.finishLanguageSave());
  await expect.poll(async () => (await commands(page, ['set_ui_language'])).length).toBe(3);
  expect((await commands(page, ['set_ui_language'])).map((call: any) => call.args.language)).toEqual(['ko', 'en', 'ko']);
  await expect(page.locator('#language-error')).toBeVisible();
  await page.evaluate(() => (window as any).i18nFixture.finishLanguageSave());
  await expect(page.locator('#language-error')).toBeHidden();
  expect(await page.evaluate(() => sessionStorage.getItem('fixture-native-language'))).toBe('ko');
  expect(await page.evaluate(() => (window as any).i18nFixture.maxLanguageSaves)).toBe(1);
  expect(await commands(page, processActions)).toEqual([]);
});

test('failed native language save remains visible and the next explicit change can recover without restarting sessions', async ({ page }) => {
  await nativeFixture(page);
  await page.evaluate(() => { (window as any).i18nFixture.holdLanguageSaves = true; });
  await switchLanguage(page, 'en');
  await expect.poll(async () => (await commands(page, ['set_ui_language'])).length).toBe(2);
  await page.evaluate(() => (window as any).i18nFixture.finishLanguageSave(true));
  await expect(page.locator('#language-error')).toHaveAttribute('role', 'alert');
  await expect(page.locator('#language-error')).toContainText('Fixture language database write failed');
  expect(await page.evaluate(() => sessionStorage.getItem('fixture-native-language'))).toBe('ko');
  await switchLanguage(page, 'ko');
  await expect.poll(async () => (await commands(page, ['set_ui_language'])).length).toBe(3);
  await page.evaluate(() => (window as any).i18nFixture.finishLanguageSave());
  await expect(page.locator('#language-error')).toBeHidden();
  await expect(page.locator('.terminal-card')).toHaveCount(1);
  expect(await commands(page, processActions)).toEqual([]);
});

test('confirmed exit waits for the latest serialized language save and keeps selection disabled until native exit settles', async ({ page }) => {
  await nativeFixture(page);
  await page.evaluate(() => { const fixture = (window as any).i18nFixture; fixture.holdLanguageSaves = true; fixture.holdCompleteExit = true; });
  await switchLanguage(page, 'en');
  await expect.poll(async () => (await commands(page, ['set_ui_language'])).length).toBe(2);
  await switchLanguage(page, 'ko');
  await page.locator('#exit').click();
  await page.locator('#confirm-exit').click();
  await expect.poll(async () => (await commands(page, ['save_layout'])).length).toBe(1);
  expect(await commands(page, ['complete_exit'])).toEqual([]);
  await page.evaluate(() => (window as any).i18nFixture.finishLanguageSave());
  await expect.poll(async () => (await commands(page, ['set_ui_language'])).length).toBe(3);
  expect(await commands(page, ['complete_exit'])).toEqual([]);
  await page.evaluate(() => (window as any).i18nFixture.finishLanguageSave());
  await expect.poll(async () => (await commands(page, ['complete_exit'])).length).toBe(1);
  expect(await page.evaluate(() => sessionStorage.getItem('fixture-native-language'))).toBe('ko');
  await expect(page.locator('#language-select')).toBeDisabled();
  await expect(page.locator('#confirm-exit')).toBeDisabled();
  await page.evaluate(() => (window as any).i18nFixture.releaseCompleteExit());
  await expect(page.locator('#confirm-exit')).toBeEnabled();
});

test('pending settings response and pending native picker finish in the current language without another request', async ({ page }) => {
  await nativeFixture(page, { holdSettings: true, holdChooser: true });
  await page.locator('#codex-settings-open').click();
  await expect(page.locator('#codex-settings-status')).toContainText('확인');
  await switchLanguage(page, 'en');
  await expect(page.locator('#codex-settings-status')).not.toHaveText(/[가-힣]/);
  await page.evaluate(() => (window as any).i18nFixture.releaseSettings());
  await expect(page.locator('#codex-executable-path')).toHaveValue(savedPath);
  await expect(page.locator('#codex-executable-source')).toHaveText(/saved|specified|custom|selected file/i);
  await expect(page.locator('#codex-settings-close')).toHaveAttribute('aria-label', /close/i);
  expect(await commands(page, ['get_codex_settings'])).toHaveLength(1);
  await page.locator('#codex-executable-choose').click();
  await expect(page.locator('#codex-executable-choose')).toBeDisabled();
  await switchLanguage(page, 'ko');
  await expect(page.locator('#codex-settings-status')).toContainText('기다리는 중');
  await page.evaluate(() => (window as any).i18nFixture.releaseChooser());
  await expect(page.locator('#codex-settings-status')).toHaveText('설정이 저장되었습니다.');
  await switchLanguage(page, 'en');
  await expect(page.locator('#codex-settings-status')).toHaveText(/saved/i);
  await expect(page.locator('#codex-executable-path')).toHaveValue(savedPath);
  await expect(page.locator('#codex-executable-resolved')).toHaveText(savedPath);
  expect(await commands(page, ['get_codex_settings', 'choose_codex_executable'])).toHaveLength(2);
  expect(await commands(page, processActions)).toEqual([]);
});

test('pending catalog list and preview use current language, preserving query, selection and original messages', async ({ page }) => {
  await nativeFixture(page, { holdList: true, holdPreview: true });
  await page.locator('#catalog-open').click();
  await expect.poll(async () => (await commands(page, ['catalog_list'])).length).toBe(1);
  await switchLanguage(page, 'en');
  await expect(page.locator('#catalog-list-status')).toHaveText(/search|finding|loading/i);
  await page.evaluate(() => (window as any).i18nFixture.releaseList());
  await expect(page.locator('#catalog-list [role=option]')).toHaveCount(2);
  await expect.poll(async () => (await commands(page, ['catalog_preview'])).length).toBe(1);
  await expect(page.locator('#catalog-list-status')).toContainText(/2.*session/i);
  await switchLanguage(page, 'ko');
  await expect(page.locator('#catalog-preview-status')).toContainText('불러오는 중');
  await page.evaluate(() => (window as any).i18nFixture.releasePreview());
  await expect(page.locator('#catalog-preview-status')).toContainText('읽기 전용');
  await page.locator('#catalog-search').fill('사용자 검색 초안');
  await expect.poll(async () => (await commands(page, ['catalog_list'])).length).toBe(2);
  await page.evaluate(() => (window as any).i18nFixture.releaseList());
  await expect.poll(async () => (await commands(page, ['catalog_preview'])).length).toBe(2);
  await page.evaluate(() => (window as any).i18nFixture.releasePreview());
  await expect(page.locator('#catalog-preview-status')).toContainText('읽기 전용');
  await page.locator('#catalog-list [role=option]').nth(1).click();
  await expect.poll(async () => (await commands(page, ['catalog_preview'])).length).toBe(3);
  await page.evaluate(() => (window as any).i18nFixture.releasePreview());
  await expect(page.locator('#catalog-messages')).toContainText('사용자가 작성한 대화 원문');
  const before = await commands(page, ['catalog_list', 'catalog_preview', 'list_projects']);
  await switchLanguage(page, 'en');
  await expect(page.locator('#catalog-search')).toHaveAttribute('placeholder', /session title/i);
  await expect(page.locator('#catalog-close')).toHaveAttribute('aria-label', /close/i);
  await expect(page.locator('#catalog-project option').first()).toHaveText(/all projects/i);
  await expect(page.locator('#catalog-list [aria-selected=true]')).toHaveAttribute('data-thread-id', catalogRows[1].id);
  await expect(page.locator('#catalog-session-title')).toHaveText(catalogRows[1].title);
  await expect(page.locator('#catalog-messages article strong')).toHaveText(/user/i);
  await expect(page.locator('#catalog-messages article p')).toHaveText('사용자가 작성한 대화 원문');
  await expect(page.locator('#catalog-preview-status')).toContainText(/read.only/i);
  expect((await page.locator('#catalog-metadata dt').allTextContents()).some(text => /[가-힣]/.test(text))).toBe(false);
  await expect(page.locator('#catalog-project')).toHaveValue('');
  await expect(page.locator('#catalog-search')).toHaveValue('사용자 검색 초안');
  expect(await commands(page, ['catalog_list', 'catalog_preview', 'list_projects'])).toEqual(before);
  expect(await commands(page, processActions)).toEqual([]);
});
