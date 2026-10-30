import { test, expect, type Page } from '@playwright/test';

const runtime = 'D:\\도구\\OpenDIY Codex\\codex.exe';
const projects = [
  { id: 'extension-alpha', name: 'Alpha 프로젝트', cwd: 'D:\\프로젝트\\Alpha', createdAt: 1700000000 },
  { id: 'extension-beta', name: 'Beta 프로젝트', cwd: 'D:\\프로젝트\\Beta', createdAt: 1700000001 },
];
const live = { id: 'extensions-live', generation: 7, title: '진행 중인 작업', cwd: projects[0].cwd, threadId: '01900000-0000-7000-8000-000000000007', state: 'idle', active: true };
const sourceFolder = 'D:\\내 스킬\\review-workflow';

async function fixture(page: Page) {
  await page.addInitScript(({ runtime, projects, live, sourceFolder }) => {
    const state = window as any;
    state.isTauri = true;
    let callbackId = 0;
    const callbacks = new Map<number, (value: any) => void>();
    const listeners = new Map<string, (value: any) => void>();
    const installed = [{ pluginId: 'review-kit@local-market', name: 'review-kit', marketplaceName: 'local-market', version: '1.2.3', installed: true, enabled: true, source: 'local', installPolicy: 'AVAILABLE', authPolicy: 'ON_INSTALL' }];
    const available = [{ pluginId: 'writing-kit@local-market', name: 'writing-kit', marketplaceName: 'local-market', version: '2.0.0', installed: false, enabled: false, source: 'local', installPolicy: 'AVAILABLE', authPolicy: 'ON_INSTALL' }];
    const marketplaces = [{ name: 'local-market', source: 'D:\\fixture marketplace', path: 'D:\\fixture marketplace' }];
    const f = state.extensionsFixture = {
      calls: [] as any[], listFailure: '', mutationFailure: '', sectionFailure: false,
      holdLists: false, holdMutation: false, pendingLists: [] as any[], pendingMutations: [] as any[],
      holdCompleteExit: false, completeExitFailure: '', pendingExits: [] as any[], exitObservations: [] as any[], settledMutations: 0,
      installed, available, marketplaces, importedSkill: null as any,
      emit(event: string, payload: any) { listeners.get(event)!({ event, id: 1, payload }); },
      releaseList(index = 0) { const pending = this.pendingLists.splice(index, 1)[0]; if (!pending) throw new Error('No pending extension list'); pending.resolve(pending.snapshot); },
      releaseMutation() { const pending = this.pendingMutations.shift(); if (!pending) throw new Error('No pending extension mutation'); pending(); },
      releaseCompleteExit() { const pending = this.pendingExits.shift(); if (!pending) throw new Error('No pending native exit'); pending(); },
      snapshot(cwd?: string | null) {
        const project = projects.find(project => project.cwd === cwd);
        return {
          executable: runtime, cwd: cwd ?? null,
          skillDestinations: { user: 'D:\\fixture-home\\.agents\\skills', project: cwd ? cwd + '\\.agents\\skills' : null },
          plugins: this.sectionFailure ? null : { installed: structuredClone(this.installed), available: structuredClone(this.available) },
          marketplaces: structuredClone(this.marketplaces),
          skills: [
            { name: 'user-review', description: '사용자가 만든 검토 스킬', path: 'D:\\fixture-home\\.agents\\skills\\user-review\\SKILL.md', scope: 'user', enabled: true, pluginId: null },
            ...(project ? [{ name: project.id + '-skill', description: project.name + ' 전용 스킬', path: cwd + '\\.agents\\skills\\project-review\\SKILL.md', scope: 'repo', enabled: true, pluginId: null }] : []),
            ...(this.importedSkill ? [structuredClone(this.importedSkill)] : []),
          ],
          errors: this.sectionFailure ? [{ section: 'plugins', message: 'Selected CLI does not support plugin list: exit 2' }] : [],
        };
      },
    };
    state.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
    state.__TAURI_INTERNALS__ = {
      transformCallback(handler: (value: any) => void) { callbacks.set(++callbackId, handler); return callbackId; }, unregisterCallback() {},
      async invoke(command: string, args: any = {}) {
        f.calls.push({ command, args: structuredClone(args) });
        if (command === 'plugin:event|listen') { listeners.set(args.event, callbacks.get(args.handler)!); return callbackId; }
        if (command.startsWith('plugin:event|')) return callbackId;
        if (command === 'get_ui_language') return 'ko';
        if (command === 'set_ui_language') return { show: args.language === 'en' ? 'Open OpenDIY Desktop' : 'OpenDIY Desktop 열기', exit: args.language === 'en' ? 'Exit…' : '종료…' };
        if (command === 'load_layout') return { version: 1, columns: 2, sessions: [] };
        if (command === 'list_sessions') return [live];
        if (command === 'list_projects') return projects;
        if (command === 'choose_skill_folder') return sourceFolder;
        if (command === 'get_codex_settings') return { savedPath: runtime, resolvedPath: runtime, source: 'saved', error: null };
        if (command === 'complete_exit') {
          f.exitObservations.push({ settledMutations: f.settledMutations, args: structuredClone(args) });
          if (f.holdCompleteExit) await new Promise<void>(resolve => { f.pendingExits.push(resolve); });
          if (f.completeExitFailure) throw new Error(f.completeExitFailure);
          return null;
        }
        if (command === 'extensions_list') {
          if (f.listFailure) throw new Error(f.listFailure);
          const snapshot = f.snapshot(args.cwd);
          if (f.holdLists) return await new Promise(resolve => { f.pendingLists.push({ resolve, snapshot }); });
          return snapshot;
        }
        if (command === 'extensions_mutate') {
          try {
          if (f.holdMutation) await new Promise<void>(resolve => { f.pendingMutations.push(resolve); });
          if (f.mutationFailure) throw new Error(f.mutationFailure);
          const request = args.request;
          if (request.operation === 'installPlugin') {
            const index = f.available.findIndex((plugin: any) => plugin.pluginId === request.selector);
            if (index < 0) throw new Error('Unknown plugin selector');
            const plugin = f.available.splice(index, 1)[0]; f.installed.push({ ...plugin, installed: true, enabled: true });
          } else if (request.operation === 'setPluginEnabled') {
            const plugin = f.installed.find((plugin: any) => plugin.pluginId === request.pluginId);
            if (!plugin) throw new Error('Unknown installed plugin'); plugin.enabled = request.enabled;
          } else if (request.operation === 'removePlugin') {
            const index = f.installed.findIndex((plugin: any) => plugin.pluginId === request.pluginId);
            if (index < 0) throw new Error('Unknown installed plugin');
            const plugin = f.installed.splice(index, 1)[0]; f.available.push({ ...plugin, installed: false, enabled: false });
          } else if (request.operation === 'setSkillEnabled') {
            // Store the requested state separately; snapshot construction remains a fixture,
            // not evidence of Codex configuration persistence.
            (f as any).skillOverride = { path: request.path, enabled: request.enabled };
          } else if (request.operation === 'importSkill') {
            if (request.scope !== 'project' || args.cwd !== projects[0].cwd || request.sourcePath !== sourceFolder) throw new Error('Unexpected skill import scope/source');
            f.importedSkill = { name: 'review-workflow', description: 'Imported fixture skill', path: projects[0].cwd + '\\.agents\\skills\\review-workflow\\SKILL.md', scope: 'repo', enabled: true, pluginId: null };
          } else if (request.operation === 'addMarketplace') {
            f.marketplaces.push({ name: 'added-market', source: request.source, path: request.source });
          } else if (!['removeMarketplace', 'upgradeMarketplace'].includes(request.operation)) throw new Error('Unknown extension operation');
          const snapshot = f.snapshot(args.cwd);
          if ((f as any).skillOverride) snapshot.skills = snapshot.skills.map((skill: any) => skill.path === (f as any).skillOverride.path ? { ...skill, enabled: (f as any).skillOverride.enabled } : skill);
          return snapshot;
          } finally { f.settledMutations++; }
        }
        return null;
      },
    };
  }, { runtime, projects, live, sourceFolder });
  await page.goto('/');
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await expect(page.locator('.terminal-card')).toHaveCount(1);
}

