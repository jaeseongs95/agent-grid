// Opt-in test of a root-launched packaged OpenDIY Desktop WebView2. Never starts a browser,
// changes global config, approves a terminal prompt, or sends Enter to a terminal.
// Opt-in executable settings tests change only the fresh isolated fixture database.
import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { mkdir, readdir, realpath, stat, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { chromium } from '@playwright/test';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';

const execFileAsync = promisify(execFile);
const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

const report = {
  schema: 'agent-grid-native-smoke.v1',
  startedAt: new Date().toISOString(),
  runtime: 'packaged WebView2 through opt-in CDP',
  checks: [], sessions: [], errors: [], cleanup: [],
  limitations: ['Synthetic composition events do not verify a physical Windows IME; Korean PTY echo is checked separately in rendered terminal rows.', 'No model turn, trust approval, or terminal Enter is sent.', 'Screenshots mask all terminal text and observed external records.'],
};
const shortError = error => ({ name: error?.name ?? 'Error', message: String(error?.message ?? error).split('\n')[0].slice(0, 400), ...(error?.windowProbe ? { windowProbe: error.windowProbe } : {}) });
const probeDiagnostic = value => String(value ?? '').replace(/\r\n/g, '\n').split('\n').slice(0, 40).join('\n').slice(0, 4096);
const hash = value => createHash('sha256').update(value).digest('hex');
let page;
let browser;
let outDir;
let safeInitialState = false;
let expectedStopTitle = '';
let exited = false;
const ownedTitles = new Set();
const sessionTimeout = 90_000;

async function check(name, work) {
  const start = Date.now();
  try { const detail = await work(); report.checks.push({ name, result: 'PASS', durationMs: Date.now() - start, detail }); return detail; }
  catch (error) { report.checks.push({ name, result: 'FAIL', durationMs: Date.now() - start, error: shortError(error) }); throw error; }
}

async function until(read, accepts, label, timeout = 15_000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) { const value = await read(); if (accepts(value)) return value; await new Promise(resolve => setTimeout(resolve, 100)); }
  throw new Error(`Timed out: ${label}`);
}

function tile(title) { return page.locator('.terminal-card').filter({ has: page.locator('.terminal-title', { hasText: title }) }); }
async function tileTitles() { return page.locator('.terminal-card .terminal-title').allTextContents(); }
async function assertOnlyOurs() {
  const titles = await tileTitles();
  assert(titles.every(title => ownedTitles.has(title)), 'Unexpected owned tile appeared; refusing complete exit to preserve other work');
  return titles.length;
}

async function maskedScreenshot(filename) {
  if (!page || page.isClosed() || !safeInitialState) return;
  await page.screenshot({ path: path.join(outDir, filename), fullPage: true, mask: [page.locator('.terminal-host'), page.locator('.terminal-path'), page.locator('#external-list'), page.locator('#notification-list'), page.locator('#restore-list'), page.locator('#catalog-list'), page.locator('#catalog-selected'), page.locator('#catalog-project'), page.locator('#projects'), page.locator('#project-tabs'), page.locator('#codex-executable-path'), page.locator('#codex-executable-resolved')], maskColor: '#141820' });
}

async function nativeCall(command, args = {}) {
  return page.evaluate(({ command, args }) => window.__TAURI_INTERNALS__.invoke(command, args), { command, args });
}

async function openSettings() {
  if (!await page.locator('#settings-dialog').isVisible()) await page.locator('#settings-open').click();
}

async function closePopups() {
  // 설정과 그 위의 하위 창을 모두 실제 닫기 키로 해제한다.
  for (let depth = 0; depth < 4 && await page.locator('dialog[open]').count(); depth++) await page.keyboard.press('Escape');
  assert.equal(await page.locator('dialog[open]').count(), 0, '열린 창을 닫지 못해 본 화면 조작을 중단합니다');
}

async function projectPicker(mode, cwd) {
  const processId = process.env.AGENT_GRID_NATIVE_PID;
  const executable = process.env.AGENT_GRID_NATIVE_EXE;
  assert(processId && executable, 'Native picker tests require the root-launched PID and exact executable');
  const script = fileURLToPath(new URL('./project-picker-probe.ps1', import.meta.url));
  const powershell = path.join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe');
  const args = ['-NoProfile', '-NonInteractive', '-File', script, '-AppProcessId', processId, '-ExpectedExecutable', executable, '-ExpectedDialogText', '프로젝트로 가져올 폴더를 선택하세요', '-Mode', mode];
  if (cwd) args.push('-ExpectedPath', cwd);
  await page.locator('#project-import').click();
  try {
    const { stdout } = await execFileAsync(powershell, args, { windowsHide: true, timeout: 25_000, maxBuffer: 64 * 1024 });
    const probe = JSON.parse(stdout.trim());
    assert.equal(probe.schema, 'agent-grid-project-picker-probe-v2');
    assert(probe.exactProcessAndDialogVerified && probe.modernFolderDialogVerified && probe.directUIHWNDVerified && probe.dialogClosed && !probe.externalProcessesTouched, 'Project picker must verify an owned modern folder dialog');
    return probe;
  } catch (error) {
    error.windowProbe = { script: 'tests/project-picker-probe.ps1', code: error.code ?? null, stdout: probeDiagnostic(error.stdout), stderr: probeDiagnostic(error.stderr) };
    throw error;
  }
}

async function executablePicker(mode, executablePath) {
  const processId = process.env.AGENT_GRID_NATIVE_PID;
  const executable = process.env.AGENT_GRID_NATIVE_EXE;
  assert(processId && executable, 'Executable picker requires the root-launched PID and exact executable');
  const script = fileURLToPath(new URL('./executable-picker-probe.ps1', import.meta.url));
  const powershell = path.join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe');
  const args = ['-NoProfile', '-NonInteractive', '-File', script, '-AppProcessId', processId, '-ExpectedExecutable', executable, '-ExpectedDialogTitle', 'CLI 실행 파일 선택', '-Mode', mode];
  if (executablePath) args.push('-ExpectedPath', executablePath);
  await page.locator('#codex-executable-choose').click();
  try {
    const { stdout } = await execFileAsync(powershell, args, { windowsHide: true, timeout: 25_000, maxBuffer: 64 * 1024 });
    const probe = JSON.parse(stdout.trim());
    assert(probe.exactProcessAndDialogVerified && probe.dialogClosed && !probe.externalProcessesTouched);
    return probe;
  } catch (error) {
    error.windowProbe = { script: 'tests/executable-picker-probe.ps1', code: error.code ?? null, stdout: probeDiagnostic(error.stdout), stderr: probeDiagnostic(error.stderr) };
    throw error;
  }
}

