// Opt-in test of a root-launched packaged Agent Grid WebView2. Never starts a browser,
// changes app config, approves a terminal prompt, or sends an Enter key to a terminal.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdir, readdir, realpath, stat, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { chromium } from '@playwright/test';

const report = {
  schema: 'agent-grid-native-smoke.v1',
  startedAt: new Date().toISOString(),
  runtime: 'packaged WebView2 through opt-in CDP',
  checks: [], sessions: [], errors: [], cleanup: [],
  limitations: ['Synthetic composition events do not verify a physical Windows IME or a Korean PTY round trip.', 'No model turn, trust approval, or terminal Enter is sent.', 'Screenshots mask all terminal text and observed external records.'],
};
const shortError = error => ({ name: error?.name ?? 'Error', message: String(error?.message ?? error).split('\n')[0].slice(0, 400) });
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
  await page.screenshot({ path: path.join(outDir, filename), fullPage: true, mask: [page.locator('.terminal-host'), page.locator('.terminal-path'), page.locator('#external-list'), page.locator('#notification-list'), page.locator('#restore-list')], maskColor: '#141820' });
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
  }, info => !!info.id && !!info.threadId && !!info.state && info.outputLength > 10, 'real CLI thread metadata, observed state and terminal text', sessionTimeout);
  assert(/^[a-zA-Z0-9_-]{12,128}$/.test(info.threadId), 'Invalid UI thread identifier');
  report.sessions.push(info);
  return info;
}

async function stopOurs() {
  for (const title of ownedTitles) {
    try {
      const card = tile(title);
      if (!await card.count()) continue;
      const openDialog = page.locator('dialog[open]');
      if (await openDialog.count()) await page.keyboard.press('Escape');
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
  assert(requestedCwd && path.isAbsolute(requestedCwd), 'AGENT_GRID_SMOKE_CWD must name a fresh absolute directory');
  assert(requestedOutput && path.isAbsolute(requestedOutput), 'AGENT_GRID_NATIVE_OUTPUT must name an absolute output directory');
  const cwd = await realpath(requestedCwd);
  assert((await stat(cwd)).isDirectory(), 'Smoke cwd is not a directory');
  assert((await readdir(cwd)).every(name => name === '.git'), 'Smoke cwd must be fresh (empty, or only .git); refusing an existing project');
  outDir = path.resolve(requestedOutput);
  if (process.platform === 'win32') assert(outDir.toLowerCase().startsWith('d:\\codex\\build\\'), 'Native test artifacts must stay under D:/codex/build');
  await mkdir(outDir, { recursive: true });
  report.cwdSha256 = hash(cwd);
  browser = await chromium.connectOverCDP(cdp.href, { timeout: 15_000 });
  await check('own packaged target guard', async () => {
    const candidates = [];
    for (const context of browser.contexts()) for (const candidate of context.pages()) {
      let url; try { url = new URL(candidate.url()); } catch { continue; }
      if (url.hostname === 'tauri.localhost' && ['http:', 'https:'].includes(url.protocol) && !url.searchParams.has('demo') && await candidate.title() === 'Agent Grid') candidates.push(candidate);
    }
    assert.equal(candidates.length, 1, 'Expected exactly one Agent Grid tauri.localhost target');
    page = candidates[0]; page.setDefaultTimeout(10_000);
    page.on('pageerror', error => report.errors.push(shortError(error)));
    page.on('dialog', async dialog => {
      if (dialog.type() === 'confirm' && expectedStopTitle && dialog.message().includes(`“${expectedStopTitle}”`)) await dialog.accept();
      else { report.errors.push({ name: 'UnexpectedDialog', message: 'Unexpected browser dialog dismissed; no approval granted' }); await dialog.dismiss(); }
    });
    await until(() => page.locator('#connection').textContent(), text => text === '로컬 연결됨', 'native IPC connected');
    assert.equal(await page.locator('#runtime-banner').isVisible(), false, 'Demo/browser preview is not native evidence');
    assert.equal(await page.locator('.terminal-card').count(), 0, 'Existing owned tiles present; refusing to touch them');
    safeInitialState = true;
    return { title: await page.title(), origin: new URL(page.url()).origin, initialTiles: 0 };
  });
  const token = Date.now().toString(36);
  await check('create first real CLI session', () => createSession(`native-smoke-${token}-A`, cwd));
  await check('create second real CLI session', () => createSession(`native-smoke-${token}-B`, cwd));
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
    await first.locator('.move-right').click();
    assert.equal((await tileTitles())[0], secondTitle, 'Tile reorder failed');
    await page.keyboard.press('Alt+1');
    assert((await tile(secondTitle).getAttribute('class')).includes('focused'), 'Keyboard focus did not move to first tile');
    await tile(secondTitle).locator('.maximize').click();
    assert.equal(await first.isVisible(), false, 'Maximize did not hide other tile');
    await page.keyboard.press('Escape');
    assert.equal(await first.isVisible(), true, 'Escape did not restore grid');
    return { beforeWidth: before.width, afterWidth: width, orderChanged: true, keyboardFocus: true, maximizeRestored: true };
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
  await check('synthetic Korean terminal composition dispatch (not physical IME)', async () => {
    const card = tile([...ownedTitles][0]);
    await card.locator('.terminal-host').click();
    const events = await card.locator('textarea.xterm-helper-textarea').evaluate(element => {
      const seen = [];
      const record = event => seen.push({ type: event.type, trusted: event.isTrusted });
      for (const type of ['compositionstart', 'compositionupdate', 'compositionend']) element.addEventListener(type, record);
      element.focus();
      element.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true, data: '' }));
      element.value = '한글';
      element.dispatchEvent(new CompositionEvent('compositionupdate', { bubbles: true, data: '한글' }));
      element.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true, data: '한글' }));
      for (const type of ['compositionstart', 'compositionupdate', 'compositionend']) element.removeEventListener(type, record);
      return seen;
    });
    assert.deepEqual(events.map(event => event.type), ['compositionstart', 'compositionupdate', 'compositionend']);
    assert(events.every(event => event.trusted === false));
    return { events, enterSent: false, physicalImeVerified: false, koreanPtyRoundTripVerified: false };
  });
  await check('external observations stay read only', async () => {
    await page.locator('#external-toggle').click();
    const dialog = page.locator('#external-dialog'); await dialog.waitFor({ state: 'visible' });
    await until(() => page.locator('#external-list').textContent(), text => !!text && !text.includes('불러오는 중'), 'external read-only query');
    assert.equal(await dialog.locator('input, textarea, .stop, .restore-item button').count(), 0, 'External records expose write controls');
    const list = await page.locator('#external-list').textContent();
    assert(!list.includes('불러오기 실패:'), 'External observer command failed');
    const observedRows = await dialog.locator('.external-item').count();
    if (observedRows) assert((await dialog.locator('.external-item small').allTextContents()).some(text => text.includes('현재 상태 미확인')), 'Unknown external state must be explicit');
    await dialog.getByRole('button', { name: '닫기', exact: true }).click();
    return { observedRows, readOnly: true, externalProcessesTouched: false };
  });
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