const extensionCalls = (page: Page, command = 'extensions_mutate') => page.evaluate(command => (window as any).extensionsFixture.calls.filter((call: any) => call.command === command), command);
const processCalls = (page: Page) => page.evaluate(() => (window as any).extensionsFixture.calls.filter((call: any) => ['start_session', 'session_stop', 'session_write', 'complete_exit', 'set_codex_executable'].includes(call.command)));
const projectTab = (page: Page, name: string) => page.locator('#project-tabs [role=tab]').filter({ hasText: name });

const rows = (page: Page) => page.locator('#extensions-list [role=option]');
async function open(page: Page, projectIndex?: number, wait = true) {
  if (projectIndex !== undefined) {
    if (await page.locator('#settings-dialog').isVisible()) await page.locator('#settings-close').click();
    await projectTab(page, projects[projectIndex].name).click();
  }
  if (!await page.locator('#settings-dialog').isVisible()) await page.locator('#settings-open').click();
  await page.locator('#extensions-open').click();
  await expect(page.locator('#extensions-dialog')).toBeVisible();
  if (wait) await expect(page.locator('#extensions-dialog')).toHaveAttribute('aria-busy', 'false');
}
async function language(page: Page, value: 'ko' | 'en') {
  const reopen = await page.locator('#extensions-dialog').isVisible();
  if (reopen) await page.locator('#extensions-close').click();
  await page.locator('#language-select').selectOption(value);
  await expect(page.locator('html')).toHaveAttribute('lang', value);
  if (reopen) await open(page, undefined, false);
}
async function confirm(page: Page) {
  await expect(page.locator('#extensions-review')).toBeVisible();
  await page.locator('#extensions-review-confirm').click();
  await expect(page.locator('#extensions-dialog')).toHaveAttribute('aria-busy', 'false');
}