async function createSession(title, cwd) {
  ownedTitles.add(title); // Include a tile created before a start command fails.
  await page.locator('#new-session').click();
  await page.locator('#session-title').fill(title);
  await page.locator('#session-cwd').fill(cwd);
  await page.locator('#create-submit').click();
  await until(async () => ({ open: await page.locator('#create-dialog').getAttribute('open'), error: await page.locator('#create-error').isVisible() ? await page.locator('#create-error').textContent() : '' }), value => {
    if (value.error) throw new Error(`Native session creation failed: ${value.error}`);
    return value.open === null;
  }, 'native create dialog completes', sessionTimeout);
  const card = tile(title);
  await card.waitFor({ state: 'visible', timeout: sessionTimeout });
  const info = await until(async () => {
    const identity = card.locator('.terminal-identity');
    const fullThreadId = (await identity.getAttribute('title')) ?? '';
    const visible = (await identity.textContent()) ?? '';
    const state = ((await card.locator('.session-state').getAttribute('class')) ?? '').split(/\s+/).find(value => ['idle', 'running', 'waiting'].includes(value));
    const text = await card.locator('.terminal-host').innerText();
    return { id: await card.getAttribute('data-id'), state, threadId: fullThreadId || /스레드\s+(\S+)/.exec(visible)?.[1], fullThreadId: !!fullThreadId, outputLength: text.trim().length, outputSha256: hash(text) };
  }, info => !!info.id && info.fullThreadId && uuid.test(info.threadId) && !!info.state && info.outputLength > 10, 'real CLI thread metadata, observed state and terminal text', sessionTimeout);
  assert(info.fullThreadId && uuid.test(info.threadId), 'Expected a full UUID in the UI thread identifier tooltip');
  report.sessions.push(info);
  return info;
}

async function stopOurs() {
  await closePopups();
  const allProjects = page.locator('#project-tabs [role="tab"][data-project-cwd=""]');
  if (await allProjects.count()) await allProjects.click();
  for (const title of ownedTitles) {
    try {
      const card = tile(title);
      if (!await card.count()) continue;
      await closePopups();
      expectedStopTitle = title;
      await card.locator('.stop').click();
      await until(async () => {
        if (!await card.count()) return 'removed';
        return (await card.locator('.session-state').getAttribute('class')) ?? '';
      }, state => state === 'removed' || /\b(stopped|exited)\b/.test(state), 'own session stopped or exited', 45_000);
      if (await card.count()) await card.locator('.stop').click();
      report.cleanup.push({ title, result: 'stopped-own-tile' });
    } catch (error) { report.cleanup.push({ title, result: 'FAIL', error: shortError(error) }); }
    finally { expectedStopTitle = ''; }
  }
}

async function completeExit() {
  await assertOnlyOurs();
  await page.locator('#exit').click();
  await page.locator('#exit-dialog').waitFor({ state: 'visible' });
  // Re-check after opening the concrete confirmation, before the destructive action.
  await assertOnlyOurs();
  const closed = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('Native app did not close after confirmed exit')), 45_000);
    const done = () => { clearTimeout(timer); resolve(); };
    page.once('close', done); browser.once('disconnected', done);
  });
  await page.locator('#confirm-exit').click();
  await closed;
  exited = true;
  return { confirmedThroughVisibleDialog: true, targetClosed: true };
}