test('discovery searches selected-runtime metadata without mutating or replacing the live terminal', async ({ page }, testInfo) => {
  await fixture(page);
  await page.evaluate(({ live }) => {
    const f = (window as any).extensionsFixture;
    f.terminal = document.querySelector('.terminal-card'); f.screen = f.terminal.querySelector('.xterm-screen');
    f.emit('terminal-output', { id: live.id, generation: live.generation, seq: 1, data: 'EXTENSIONS_LIVE_OUTPUT 한글\r\n' });
  }, { live });
  await expect(page.locator('.xterm-rows')).toContainText('EXTENSIONS_LIVE_OUTPUT');
  const before = await processCalls(page);
  await open(page);
  await expect(page.locator('#extensions-executable')).toHaveText(runtime);
  await expect(rows(page)).toHaveCount(2);
  await page.locator('#extensions-search').fill('WRITING-KIT');
  await expect(rows(page)).toHaveCount(1);
  await expect(page.locator('#extensions-detail')).toContainText('writing-kit@local-market');
  await expect(page.locator('#extensions-detail')).toContainText('2.0.0');
  await expect(page.locator('#extensions-detail')).toContainText('ON_INSTALL');
  expect(await extensionCalls(page)).toEqual([]);
  await page.screenshot({ path: testInfo.outputPath('extensions-discovery.png'), fullPage: true });
  await page.locator('#extensions-close').click();
  expect(await processCalls(page)).toEqual(before);
  expect(await page.evaluate(() => {
    const f = (window as any).extensionsFixture;
    return f.terminal.isConnected && f.screen.isConnected && f.terminal.querySelector('.xterm-screen') === f.screen;
  })).toBe(true);
  await expect(page.locator('.terminal-identity')).toHaveAttribute('title', live.threadId);
  await expect(page.locator('.xterm-rows')).toContainText('EXTENSIONS_LIVE_OUTPUT');
});

test('plugin installation reviews the exact catalog selector then displays the read-back installed state', async ({ page }) => {
  await fixture(page); await open(page, 0);
  await rows(page).filter({ hasText: 'writing-kit' }).click();
  await page.locator('#extensions-plugin-install').click();
  await expect(page.locator('#extensions-review-target')).toHaveText('writing-kit@local-market');
  await expect(page.locator('#extensions-review-scope')).toContainText('모든 프로젝트');
  expect(await extensionCalls(page)).toEqual([]);
  await confirm(page);
  expect(await extensionCalls(page)).toEqual([{ command: 'extensions_mutate', args: { cwd: projects[0].cwd, request: { operation: 'installPlugin', selector: 'writing-kit@local-market' } } }]);
  await expect(page.locator('#extensions-detail')).toContainText('설치됨 · 사용');
  await expect(page.locator('#extensions-plugin-install')).toHaveCount(0);
  await expect(page.locator('#extensions-plugin-remove')).toBeVisible();
});

test('installed plugin toggle and removal address one ID and removal can be canceled before mutation', async ({ page }) => {
  await fixture(page); await open(page);
  await rows(page).filter({ hasText: 'review-kit' }).click();
  await page.locator('#extensions-plugin-toggle').click(); await confirm(page);
  await expect(page.locator('#extensions-detail')).toContainText('설치됨 · 사용 안 함');
  await page.locator('#extensions-plugin-remove').click();
  await expect(page.locator('#extensions-review-target')).toHaveText('review-kit@local-market');
  expect((await extensionCalls(page)).map((call: any) => call.args.request.operation)).toEqual(['setPluginEnabled']);
  await page.locator('#extensions-review-cancel').click();
  await expect(page.locator('#extensions-review')).toBeHidden();
  await expect(page.locator('#extensions-plugin-remove')).toBeVisible();
  await page.locator('#extensions-plugin-remove').click(); await confirm(page);
  expect(await extensionCalls(page)).toEqual([
    { command: 'extensions_mutate', args: { cwd: null, request: { operation: 'setPluginEnabled', pluginId: 'review-kit@local-market', enabled: false } } },
    { command: 'extensions_mutate', args: { cwd: null, request: { operation: 'removePlugin', pluginId: 'review-kit@local-market' } } },
  ]);
  await expect(page.locator('#extensions-detail')).toContainText('설치할 수 있음');
});

test('unsupported plugin list remains an error while skills work and refresh can recover', async ({ page }) => {
  await fixture(page);
  await page.evaluate(() => { (window as any).extensionsFixture.sectionFailure = true; });
  await open(page, 0);
  await expect(page.locator('#extensions-error')).toHaveText('Selected CLI does not support plugin list: exit 2');
  await expect(page.locator('#extensions-list')).toContainText('목록을 불러오지 못했습니다');
  await expect(page.locator('#extensions-list')).not.toContainText('등록된 항목이 없습니다');
  await page.locator('#extensions-tab-skills').click();
  await expect(rows(page).filter({ hasText: 'user-review' })).toBeVisible();
  await expect(page.locator('#extensions-error')).toBeHidden();
  await page.evaluate(() => { (window as any).extensionsFixture.sectionFailure = false; });
  await page.locator('#extensions-refresh').click();
  await expect(page.locator('#extensions-dialog')).toHaveAttribute('aria-busy', 'false');
  await page.locator('#extensions-tab-plugins').click();
  await expect(rows(page)).toHaveCount(2);
  await expect(page.locator('#extensions-error')).toBeHidden();
  expect(await extensionCalls(page)).toEqual([]);
});

test('설정에서 언어를 바꾸고 확장 창을 다시 열어도 초안과 보류된 변경 한 건이 유지된다', async ({ page }) => {
  await fixture(page); await open(page);
  await page.locator('#extensions-tab-marketplaces').click();
  await page.locator('#extensions-marketplace-source').fill('https://example.test/사용자/market.git');
  await page.locator('#extensions-marketplace-ref').fill('draft-branch');
  await page.locator('#extensions-tab-plugins').click();
  await page.locator('#extensions-search').fill('review-kit');
  await rows(page).first().click();
  await page.evaluate(() => {
    const f = (window as any).extensionsFixture; f.holdMutation = true;
    f.terminal = document.querySelector('.terminal-card'); f.screen = f.terminal.querySelector('.xterm-screen');
  });
  await page.locator('#extensions-plugin-toggle').click();
  await page.locator('#extensions-review-confirm').click();
  await expect(page.locator('#extensions-dialog')).toHaveAttribute('aria-busy', 'true');
  await language(page, 'en');
  await expect(page.locator('#extensions-title')).toHaveText('Plugins & skills');
  await expect(page.locator('#extensions-search')).toHaveValue('review-kit');
  await expect(page.locator('#extensions-marketplace-source')).toHaveValue('https://example.test/사용자/market.git');
  await expect(page.locator('#extensions-marketplace-ref')).toHaveValue('draft-branch');
  await expect(page.locator('#extensions-refresh')).toBeDisabled();
  expect(await extensionCalls(page)).toHaveLength(1);
  await page.evaluate(() => (window as any).extensionsFixture.releaseMutation());
  await expect(page.locator('#extensions-dialog')).toHaveAttribute('aria-busy', 'false');
  await expect(rows(page).filter({ hasText: 'review-kit' })).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('#extensions-detail')).toContainText('Installed · disabled');
  await expect(page.locator('#extensions-status')).toContainText('Loaded the actual CLI configuration');
  expect(await processCalls(page)).toEqual([]);
  expect(await page.evaluate(() => {
    const f = (window as any).extensionsFixture; return f.terminal.isConnected && f.terminal.querySelector('.xterm-screen') === f.screen;
  })).toBe(true);
});