try {
  const cdp = new URL(process.env.AGENT_GRID_CDP_URL ?? '');
  assert(cdp.protocol === 'http:' && ['127.0.0.1', 'localhost', '[::1]'].includes(cdp.hostname) && cdp.port && !cdp.username && !cdp.password && cdp.pathname === '/', 'AGENT_GRID_CDP_URL must be an explicit loopback HTTP endpoint with port');
  const requestedCwd = process.env.AGENT_GRID_SMOKE_CWD;
  const requestedOutput = process.env.AGENT_GRID_NATIVE_OUTPUT;
  assert(requestedCwd && path.isAbsolute(requestedCwd), 'AGENT_GRID_SMOKE_CWD must name an absolute directory');
  assert(requestedOutput && path.isAbsolute(requestedOutput), 'AGENT_GRID_NATIVE_OUTPUT must name an absolute output directory');
  const cwd = await realpath(requestedCwd);
  assert((await stat(cwd)).isDirectory(), 'Smoke cwd is not a directory');
  const trustedExistingCwd = process.env.AGENT_GRID_NATIVE_USE_TRUSTED_CWD === '1';
  if (!trustedExistingCwd) assert((await readdir(cwd)).every(name => name === '.git'), 'Smoke cwd must be fresh (empty, or only .git); refusing an existing project without explicit trusted-cwd opt-in');
  report.startupContext = { mode: trustedExistingCwd ? 'existing/read-only user project' : 'fresh isolated fixture', trustedCwdExplicitlySelected: trustedExistingCwd, projectCommandsSent: false, projectFileWritesRequested: false, trustAnswersSent: false };
  outDir = path.resolve(requestedOutput);
  if (process.platform === 'win32') assert(outDir.toLowerCase().startsWith('d:\\codex\\build\\'), 'Native test artifacts must stay under D:/codex/build');
  await mkdir(outDir, { recursive: true });
  report.cwdSha256 = hash(cwd);
  browser = await chromium.connectOverCDP(cdp.href, { timeout: 15_000 });
  await check('own packaged target guard', async () => {
    const candidates = await until(async () => {
      const matches = [];
      for (const context of browser.contexts()) for (const candidate of context.pages()) {
        let url; try { url = new URL(candidate.url()); } catch { continue; }
        if (url.hostname === 'tauri.localhost' && ['http:', 'https:'].includes(url.protocol) && !url.searchParams.has('demo') && await candidate.title() === 'OpenDIY Desktop') matches.push(candidate);
      }
      assert(matches.length <= 1, 'Multiple OpenDIY Desktop targets found; refusing ambiguous ownership');
      return matches;
    }, matches => matches.length === 1, 'exactly one OpenDIY Desktop tauri.localhost target');
    page = candidates[0]; page.setDefaultTimeout(10_000);
    page.on('pageerror', error => report.errors.push(shortError(error)));
    page.on('dialog', async dialog => {
      try {
        if (dialog.type() === 'confirm' && expectedStopTitle && dialog.message().includes(`“${expectedStopTitle}”`)) await dialog.accept();
        else { report.errors.push({ name: 'UnexpectedDialog', message: 'Unexpected browser dialog dismissed; no approval granted' }); await dialog.dismiss(); }
      } catch (error) { report.errors.push(shortError(error)); }
    });
    await until(() => page.locator('#app').getAttribute('aria-busy'), value => value === 'false', '앱 초기화 완료');
    assert.equal(await page.locator('.toast.error').count(), 0, '앱 초기화 오류가 없어야 합니다');
    assert.equal(await page.locator('#runtime-banner').isVisible(), false, 'Demo/browser preview is not native evidence');
    assert.equal(await page.locator('.terminal-card').count(), 0, 'Existing owned tiles present; refusing to touch them');
    assert.equal(await page.locator('.broadcast-panel, [id^=broadcast-], #clear-selection, #selected-count').count(), 0, '선택 전송 패널이 제거되어야 합니다');
    safeInitialState = true;
    return { title: await page.title(), origin: new URL(page.url()).origin, initialTiles: 0 };
  });
  if (process.env.AGENT_GRID_NATIVE_PROJECT_PICKER === '1') {
    assert(process.env.AGENT_GRID_WORKSPACE_DIR && path.isAbsolute(process.env.AGENT_GRID_WORKSPACE_DIR), 'Picker validation requires an explicitly isolated OpenDIY Desktop workspace');
    await check('native project picker cancel preserves empty isolated registry', async () => {
      assert.equal((await nativeCall('list_projects')).length, 0, 'Fixture workspace must start with no imported projects');
      const picker = await projectPicker('Cancel');
      await until(() => page.locator('#project-import').isEnabled(), value => value, 'cancelled picker releases import button');
      assert.equal((await nativeCall('list_projects')).length, 0, 'Cancelled picker registered a project');
      assert.equal(await page.locator('#catalog-dialog').getAttribute('open'), null);
      return { picker, registryUnchanged: true };
    });
    await check('native project selection imports directory and opens filtered session popup', async () => {
      const picker = await projectPicker('Select', cwd);
      await page.locator('#catalog-dialog').waitFor({ state: 'visible', timeout: 30_000 });
      const projects = await nativeCall('list_projects'); assert.equal(projects.length, 1);
      assert.equal(await realpath(projects[0].cwd), cwd);
      assert.equal(await page.locator('#catalog-project').inputValue(), projects[0].cwd);
      const duplicate = await nativeCall('import_project', { cwd });
      assert.equal(duplicate.id, projects[0].id); assert.equal((await nativeCall('list_projects')).length, 1);
      return { picker, selectedCwdSha256: hash(projects[0].cwd), registryCount: 1, duplicateReturnsSameId: true, popupProjectFilterApplied: true };
    });
    await check('actual stored-session lookup, metadata and recent conversation preview', async () => {
      const status = page.locator('#catalog-list-status');
      await until(() => status.textContent(), text => !!text && !text.includes('찾는 중'), 'native catalog query finishes', 45_000);
      const text = await status.textContent(); assert(!text.includes('실패'), 'Native catalog query failed');
      const rows = page.locator('#catalog-list [role="option"]');
      const count = await rows.count(); assert(count > 0, 'Trusted fixture project has no discoverable stored sessions');
      await rows.first().click();
      await until(() => page.locator('#catalog-preview-status').textContent(), value => !value?.includes('불러오는 중'), 'native preview query finishes', 45_000);
      for (let older = 0; older < 3 && !await page.locator('#catalog-messages .catalog-message').count() && await page.locator('#catalog-preview-more').isVisible(); older++) {
        await page.locator('#catalog-preview-more').click();
        await until(() => page.locator('#catalog-preview-status').textContent(), value => !value?.includes('불러오는 중'), 'older preview page finishes', 45_000);
      }
      const previewStatus = (await page.locator('#catalog-preview-status').textContent()) ?? '';
      assert(!previewStatus.includes('실패'), 'Native conversation preview failed');
      const messageCount = await page.locator('#catalog-messages .catalog-message').count();
      assert(messageCount > 0, 'Expected text messages in the bounded real history pages');
      const selected = (await page.locator('#catalog-list [aria-selected="true"]').getAttribute('data-thread-id')) ?? '';
      assert(uuid.test(selected));
      const title = await page.locator('#catalog-session-title').textContent(); assert(title);
      await page.locator('#catalog-search').fill(title.slice(0, 12));
      await until(() => status.textContent(), value => !!value && !value.includes('찾는 중'), 'native title search completes', 45_000);
      assert(!((await status.textContent()) ?? '').includes('실패'));
      assert(await rows.count() > 0, 'Native title search lost its selected title');
      await maskedScreenshot('native-session-popup-masked.png');
      await page.locator('#catalog-close').click();
      return { projectSessionRows: count, previewMessagesRendered: messageCount, selectedThreadIdSha256: hash(selected), titleSearchPerformed: true, existingSessionResumed: false, transcriptSaved: false };
    });
    await check('completed catalog queries leave no owned helper or CLOSE_WAIT socket', async () => {
      const powershell = path.join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe');
      // Fixed code reads the explicitly supplied fixture identity from its own
      // environment. No path or command text is interpolated into this script.
      const command = `$ErrorActionPreference='Stop'; $targetProcessId=[int]$env:AGENT_GRID_NATIVE_PID; $target=Get-Process -Id $targetProcessId; if(-not [string]::Equals([IO.Path]::GetFullPath($target.Path),[IO.Path]::GetFullPath($env:AGENT_GRID_NATIVE_EXE),[StringComparison]::OrdinalIgnoreCase)){throw 'Unexpected fixture process'}; $children=@(Get-CimInstance Win32_Process -Filter ('ParentProcessId='+$targetProcessId)); $connections=@(Get-NetTCPConnection -ErrorAction Stop); @{ownedHelperChildren=@($children|Where-Object Name -eq 'opendiy-desktop-helper.exe').Count; closeWaitSockets=@($connections|Where-Object {$_.OwningProcess -eq $targetProcessId -and $_.State -eq 'CloseWait'}).Count}|ConvertTo-Json -Compress`;
      const deadline = Date.now() + 15_000; let sample;
      do {
        const { stdout } = await execFileAsync(powershell, ['-NoProfile', '-NonInteractive', '-Command', command], { windowsHide: true, timeout: 10_000, maxBuffer: 64 * 1024 });
        sample = JSON.parse(stdout.trim());
        if (sample.ownedHelperChildren === 0 && sample.closeWaitSockets === 0) return { ...sample, scope: 'Only the root-launched native fixture process, after catalog replies and before PTYs start' };
        await new Promise(resolve => setTimeout(resolve, 100));
      } while (Date.now() < deadline);
      assert.fail(`Catalog resources remained: ${JSON.stringify(sample)}`);
    });
  } else report.limitations.push('Native project folder picker and history popup were not tested: AGENT_GRID_NATIVE_PROJECT_PICKER=1 was not supplied.');
  const token = Date.now().toString(36);
  await check('create first real CLI session', () => createSession(`native-smoke-${token}-A`, cwd));
  await check('create second real CLI session', () => createSession(`native-smoke-${token}-B`, cwd));
  await check('native Korean and English switch preserves two live CLI sessions and terminal DOM', async () => {
    const before = await nativeCall('list_sessions');
    assert.equal(before.filter(session => session.active).length, 2);
    const retained = [];
    for (const title of await tileTitles()) {
      const card = tile(title);
      retained.push({ title, path: await card.locator('.terminal-path').textContent(), screen: await card.locator('.xterm-screen').elementHandle(), output: await card.locator('.xterm-rows').innerText() });
    }
    try {
      await openSettings();
      await page.locator('#language-select').selectOption('en');
      await until(() => page.locator('html').getAttribute('lang'), value => value === 'en', 'English language applied');
      assert.match((await page.locator('#new-session').textContent()) ?? '', /new session/i);
      assert.equal(await page.locator('#language-select').inputValue(), 'en');
      assert.equal(await page.evaluate(() => localStorage.getItem('agent-grid-language')), 'en');
      const menu = await nativeCall('set_ui_language', { language: 'en' });
      assert.deepEqual(menu, { show: 'Open OpenDIY Desktop', exit: 'Exit…' }, 'Native menu readback did not match English');
      let invalidRejected = false;
      try { await nativeCall('set_ui_language', { language: 'unsupported-language' }); } catch { invalidRejected = true; }
      assert(invalidRejected, 'Native language command silently accepted an unsupported value');
      await page.locator('#codex-settings-open').click();
      await until(() => page.locator('#codex-settings-dialog').getAttribute('aria-busy'), value => value === 'false', 'English settings read completes');
      assert.match((await page.locator('#codex-settings-title').textContent()) ?? '', /executable/i);
      assert.match((await page.locator('#codex-executable-choose').textContent()) ?? '', /select|choose/i);
      assert(!/[가-힣]/.test((await page.locator('#codex-executable-source').textContent()) ?? ''), 'English settings source contains Korean UI text');
      await page.locator('#codex-settings-close').click();
      await page.locator('#settings-close').click();
      await page.locator('#catalog-open').click();
      await until(() => page.locator('#catalog-list').getAttribute('aria-busy'), value => value === 'false', 'English catalog query completes', 45_000);
      assert.match((await page.locator('#catalog-title').textContent()) ?? '', /session/i);
      assert.match((await page.locator('#catalog-search').getAttribute('placeholder')) ?? '', /session title/i);
      assert.match((await page.locator('#catalog-project option').first().textContent()) ?? '', /all projects/i);
      assert.match((await page.locator('#catalog-close').getAttribute('aria-label')) ?? '', /close/i);
      await page.locator('#catalog-close').click();
      const after = await nativeCall('list_sessions');
      assert.equal(after.length, before.length, 'Language switch changed session count');
      for (const previous of before) {
        const current = after.find(session => session.id === previous.id);
        assert(current?.active, 'Language switch stopped a live CLI session');
        assert.equal(current.generation, previous.generation, 'Language switch restarted a PTY generation');
        assert.equal(current.threadId, previous.threadId, 'Language switch replaced a CLI UUID');
      }
      const sessions = [];
      for (const previous of retained) {
        const card = tile(previous.title);
        assert(previous.screen && await previous.screen.evaluate(element => element.isConnected && element.closest('.terminal-card')?.querySelector('.xterm-screen') === element), 'Language switch replaced an xterm DOM');
        assert.equal(await card.locator('.terminal-title').textContent(), previous.title);
        assert.equal(await card.locator('.terminal-path').textContent(), previous.path);
        const output = await card.locator('.xterm-rows').innerText();
        assert(output.trim().length > 10, 'Language switch cleared rendered terminal output');
        // Real CLIs may asynchronously repaint. Record equality without claiming a quiet PTY.
        sessions.push({ title: previous.title, sameTerminalDom: true, beforeOutputSha256: hash(previous.output), afterOutputSha256: hash(output), outputUnchanged: output === previous.output });
      }
      return { nativeMenuReadback: menu, invalidLanguageRejected: true, sameSessionIdsGenerationsAndThreadUuids: true, sessions, physicalTrayInteractionVerified: false, globalConfigurationWritten: false };
    } finally {
      await closePopups();
      await openSettings();
      await page.locator('#language-select').selectOption('ko');
      await until(() => page.locator('html').getAttribute('lang'), value => value === 'ko', '한국어 복원 완료');
      assert.deepEqual(await nativeCall('set_ui_language', { language: 'ko' }), { show: 'OpenDIY Desktop 열기', exit: '종료…' });
      await page.locator('#settings-close').click();
      for (const previous of retained) await previous.screen?.dispose();
    }
  });
  await check('independent real CLI threads', async () => {
    assert(report.sessions.every(session => session.fullThreadId && uuid.test(session.threadId)), 'Both sessions must expose full thread UUIDs');
    assert.notEqual(report.sessions[0].threadId, report.sessions[1].threadId, 'Two new sessions unexpectedly share a thread UUID');
    return { validFullUuids: true, distinctThreads: true };
  });
  if (process.env.AGENT_GRID_NATIVE_PROJECT_PICKER === '1') {
    await check('native project tabs filter live terminals without replacing PTYs or xterm DOM', async () => {
      const originalProjects = await nativeCall('list_projects');
      assert.equal(originalProjects.length, 1);
      const originalProject = originalProjects[0];
      assert.equal(await realpath(originalProject.cwd), cwd);
      const originalSessions = await nativeCall('list_sessions');
      assert.equal(originalSessions.length, 2);
      const screens = [];
      for (const title of await tileTitles()) screens.push({ title, screen: await tile(title).locator('.xterm-screen').elementHandle() });
      const tab = async projectCwd => {
        const tabs = page.locator('#project-tabs [role="tab"]');
        const index = await tabs.evaluateAll((elements, value) => elements.findIndex(element => element.dataset.projectCwd === value), projectCwd);
        assert(index >= 0, 'Imported project has no selectable main-grid tab');
        return tabs.nth(index);
      };
      const assertLive = async () => {
        const current = await nativeCall('list_sessions');
        assert.equal(current.length, 2, 'Project switching changed the backend session count');
        for (const before of originalSessions) {
          const after = current.find(session => session.id === before.id);
          assert(after && after.active === true, 'Project switching stopped or removed an owned PTY');
          assert.equal(after.generation, before.generation, 'Project switching restarted a PTY generation');
          assert.equal(after.threadId, before.threadId, 'Project switching replaced a CLI thread');
        }
        for (const previous of screens) assert(previous.screen && await previous.screen.evaluate(element => element.isConnected && element.closest('.terminal-card')?.querySelector('.xterm-screen') === element), 'Project switching disposed or replaced an xterm DOM');
        return current.map(({ id, generation, threadId }) => ({ id, generation, threadId }));
      };
      try {
        const projectTab = await tab(originalProject.cwd);
        await projectTab.click();
        await until(() => page.locator('.terminal-card:visible').count(), count => count === 2, 'original project shows both live terminals');
        assert.equal(await projectTab.getAttribute('aria-selected'), 'true');
        assert.equal(await page.locator('#session-list .session-link').count(), 2);
        const emptyDirectory = path.join(outDir, 'empty-project-' + token);
        await mkdir(emptyDirectory); // Fresh fixture only; no CLI is started in this untrusted folder.
        const picker = await projectPicker('Select', emptyDirectory);
        await page.locator('#catalog-dialog').waitFor({ state: 'visible', timeout: 30_000 });
        const projects = await nativeCall('list_projects');
        assert.equal(projects.length, 2);
        const emptyProject = projects.find(project => project.id !== originalProject.id);
        assert(emptyProject);
        assert.equal(await realpath(emptyProject.cwd), await realpath(emptyDirectory));
        assert.equal(await page.locator('#catalog-project').inputValue(), emptyProject.cwd);
        await page.locator('#catalog-close').click();
        const emptyTab = await tab(emptyProject.cwd);
        assert.equal(await emptyTab.getAttribute('aria-selected'), 'true', 'Import did not select its project tab');
        await until(() => page.locator('.terminal-card:visible').count(), count => count === 0, 'empty project hides unrelated terminals');
        assert.equal(await page.locator('#empty').isVisible(), true);
        assert.equal(await page.locator('#session-list .session-link').count(), 0);
        assert.equal(await page.locator('.terminal-card').count(), 2, 'Filtering removed hidden terminal nodes');
        await assertLive();
        await (await tab('')).click();
        await until(() => page.locator('.terminal-card:visible').count(), count => count === 2, 'all-project tab restores both terminals');
        await assertLive();
        await (await tab(originalProject.cwd)).click();
        await until(() => page.locator('.terminal-card:visible').count(), count => count === 2, 'returning to original project restores both terminals');
        const retained = [];
        for (const previous of screens) {
          const text = await tile(previous.title).locator('.terminal-host').innerText();
          assert(text.trim().length > 10, 'Terminal output is empty after project switching');
          retained.push({ sameTerminalDom: true, outputLength: text.trim().length, outputSha256: hash(text) });
        }
        return { picker, registryCount: 2, selectedImportedProject: true, emptyProjectVisibleTerminals: 0, originalProjectVisibleTerminals: 2, allProjectsVisibleTerminals: 2, sessions: await assertLive(), terminals: retained, hiddenOutputArrivalTimingVerified: false, additionalCliStarted: false };
      } finally {
        await closePopups();
        await (await tab('')).click();
        for (const previous of screens) await previous.screen?.dispose();
      }
    });
  } else report.limitations.push('Native project tabs were not tested: AGENT_GRID_NATIVE_PROJECT_PICKER=1 was not supplied.');
  if (process.env.AGENT_GRID_NATIVE_EXECUTABLE_PICKER === '1') {
    const workspace = process.env.AGENT_GRID_WORKSPACE_DIR;
    assert(workspace && path.isAbsolute(workspace), 'Executable picker validation requires an explicitly isolated workspace');
    const relativeWorkspace = path.relative(outDir, await realpath(workspace));
    assert(relativeWorkspace && relativeWorkspace !== '..' && !relativeWorkspace.startsWith('..' + path.sep) && !path.isAbsolute(relativeWorkspace), 'Settings writes must stay inside this fixture output directory');
    const executablePath = process.env.AGENT_GRID_CODEX;
    assert(executablePath && path.isAbsolute(executablePath) && path.extname(executablePath).toLowerCase() === '.exe', 'Provide the actual native Codex executable as AGENT_GRID_CODEX');
    const canonicalExecutable = await realpath(executablePath);
    assert((await stat(canonicalExecutable)).isFile());
    const { DatabaseSync } = await import('node:sqlite');
    const persistedExecutable = () => {
      const database = new DatabaseSync(path.join(workspace, 'workspace.sqlite3'), { readOnly: true });
      try { return database.prepare("SELECT value FROM app_settings WHERE key = 'codex_executable'").get()?.value ?? null; }
      finally { database.close(); }
    };
    const originalSessions = await nativeCall('list_sessions');
    assert.equal(originalSessions.length, 2);
    const screens = [];
    for (const title of await tileTitles()) screens.push({ title, screen: await tile(title).locator('.xterm-screen').elementHandle() });
    const assertLiveSessions = async () => {
      await assertOnlyOurs();
      const current = await nativeCall('list_sessions');
      assert.equal(current.length, originalSessions.length, 'Settings changed the live session count');
      for (const before of originalSessions) {
        const after = current.find(session => session.id === before.id);
        assert(after, 'Settings removed an existing PTY');
        assert.equal(after.generation, before.generation, 'Settings restarted an existing PTY generation');
        assert.equal(after.threadId, before.threadId, 'Settings replaced an existing CLI thread');
        assert(!['stopping', 'stopped', 'exited', 'error'].includes(after.state), 'Settings stopped an existing PTY');
      }
      for (const previous of screens) {
        assert(previous.screen && await previous.screen.evaluate(element => element.isConnected && element.closest('.terminal-card')?.querySelector('.xterm-screen') === element), 'Settings disposed the existing terminal DOM');
        assert((await tile(previous.title).locator('.terminal-host').innerText()).trim().length > 10);
      }
      return current.map(({ id, generation, threadId }) => ({ id, generation, threadId }));
    };
    const settingsDialog = page.locator('#codex-settings-dialog');
    try {
      await check('native executable picker cancel preserves initial isolated settings and live PTYs', async () => {
        const before = await nativeCall('get_codex_settings');
        assert.equal(before.savedPath, null, 'Fresh isolated settings already have a saved executable');
        assert.equal(persistedExecutable(), null);
        await openSettings();
        await page.locator('#codex-settings-open').click();
        await settingsDialog.waitFor({ state: 'visible' });
        const picker = await executablePicker('Cancel');
        await until(() => page.locator('#codex-executable-choose').isEnabled(), value => value, 'cancelled executable picker releases UI');
        assert.deepEqual(await nativeCall('get_codex_settings'), before);
        assert.equal(persistedExecutable(), null);
        return { picker, savedSettingsUnchanged: true, sessions: await assertLiveSessions() };
      });
      await check('native executable selection persists exact file and settings reopen with live PTYs', async () => {
        const picker = await executablePicker('Select', canonicalExecutable);
        const saved = await until(() => nativeCall('get_codex_settings'), value => value.source === 'saved' && !!value.savedPath, 'executable selection saved');
        assert.equal(await realpath(saved.savedPath), canonicalExecutable);
        assert.equal(await realpath(saved.resolvedPath), canonicalExecutable);
        assert.equal(saved.error, null);
        assert.equal(persistedExecutable(), saved.savedPath, 'SQLite setting differs from selected file');
        await until(() => page.locator('#codex-executable-path').inputValue(), value => value === saved.savedPath, 'saved executable appears in UI');
        await page.locator('#codex-settings-close').click();
        await page.locator('#codex-settings-open').click();
        await until(() => page.locator('#codex-executable-path').inputValue(), value => value === saved.savedPath, 'reopened settings reload saved executable');
        const cancelled = await executablePicker('Cancel');
        await until(() => page.locator('#codex-executable-choose').isEnabled(), value => value, 'saved-setting cancel completes');
        assert.deepEqual(await nativeCall('get_codex_settings'), saved);
        assert.equal(persistedExecutable(), saved.savedPath);
        return { picker, cancelled, savedPathSha256: hash(saved.savedPath), resolvedPathSha256: hash(saved.resolvedPath), source: saved.source, sqliteMatchesSelectedPath: true, uiReopened: true, savedSettingPreservedOnCancel: true, appRestartVerified: false, sessions: await assertLiveSessions() };
      });
      await check('native invalid executable choice exposes error and preserves saved setting and live PTYs', async () => {
        const before = await nativeCall('get_codex_settings');
        const invalidExecutable = path.join(outDir, 'invalid-native-cli.exe');
        await writeFile(invalidExecutable, 'OpenDIY Desktop fixture: this is not a Windows PE executable.\n', { encoding: 'utf8', flag: 'wx' });
        const picker = await executablePicker('Select', invalidExecutable);
        await page.locator('#codex-settings-error').waitFor({ state: 'visible' });
        const visibleError = (await page.locator('#codex-settings-error').textContent()) ?? '';
        assert(visibleError.trim(), 'Invalid executable selection gave no visible reason');
        assert.deepEqual(await nativeCall('get_codex_settings'), before);
        assert.equal(persistedExecutable(), before.savedPath);
        let missingPathError = '';
        try { await nativeCall('set_codex_executable', { path: path.join(outDir, 'does-not-exist-' + randomUUID() + '.exe') }); }
        catch (error) { missingPathError = String(error); }
        assert(missingPathError, 'Missing executable path was silently accepted');
        assert.deepEqual(await nativeCall('get_codex_settings'), before);
        assert.equal(persistedExecutable(), before.savedPath);
        return { picker, visibleErrorSha256: hash(visibleError), missingPathErrorSha256: hash(missingPathError), invalidSelectionPreservedPriorSetting: true, missingPathRejected: true, sessions: await assertLiveSessions() };
      });
      await check('native executable reset deletes only saved choice and exposes fallback without restarting live PTYs', async () => {
        await page.locator('#codex-executable-reset').click();
        const reset = await until(() => nativeCall('get_codex_settings'), value => value.savedPath === null && value.source !== 'saved', 'executable reset selects fallback');
        assert.equal(persistedExecutable(), null, 'Reset left saved SQLite row');
        assert.equal(reset.source, 'environment', 'Fixture AGENT_GRID_CODEX fallback is not explicit');
        assert.equal(await realpath(reset.resolvedPath), canonicalExecutable);
        assert.equal(reset.error, null);
        await until(() => page.locator('#codex-executable-path').inputValue(), value => value === '', 'reset clears saved path UI');
        assert((await page.locator('#codex-executable-source').textContent())?.trim(), 'Reset fallback source is not visible');
        assert((await page.locator('#codex-executable-resolved').textContent())?.includes(reset.resolvedPath), 'Reset resolved path is not visible');
        return { savedPath: null, source: reset.source, resolvedPathSha256: hash(reset.resolvedPath), sqliteSavedRowRemoved: true, fallbackExplicit: true, sessions: await assertLiveSessions() };
      });
      await page.locator('#codex-settings-close').click();
      await page.locator('#settings-close').click();
    } finally {
      for (const previous of screens) await previous.screen?.dispose();
    }
  } else report.limitations.push('Native executable picker/settings were not tested: AGENT_GRID_NATIVE_EXECUTABLE_PICKER=1 was not supplied.');
  await check('nonexistent resume can retry as a distinct inactive session and both error tiles close', async () => {
    const title = `native-smoke-${token}-invalid-resume`;
    const retryTitle = `native-smoke-${token}-retry-resume`;
    const requestedThreadId = randomUUID();
    assert(!report.sessions.some(session => session.threadId === requestedThreadId));
    ownedTitles.add(title);
    await page.locator('#new-session').click();
    await page.locator('#session-title').fill(title);
    await page.locator('#session-cwd').fill(cwd);
    await page.locator('#session-thread').fill(requestedThreadId);
    await page.locator('#create-submit').click();
    const errorText = await until(async () => {
      const dialogOpen = await page.locator('#create-dialog').getAttribute('open');
      assert(dialogOpen !== null, 'Invalid resume unexpectedly succeeded or fell back to a fresh session');
      return await page.locator('#create-error').isVisible() ? await page.locator('#create-error').textContent() : '';
    }, value => !!value, 'nonexistent resume exposes a creation error', sessionTimeout);
    assert(/thread|resume|rollout|스레드|대화/i.test(errorText), 'Resume failure must identify a thread/resume error');
    const card = tile(title);
    await until(() => card.locator('.session-state').getAttribute('class'), state => /\berror\b/.test(state ?? ''), 'failed resume has an error tile');
    assert.equal(await card.locator('.terminal-identity').getAttribute('title'), requestedThreadId, 'Invalid resume was replaced with another thread ID');
    const originalId = await card.getAttribute('data-id');
    assert(originalId && uuid.test(originalId), 'Original failed resume has no exact session ID');
    const original = await until(async () => (await nativeCall('list_sessions')).find(session => session.id === originalId), session => session?.active === false, 'first failed resume releases its live ownership');
    assert.equal(original.threadId, requestedThreadId);
    assert.equal(original.state, 'error');
    assert.equal(await page.locator('#session-thread').inputValue(), requestedThreadId);
    assert.equal(await page.locator('#session-cwd').inputValue(), cwd);
    ownedTitles.add(retryTitle);
    await page.locator('#session-title').fill(retryTitle);
    await page.locator('#create-submit').click();
    const retryCard = tile(retryTitle);
    await retryCard.waitFor({ state: 'visible', timeout: sessionTimeout });
    const retryId = await retryCard.getAttribute('data-id');
    assert(retryId && uuid.test(retryId), 'Retry did not create a new exact session ID');
    assert.notEqual(retryId, originalId, 'Retry focused or reused the original failed card');
    const retry = await until(async () => (await nativeCall('list_sessions')).find(session => session.id === retryId), session => session?.active === false, 'retry failed resume releases its live ownership', sessionTimeout);
    assert.equal(retry.threadId, requestedThreadId);
    assert.equal(retry.cwd, original.cwd);
    assert.equal(retry.state, 'error');
    await until(() => retryCard.locator('.session-state').getAttribute('class'), state => /\berror\b/.test(state ?? ''), 'distinct retry card reaches error');
    assert.equal(await retryCard.locator('.terminal-identity').getAttribute('title'), requestedThreadId, 'Retry fell back to a fresh thread');
    assert.notEqual(await page.locator('#create-dialog').getAttribute('open'), null, 'Retry unexpectedly succeeded or closed the error form');
    assert.equal(await card.count(), 1, 'Retry unexpectedly removed the first error card');
    await page.locator('#create-dialog').getByRole('button', { name: '취소', exact: true }).click();
    for (const failedTitle of [title, retryTitle]) {
      expectedStopTitle = failedTitle;
      try {
        const failedCard = tile(failedTitle);
        await failedCard.locator('.stop').click();
        await failedCard.waitFor({ state: 'detached', timeout: 45_000 });
      } finally { expectedStopTitle = ''; }
    }
    await assertOnlyOurs();
    assert.equal(await page.locator('.terminal-card').count(), 2, 'Failed resume or retry left a tile or created an extra session');
    const remaining = await nativeCall('list_sessions');
    assert.equal(remaining.filter(session => session.active).length, 2, 'Retry cleanup left an extra live session');
    assert(remaining.filter(session => [originalId, retryId].includes(session.id)).every(session => session.active === false), 'Closed error card still owns a live session');
    for (const live of report.sessions) {
      const preserved = remaining.find(session => session.id === live.id);
      assert(preserved && preserved.active === true, 'Retry cleanup removed or stopped an original real CLI session');
      assert.equal(preserved.threadId, live.threadId);
    }
    return { requestedThreadId, originalSessionId: originalId, retrySessionId: retryId, distinctSessionIds: true, exactRequestedThreadRetainedBoth: true, originalActive: original.active, retryActive: retry.active, errorDisplayed: true, errorSha256: hash(errorText), retryCompletionOracle: 'new unique card ID plus backend inactive error metadata; existing error text is not a completion signal', freshFallbackOccurred: false, bothErrorTilesRemovedAfterNativeStop: true, originalRealSessionsPreserved: true };
  });
  await check('native terminal focus, layout resize and reorder', async () => {
    await assertOnlyOurs();
    await page.getByRole('button', { name: '2열', exact: true }).click();
    const firstTitle = [...ownedTitles][0]; const secondTitle = [...ownedTitles][1];
    const first = tile(firstTitle);
    const before = await first.boundingBox(); assert(before, 'First tile has no geometry');
    const separator = page.getByRole('separator', { name: '1번째 열 크기 조절' });
    const bounds = await separator.boundingBox(); assert(bounds, 'Resize separator is missing');
    await page.mouse.move(bounds.x + bounds.width / 2, bounds.y + 60); await page.mouse.down();
    await page.mouse.move(bounds.x + bounds.width / 2 + 65, bounds.y + 60, { steps: 6 }); await page.mouse.up();
    const width = await until(async () => (await first.boundingBox())?.width ?? 0, value => value > before.width + 30, 'native tile resize');
    await first.locator('.drag-handle').dragTo(tile(secondTitle).locator('.terminal-header'));
    await until(async () => (await tileTitles())[0], value => value === secondTitle, '점 손잡이로 실제 세션 순서 변경');
    await page.keyboard.press('Alt+1');
    assert((await tile(secondTitle).getAttribute('class')).includes('focused'), 'Keyboard focus did not move to first tile');
    await tile(secondTitle).locator('.maximize').click();
    assert.equal(await first.isVisible(), false, 'Maximize did not hide other tile');
    await page.keyboard.press('Escape');
    assert.equal(await first.isVisible(), true, 'Escape did not restore grid');
    return { beforeWidth: before.width, afterWidth: width, orderChanged: true, reorderInput: '실제 점 손잡이 드래그', keyboardFocus: true, maximizeRestored: true };
  });
  await check('terminal search UI', async () => {
    await page.locator('#search-toggle').click();
    await page.locator('#terminal-search').fill('codex');
    await page.locator('#search-next').click();
    const result = await page.locator('#search-result').textContent();
    assert(['결과 찾음', '결과 없음'].includes(result), 'Search did not produce a result status');
    await page.locator('#search-close').click();
    return { searchResult: result };
  });
  await check('synthetic Korean composition reaches actual PTY echo (not physical IME)', async () => {
    const card = tile([...ownedTitles][0]);
    assert.equal(await page.locator('.select-session, .terminal-header input[type=checkbox]').count(), 0, '선택 전송 체크박스가 제거되어야 합니다');
    const marker = '한글입력검사';
    // Read only VT-rendered rows, excluding xterm's local composition overlay and textarea.
    const rows = card.locator('.xterm-rows');
    await rows.waitFor({ state: 'visible' });
    const before = await rows.innerText();
    assert(!before.includes(marker), 'Korean echo marker must be absent before synthetic input');
    await card.locator('.terminal-host').click();
    const events = await card.locator('textarea.xterm-helper-textarea').evaluate((element, marker) => {
      const seen = [];
      const record = event => seen.push({ type: event.type, trusted: event.isTrusted });
      for (const type of ['compositionstart', 'compositionupdate', 'compositionend']) element.addEventListener(type, record);
      element.focus();
      element.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true, data: '' }));
      element.value = marker;
      element.dispatchEvent(new CompositionEvent('compositionupdate', { bubbles: true, data: marker }));
      element.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: marker }));
      for (const type of ['compositionstart', 'compositionupdate', 'compositionend']) element.removeEventListener(type, record);
      return seen;
    }, marker);
    assert.deepEqual(events.map(event => event.type), ['compositionstart', 'compositionupdate', 'compositionend']);
    assert(events.every(event => event.trusted === false));
    const dispatchedAt = Date.now();
    const echoed = await until(() => rows.innerText(), text => text.includes(marker), 'actual Korean marker echo after composition end', 15_000);
    // 정리용 ownedTitles에는 이미 닫은 실패 재개 카드도 남아 있다.
    // 실제 생성한 다른 실행 세션의 고정 ID로 입력 격리를 검사한다.
    for (const session of report.sessions.slice(1)) {
      const peer = page.locator(`.terminal-card[data-id="${session.id}"]`);
      assert.equal(await peer.count(), 1, '입력 격리를 검사할 다른 실행 세션이 있어야 합니다');
      assert(!(await peer.locator('.xterm-rows').innerText()).includes(marker), '직접 입력이 다른 터미널에 전달되면 안 됩니다');
    }
    return { events, marker, markerInitiallyAbsent: true, echoObservedIn: 'VT-rendered xterm rows (composition overlay excluded)', echoWaitMs: Date.now() - dispatchedAt, beforeOutputLength: before.length, afterOutputLength: echoed.length, beforeOutputSha256: hash(before), afterOutputSha256: hash(echoed), enterSent: false, physicalImeVerified: false, koreanPtyRoundTripVerified: true, unsubmittedTextCleared: false, disposition: 'Left unsubmitted until the owned session is stopped; no Ctrl+U sent to a potentially waiting session.' };
  });
  await check('external observations stay read only and report unavailable daemon explicitly', async () => {
    await openSettings();
    await page.locator('#external-toggle').click();
    const dialog = page.locator('#external-dialog'); await dialog.waitFor({ state: 'visible' });
    await until(() => page.locator('#external-list').textContent(), text => !!text && !text.includes('불러오는 중'), 'external read-only query');
    assert.equal(await dialog.locator('input, textarea, .stop, .external-item button, .external-item [role="button"]').count(), 0, 'External records expose write controls');
    const list = await page.locator('#external-list').textContent();
    const unavailable = list.includes('불러오기 실패:');
    if (unavailable) assert(/daemon|데몬/i.test(list), 'Unavailable external observer must explain the daemon connection failure');
    const observedRows = await dialog.locator('.external-item').count();
    if (observedRows) assert((await dialog.locator('.external-item small').allTextContents()).some(text => text.includes('현재 상태 미확인')), 'Unknown external state must be explicit');
    await dialog.getByRole('button', { name: '닫기', exact: true }).click();
    await page.locator('#settings-close').click();
    return { observedRows, readOnly: true, available: !unavailable, ...(unavailable ? { unavailableReasonSha256: hash(list), explicitUnavailableDisplayed: true } : {}), externalProcessesTouched: false };
  });
  if (process.env.AGENT_GRID_NATIVE_PID || process.env.AGENT_GRID_NATIVE_EXE) {
    await check('own native window close hides and same HWND restores', async () => {
      assert.equal(process.platform, 'win32', 'Native window probe requires Windows');
      const processId = process.env.AGENT_GRID_NATIVE_PID;
      const executable = process.env.AGENT_GRID_NATIVE_EXE;
      assert(processId && /^[1-9]\d{0,9}$/.test(processId) && Number(processId) <= 2147483647, 'AGENT_GRID_NATIVE_PID must be a positive process ID');
      assert(executable && path.isAbsolute(executable), 'AGENT_GRID_NATIVE_EXE must be the exact absolute launched executable');
      await assertOnlyOurs();
      const before = [];
      for (const title of await tileTitles()) {
        const card = tile(title);
        before.push({ title, threadId: await card.locator('.terminal-identity').getAttribute('title'), screen: await card.locator('.xterm-screen').elementHandle(), text: await card.locator('.terminal-host').innerText() });
      }
      const script = fileURLToPath(new URL('./window-probe.ps1', import.meta.url));
      const powershell = path.join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'WindowsPowerShell', 'v1.0', 'powershell.exe');
      let probeOutput;
      try {
        probeOutput = await execFileAsync(powershell, ['-NoProfile', '-NonInteractive', '-File', script, '-AppProcessId', processId, '-ExpectedExecutable', executable], { windowsHide: true, timeout: 20_000, maxBuffer: 64 * 1024 });
      } catch (error) {
        // This child runs only our bounded Win32 probe, never a shell/CLI transcript.
        // Preserve its diagnostic stream so a visibility timeout and a failed guard
        // remain distinguishable after shortError removes multiline messages.
        error.windowProbe = { script: 'tests/window-probe.ps1', code: error.code ?? null, signal: error.signal ?? null, killed: !!error.killed, stdout: probeDiagnostic(error.stdout), stderr: probeDiagnostic(error.stderr), diagnosticLimit: '40 lines / 4096 characters per stream' };
        throw error;
      }
      let window;
      try { window = JSON.parse(probeOutput.stdout.trim()); }
      catch (error) {
        error.windowProbe = { script: 'tests/window-probe.ps1', code: 0, signal: null, killed: false, stdout: probeDiagnostic(probeOutput.stdout), stderr: probeDiagnostic(probeOutput.stderr), diagnosticLimit: '40 lines / 4096 characters per stream' };
        throw error;
      }
      assert(window.hidden && window.restored && window.sameHandle && window.processIdentityPreserved, 'Native window did not hide and restore safely');
      const retained = [];
      for (const previous of before) {
        const card = tile(previous.title);
        const threadId = await card.locator('.terminal-identity').getAttribute('title');
        assert.equal(threadId, previous.threadId, 'Thread UUID changed during native hide/reopen');
        assert(previous.screen && await previous.screen.evaluate(element => element.isConnected && element.closest('.terminal-card')?.querySelector('.xterm-screen') === element), 'Existing xterm DOM was disposed during native hide/reopen');
        const text = await card.locator('.terminal-host').innerText();
        assert(text.trim().length > 10, 'Terminal output DOM is empty after native hide/reopen');
        retained.push({ threadId, sameTerminalDom: true, beforeOutputLength: previous.text.trim().length, afterOutputLength: text.trim().length, beforeOutputSha256: hash(previous.text), afterOutputSha256: hash(text), outputUnchanged: previous.text === text });
        await previous.screen.dispose();
      }
      return { ...window, sessions: retained, trayIconClickVerified: false };
    });
  } else report.limitations.push('Native window close/hide/reopen was not tested: AGENT_GRID_NATIVE_PID and AGENT_GRID_NATIVE_EXE were not supplied.');
  await check('masked native UI screenshot', async () => { await maskedScreenshot('native-ui-masked.png'); return { file: 'native-ui-masked.png', terminalTextMasked: true }; });
} catch (error) {
  report.errors.push(shortError(error));
  if (outDir) try { await maskedScreenshot('native-failure-masked.png'); } catch (screenshotError) { report.errors.push(shortError(screenshotError)); }
} finally {
  if (page && !page.isClosed() && safeInitialState) {
    try { await stopOurs(); await check('confirmed native exit', completeExit); }
    catch (error) { report.errors.push(shortError(error)); }
  }
  report.finishedAt = new Date().toISOString();
  report.exitObserved = exited;
  report.result = report.errors.length === 0 && report.checks.every(item => item.result === 'PASS') && report.cleanup.every(item => item.result !== 'FAIL') && exited ? 'PASS' : 'FAIL';
  if (outDir) await writeFile(path.join(outDir, 'native-smoke.json'), JSON.stringify(report, null, 2) + '\n', 'utf8');
  process.stdout.write(JSON.stringify({ result: report.result, artifact: outDir ? path.join(outDir, 'native-smoke.json') : null, checks: report.checks.length, errors: report.errors }) + '\n');
  // Disconnect this inspector by exiting our own process; never Browser.close()
  // an attached user's WebView2 instance, especially after a failed ownership guard.
  process.exit(report.result === 'PASS' ? 0 : 1);
}