test('project scope skill import captures the selected project and reviews source and destination', async ({ page }) => {
  await fixture(page);
  // Exercise no-project first so stale translated text cannot overwrite later user paths.
  await open(page); await page.locator('#extensions-close').click();
  await open(page, 0); await language(page, 'en');
  await expect(page.locator('#extensions-project')).toHaveText(projects[0].cwd);
  await page.locator('#extensions-tab-skills').click();
  await page.locator('#extensions-skill-choose').click();
  await expect(page.locator('#extensions-skill-source')).toHaveValue(sourceFolder);
  await page.locator('#extensions-skill-name').fill('review-workflow');
  await page.locator('#extensions-skill-scope').selectOption('project');
  const destination = projects[0].cwd + '\\.agents\\skills\\review-workflow';
  await expect(page.locator('#extensions-skill-destination')).toContainText(destination);
  await page.locator('#extensions-skill-import').click();
  await expect(page.locator('#extensions-review-target')).toHaveText(sourceFolder + '\n→ ' + destination);
  await expect(page.locator('#extensions-review-scope')).toContainText('current project');
  expect(await extensionCalls(page)).toEqual([]);
  await confirm(page);
  expect(await extensionCalls(page)).toEqual([{ command: 'extensions_mutate', args: { cwd: projects[0].cwd, request: { operation: 'importSkill', sourcePath: sourceFolder, name: 'review-workflow', scope: 'project' } } }]);
  await expect(rows(page).filter({ hasText: 'review-workflow' })).toBeVisible();
  expect(await processCalls(page)).toEqual([]);
});

test('marketplace source and revision are explicit data reviewed before registration', async ({ page }) => {
  await fixture(page); await open(page);
  await page.locator('#extensions-tab-marketplaces').click();
  const source = 'D:\\한글 경로\\marketplace';
  await page.locator('#extensions-marketplace-source').fill(source);
  await page.locator('#extensions-marketplace-ref').fill('release/test-branch');
  await language(page, 'en');
  await expect(page.locator('#extensions-marketplace-source')).toHaveValue(source);
  await page.locator('#extensions-marketplace-add').click();
  await expect(page.locator('#extensions-review-target')).toHaveText(source + '\nref: release/test-branch');
  expect(await extensionCalls(page)).toEqual([]);
  await confirm(page);
  expect(await extensionCalls(page)).toEqual([{ command: 'extensions_mutate', args: { cwd: null, request: { operation: 'addMarketplace', source, gitRef: 'release/test-branch' } } }]);
  await expect(rows(page).filter({ hasText: 'added-market' })).toBeVisible();
});

test('a late list from a closed project cannot replace the newly opened project context', async ({ page }) => {
  await fixture(page);
  await page.evaluate(() => { (window as any).extensionsFixture.holdLists = true; });
  await open(page, 0, false);
  await expect.poll(() => page.evaluate(() => (window as any).extensionsFixture.pendingLists.length)).toBe(1);
  await page.locator('#extensions-close').click();
  await open(page, 1, false);
  await page.locator('#extensions-close').click();
  await open(page, 0, false);
  await page.locator('#extensions-close').click();
  await open(page, 1, false);
  // Closing/reopening coalesces the newest project intent behind the sole active read.
  expect((await extensionCalls(page, 'extensions_list')).map((call: any) => call.args.cwd)).toEqual([projects[0].cwd]);
  await page.evaluate(() => (window as any).extensionsFixture.releaseList(0));
  await expect.poll(() => extensionCalls(page, 'extensions_list')).toHaveLength(2);
  await expect.poll(() => page.evaluate(() => (window as any).extensionsFixture.pendingLists.length)).toBe(1);
  expect((await extensionCalls(page, 'extensions_list')).map((call: any) => call.args.cwd)).toEqual([projects[0].cwd, projects[1].cwd]);
  await page.evaluate(() => (window as any).extensionsFixture.releaseList(0));
  await expect(page.locator('#extensions-dialog')).toHaveAttribute('aria-busy', 'false');
  await expect(page.locator('#extensions-project')).toHaveText(projects[1].cwd);
  await page.locator('#extensions-tab-skills').click();
  await expect(rows(page).filter({ hasText: 'extension-beta-skill' })).toBeVisible();
  await expect(rows(page).filter({ hasText: 'extension-alpha-skill' })).toHaveCount(0);
  expect(await extensionCalls(page)).toEqual([]);
});

test('failed mutation reports its diagnostic and requires readback before a deliberate retry', async ({ page }) => {
  await fixture(page); await open(page);
  await page.evaluate(() => { (window as any).extensionsFixture.mutationFailure = 'Fixture CLI write rejected: policy denied'; });
  await page.locator('#extensions-plugin-toggle').click(); await confirm(page);
  await expect(page.locator('#extensions-error')).toHaveText('Fixture CLI write rejected: policy denied');
  await expect(page.locator('#extensions-plugin-toggle')).toHaveCount(0);
  await expect(rows(page)).toHaveCount(0);
  await expect(page.locator('#extensions-dialog')).toHaveAttribute('aria-busy', 'false');
  await page.evaluate(() => { (window as any).extensionsFixture.mutationFailure = ''; });
  await page.locator('#extensions-refresh').click();
  await expect(page.locator('#extensions-dialog')).toHaveAttribute('aria-busy', 'false');
  await page.locator('#extensions-plugin-toggle').click(); await confirm(page);
  await expect(page.locator('#extensions-detail')).toContainText('설치됨 · 사용 안 함');
  expect(await extensionCalls(page)).toHaveLength(2);
});


test('skill activation reviews the exact SKILL.md path and sends the enabled setting in project context', async ({ page }) => {
  await fixture(page); await open(page, 0);
  await page.locator('#extensions-tab-skills').click();
  await rows(page).filter({ hasText: 'extension-alpha-skill' }).click();
  const path = projects[0].cwd + '\\.agents\\skills\\project-review\\SKILL.md';
  await page.locator('#extensions-skill-toggle').click();
  await expect(page.locator('#extensions-review-target')).toHaveText(path);
  expect(await extensionCalls(page)).toEqual([]);
  await confirm(page);
  expect(await extensionCalls(page)).toEqual([{ command: 'extensions_mutate', args: { cwd: projects[0].cwd, request: { operation: 'setSkillEnabled', path, enabled: false } } }]);
  await expect(page.locator('#extensions-detail')).toContainText('사용 안 함');
});

test('editing an import destination invalidates the reviewed action until the new target is reviewed', async ({ page }) => {
  await fixture(page); await open(page, 0);
  await page.locator('#extensions-tab-skills').click();
  await page.locator('#extensions-skill-choose').click();
  await page.locator('#extensions-skill-scope').selectOption('project');
  await page.locator('#extensions-skill-import').click();
  await expect(page.locator('#extensions-review')).toBeVisible();
  await page.locator('#extensions-skill-name').fill('../escape');
  await expect(page.locator('#extensions-review')).toBeHidden();
  await page.locator('#extensions-skill-import').click();
  expect(await page.locator('#extensions-skill-name').evaluate((input: HTMLInputElement) => input.validity.patternMismatch)).toBe(true);
  await expect(page.locator('#extensions-review')).toBeHidden();
  expect(await extensionCalls(page)).toEqual([]);
  await page.locator('#extensions-skill-name').fill('foo_bar');
  await page.locator('#extensions-skill-import').click();
  await expect(page.locator('#extensions-review')).toBeVisible();
  await expect(page.locator('#extensions-review-target')).toContainText('foo_bar');
  await expect(page.locator('#extensions-review-target')).not.toContainText('skills\\review-workflow');
  expect(await extensionCalls(page)).toEqual([]);
});


test('large catalogs bound rendered rows and close releases row and detail DOM while keeping a small query draft', async ({ page }) => {
  await fixture(page);
  await page.evaluate(() => {
    const f = (window as any).extensionsFixture;
    for (let index = 0; index < 1000; index++) f.available.push({ pluginId: `bulk-${index}@local-market`, name: `bulk-${index}`, marketplaceName: 'local-market', version: '1.0', enabled: false, source: 'fixture', installPolicy: 'AVAILABLE', authPolicy: 'ON_INSTALL' });
  });
  await open(page);
  await expect(rows(page)).toHaveCount(50);
  await page.locator('#extensions-more').click();
  await expect(rows(page)).toHaveCount(100);
  for (const count of [150, 200, 250]) {
    await page.locator('#extensions-more').click();
    await expect(rows(page)).toHaveCount(count);
  }
  await page.locator('#extensions-more').click();
  await expect(rows(page)).toHaveCount(50);
  await expect(page.locator('#extensions-range')).toContainText('251–300');
  await expect(rows(page).first()).not.toContainText('review-kit');
  await page.locator('#extensions-previous').click();
  await expect(rows(page)).toHaveCount(250);
  await expect(rows(page).first()).toContainText('review-kit');
  await page.locator('#extensions-search').fill('bulk-999');
  await expect(rows(page)).toHaveCount(1);
  await expect(rows(page).first()).toContainText('bulk-999');
  for (let cycle = 0; cycle < 3; cycle++) {
    await page.locator('#extensions-close').click();
    await expect(rows(page)).toHaveCount(0);
    await expect(page.locator('#extensions-detail > *')).toHaveCount(0);
    await open(page);
    await expect(page.locator('#extensions-search')).toHaveValue('bulk-999');
    await expect(rows(page)).toHaveCount(1);
  }
  await page.locator('#extensions-close').click();
  await expect(rows(page)).toHaveCount(0);
  expect(await extensionCalls(page)).toEqual([]);
});


for (const nativeFailure of [false, true]) test(`confirmed app exit drains accepted extension mutation and ${nativeFailure ? 'unlocks management after native exit failure' : 'blocks new management until native exit completes'}`, async ({ page }) => {
  await fixture(page); await open(page, 0);
  await page.clock.install();
  await page.evaluate((nativeFailure) => {
    const f = (window as any).extensionsFixture;
    f.holdMutation = true; f.holdCompleteExit = true;
    f.completeExitFailure = nativeFailure ? 'Fixture native exit failed before shutdown' : '';
  }, nativeFailure);
  await page.locator('#extensions-plugin-toggle').click();
  await page.locator('#extensions-review-confirm').click();
  await expect.poll(() => page.evaluate(() => (window as any).extensionsFixture.pendingMutations.length)).toBe(1);
  await page.locator('#extensions-close').click();
  await page.locator('#settings-close').click();
  await page.locator('#exit').click();
  await page.locator('#confirm-exit').click();
  // Advance the app's 450ms save/exit debounce without wall-clock sleeps.
  // A missing mutation drain would now enter complete_exit while the write is held.
  await page.clock.runFor(500);
  expect(await extensionCalls(page, 'complete_exit')).toEqual([]);
  await expect(page.locator('#confirm-exit')).toBeDisabled();
  await expect(page.locator('#exit-dialog [data-close]').first()).toBeDisabled();
  await expect(page.locator('#exit-dialog [data-close]').last()).toBeDisabled();
  await page.keyboard.press('Escape');
  await expect(page.locator('#exit-dialog')).toBeVisible();
  await expect(page.locator('#extensions-open')).toBeDisabled();
  const before = await extensionCalls(page, 'extensions_list');
  await expect(page.locator('#extensions-dialog')).toBeHidden();
  expect(await extensionCalls(page, 'extensions_list')).toEqual(before);
  await page.evaluate(() => (window as any).extensionsFixture.releaseMutation());
  await expect.poll(async () => {
    await page.clock.runFor(500);
    return (await extensionCalls(page, 'complete_exit')).length;
  }).toBe(1);
  expect(await page.evaluate(() => (window as any).extensionsFixture.exitObservations)).toEqual([{ settledMutations: 1, args: { confirmed: true } }]);
  await expect(page.locator('#extensions-open')).toBeDisabled();
  await page.evaluate(() => (window as any).extensionsFixture.releaseCompleteExit());
  if (nativeFailure) {
    await expect(page.locator('#confirm-exit')).toBeEnabled();
    await expect(page.locator('#extensions-open')).toBeEnabled();
    await expect(page.locator('#exit-dialog [data-close]').first()).toBeEnabled();
    await expect(page.locator('#exit-dialog [data-close]').last()).toBeEnabled();
    await page.keyboard.press('Escape');
    await expect(page.locator('#exit-dialog')).toBeHidden();
    await open(page);
    await expect(page.locator('#extensions-detail')).toContainText('설치됨 · 사용 안 함');
    expect(await extensionCalls(page, 'extensions_list')).toHaveLength(before.length + 1);
  } else {
    await expect(page.locator('#extensions-open')).toBeDisabled();
    await expect(page.locator('#extensions-dialog')).toBeHidden();
    expect(await extensionCalls(page, 'extensions_list')).toEqual(before);
  }
  expect(await extensionCalls(page)).toHaveLength(1);
});
