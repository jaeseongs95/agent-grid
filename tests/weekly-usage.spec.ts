import { test, expect, type Page } from '@playwright/test';

const snapshot = {
  executable: 'D:\\선택한 도구\\codex.exe', observedAt: Date.parse('2026-10-29T00:00:00Z'), limitName: '기본 계정',
  weekly: { usedPercent: 42.7, resetsAt: Date.parse('2026-10-30T08:00:00Z') / 1000 },
};
const readFailure = '사용량을 불러오지 못했습니다. 잠시 후 새로고침해 주세요.';
const lastKnownFailure = readFailure + ' 마지막 정상값을 표시합니다.';

async function fixture(page: Page, options: { hold?: boolean; hidden?: boolean; weekly?: null; error?: string } = {}) {
  await page.clock.install({ time: new Date('2026-10-29T00:00:00Z') });
  await page.clock.pauseAt(new Date('2026-10-29T00:00:01Z'));
  await page.route('**/weekly-usage-harness', route => route.fulfill({ contentType: 'text/html', body: '<!doctype html><html><body></body></html>' }));
  await page.goto('/weekly-usage-harness');
  await page.evaluate(async ({ snapshot, options }) => {
    const modulePath = '/src/weekly-usage.ts';
    const { createWeeklyUsage } = await import(modulePath);
    const languagePath = '/src/i18n.ts';
    const { setLanguage } = await import(languagePath);
    const f = (window as any).usageFixture = {
      calls: [] as { command: string; time: number; manual: boolean }[], active: 0, maxActive: 0, hidden: options.hidden ?? false,
      hold: options.hold ?? false, error: options.error ?? '', response: { ...snapshot, ...(options.weekly === null ? { weekly: null } : {}) },
      pending: [] as (() => void)[], setLanguage,
      release() { const done = this.pending.shift(); if (!done) throw new Error('No held usage read'); done(); },
      visibility(hidden: boolean) { this.hidden = hidden; document.dispatchEvent(new Event('visibilitychange')); },
    };
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => f.hidden });
    f.widget = createWeeklyUsage({ async call(command: string, args?: { manual?: boolean }) {
      f.calls.push({ command, time: performance.now(), manual: args?.manual ?? false }); f.active++; f.maxActive = Math.max(f.maxActive, f.active);
      const result = structuredClone(f.response); const failure = f.error;
      try {
        if (f.hold) await new Promise<void>(resolve => f.pending.push(resolve));
        if (failure) throw new Error(failure);
        return result;
      } finally { f.active--; }
    } });
    document.body.append(f.widget.element);
  }, { snapshot, options });
}
async function count(page: Page) { return page.evaluate(() => (window as any).usageFixture.calls.length); }

test('실제 주간 비율과 현지 초기화 시각을 표시하고 언어를 전환한다', async ({ page }) => {
  await fixture(page);
  await expect(page.locator('#weekly-usage-value')).toHaveText('42.7%');
  await expect(page.locator('#weekly-usage-reset')).toContainText('초기화');
  await expect(page.locator('#weekly-usage-reset')).toContainText('30');
  await expect(page.locator('#weekly-usage-meter')).toHaveAttribute('value', '42.7');
  await expect(page.locator('#weekly-usage')).toHaveAttribute('title', '기본 계정');
  await page.evaluate(() => (window as any).usageFixture.setLanguage('en'));
  await expect(page.locator('#weekly-usage-label')).toHaveText('Weekly usage');
  await expect(page.locator('#weekly-usage-reset')).toContainText('Resets');
  expect(await count(page)).toBe(1);
});

test('미제공·인증 오류·초기화 시각 부재를 0퍼센트로 바꾸지 않는다', async ({ page }) => {
  await fixture(page, { weekly: null });
  await expect(page.locator('#weekly-usage-status')).toHaveText('주간 사용량 제공 안 됨');
  await expect(page.locator('#weekly-usage-value')).toHaveText('—');
  await expect(page.locator('#weekly-usage-meter')).toBeHidden();
  await page.evaluate(() => { const f = (window as any).usageFixture; f.error = 'chatgpt authentication required'; });
  await page.clock.runFor(60_000);
  await expect(page.locator('#weekly-usage-status')).toHaveText(readFailure);
  await expect(page.locator('#weekly-usage-value')).toHaveText('—');
  await page.evaluate(() => { const f = (window as any).usageFixture; f.error = ''; f.response.weekly = { usedPercent: 0, resetsAt: null }; });
  await page.clock.runFor(60_000);
  await expect(page.locator('#weekly-usage-value')).toHaveText('0%');
  await expect(page.locator('#weekly-usage-reset')).toHaveText('초기화 시각 확인 불가');
});

test('1분 시작 간격과 동시 조회 상한을 지키고 숨긴 동안 갱신하지 않는다', async ({ page }) => {
  await fixture(page, { hold: true });
  await expect.poll(() => count(page)).toBe(1);
  await expect(page.locator('#weekly-usage-refresh')).toBeDisabled();
  await page.evaluate(() => { document.querySelector<HTMLButtonElement>('#weekly-usage-refresh')!.click(); });
  await page.clock.runFor(600_000);
  expect(await count(page)).toBe(1);
  await page.evaluate(() => { const f = (window as any).usageFixture; f.visibility(true); f.release(); });
  await page.clock.runFor(600_000);
  expect(await count(page)).toBe(1);
  await expect(page.locator('#weekly-usage-value')).toHaveText('—');
  await page.evaluate(() => { const f = (window as any).usageFixture; f.hold = false; f.visibility(false); });
  await expect(page.locator('#weekly-usage-value')).toHaveText('42.7%');
  expect(await count(page)).toBe(2);
  await page.clock.runFor(59_999);
  expect(await count(page)).toBe(2);
  await page.clock.runFor(1);
  await expect.poll(() => count(page)).toBe(3);
  const observed = await page.evaluate(() => { const f = (window as any).usageFixture; return { maxActive: f.maxActive, calls: f.calls }; });
  expect(observed.maxActive).toBe(1);
  for (let i = 1; i < observed.calls.length; i++) expect(observed.calls[i].time - observed.calls[i - 1].time).toBeGreaterThanOrEqual(60_000);
});

test('숨김에서 돌아와도 이전 응답은 무효이며 새 CLI 선택을 중복 없이 즉시 조회한다', async ({ page }) => {
  await fixture(page, { hold: true });
  await expect.poll(() => count(page)).toBe(1);
  await page.evaluate(() => {
    const f = (window as any).usageFixture;
    f.visibility(true); f.visibility(false);
    f.response = { ...f.response, executable: 'D:\\다른 도구\\codex.exe', weekly: { usedPercent: 91, resetsAt: null } };
    f.widget.refresh(); f.release(); f.hold = false;
  });
  await expect(page.locator('#weekly-usage')).toHaveAttribute('aria-busy', 'false');
  expect(await count(page)).toBe(1);
  await page.clock.runFor(1);
  await expect(page.locator('#weekly-usage-value')).toHaveText('91%');
  await expect(page.locator('#weekly-usage')).toHaveAttribute('title', '기본 계정');
});

test('종료는 진행 조회를 정산하고 늦은 응답을 버리며 종료 취소 뒤에만 재개한다', async ({ page }) => {
  await fixture(page, { hold: true });
  await expect.poll(() => count(page)).toBe(1);
  await page.evaluate(() => {
    const f = (window as any).usageFixture; f.drained = false;
    f.widget.beginExit().then(() => { f.drained = true; });
  });
  await page.clock.runFor(600_000);
  expect(await page.evaluate(() => (window as any).usageFixture.drained)).toBe(false);
  expect(await count(page)).toBe(1);
  await page.evaluate(() => (window as any).usageFixture.release());
  await expect.poll(() => page.evaluate(() => (window as any).usageFixture.drained)).toBe(true);
  await expect(page.locator('#weekly-usage-value')).toHaveText('—');
  await page.clock.runFor(600_000);
  expect(await count(page)).toBe(1);
  await page.evaluate(() => { const f = (window as any).usageFixture; f.hold = false; f.widget.cancelExit(); });
  await expect(page.locator('#weekly-usage-value')).toHaveText('42.7%');
  expect(await count(page)).toBe(2);
});

test('지난 초기화 시각의 수치는 갱신 대기로 표시하고 잘못된 응답을 거부한다', async ({ page }) => {
  await fixture(page, { hold: true });
  await expect.poll(() => count(page)).toBe(1);
  await page.evaluate(() => { const f = (window as any).usageFixture; f.release(); f.hold = false; f.response.weekly = { usedPercent: 88, resetsAt: Math.floor(Date.now() / 1000) + 90 }; });
  await expect(page.locator('#weekly-usage-value')).toHaveText('42.7%');
  await page.clock.runFor(60_000);
  await expect(page.locator('#weekly-usage-value')).toHaveText('88%');
  await page.clock.runFor(30_000);
  await expect(page.locator('#weekly-usage-status')).toHaveText('초기화 시각 경과 · 갱신 대기');
  await expect(page.locator('#weekly-usage-value')).toHaveText('88%');
  expect(await count(page)).toBe(2);
  await page.evaluate(() => { (window as any).usageFixture.response.weekly.usedPercent = 200; });
  await page.clock.runFor(30_000);
  await expect(page.locator('#weekly-usage-status')).toHaveText(lastKnownFailure);
  await expect(page.locator('#weekly-usage-value')).toHaveText('88%');
  expect(await count(page)).toBe(3);
  await page.clock.runFor(59_999);
  expect(await count(page)).toBe(3);
});

test('최초 숨김은 조회를 시작하지 않고 표시할 때 처음 조회한다', async ({ page }) => {
  await fixture(page, { hidden: true });
  await page.clock.runFor(600_000);
  expect(await count(page)).toBe(0);
  await page.evaluate(() => (window as any).usageFixture.visibility(false));
  await expect(page.locator('#weekly-usage-value')).toHaveText('42.7%');
  expect(await count(page)).toBe(1);
});

test('운영체제 숨김 신호는 문서가 보여도 폴링을 멈추고 늦은 결과를 폐기한다', async ({ page }) => {
  await fixture(page, { hold: true });
  await expect.poll(() => count(page)).toBe(1);
  await page.evaluate(() => { const f = (window as any).usageFixture; f.widget.setVisible(false); f.release(); f.hold = false; });
  await expect(page.locator('#weekly-usage')).toHaveAttribute('aria-busy', 'false');
  expect(await page.evaluate(() => document.hidden)).toBe(false);
  await expect(page.locator('#weekly-usage-value')).toHaveText('—');
  await page.clock.runFor(59_999);
  await page.evaluate(() => (window as any).usageFixture.widget.setVisible(true));
  expect(await count(page)).toBe(1);
  await page.clock.runFor(1);
  await expect(page.locator('#weekly-usage-value')).toHaveText('42.7%');
  expect(await count(page)).toBe(2);
  await page.evaluate(() => (window as any).usageFixture.widget.setVisible(false));
  await page.clock.runFor(600_000);
  expect(await count(page)).toBe(2);
  await page.evaluate(() => (window as any).usageFixture.widget.setVisible(true));
  await expect.poll(() => count(page)).toBe(3);
  await page.evaluate(() => (window as any).usageFixture.widget.setVisible(true));
  expect(await count(page)).toBe(3);
});

test('수동 새로고침은 1분을 기다리지 않고 조회하며 5초 제한과 진행 중 중복 방지를 지킨다', async ({ page }) => {
  await fixture(page);
  await expect(page.locator('#weekly-usage-value')).toHaveText('42.7%');
  await page.evaluate(() => { const f = (window as any).usageFixture; f.hold = true; f.response.weekly.usedPercent = 55; });
  await page.locator('#weekly-usage-refresh').click();
  await expect.poll(() => count(page)).toBe(2);
  await expect(page.locator('#weekly-usage-refresh')).toBeDisabled();
  await page.evaluate(() => { for (let i = 0; i < 20; i++) document.querySelector<HTMLButtonElement>('#weekly-usage-refresh')!.click(); });
  await page.clock.runFor(4_999);
  expect(await count(page)).toBe(2);
  await page.evaluate(() => { const f = (window as any).usageFixture; f.release(); f.hold = false; });
  await expect(page.locator('#weekly-usage-value')).toHaveText('55%');
  await expect(page.locator('#weekly-usage-refresh')).toBeDisabled();
  await page.clock.runFor(1);
  await expect(page.locator('#weekly-usage-refresh')).toBeEnabled();
  await page.locator('#weekly-usage-refresh').click();
  await expect.poll(() => count(page)).toBe(3);
  const observed = await page.evaluate(() => { const f = (window as any).usageFixture; return { maxActive: f.maxActive, calls: f.calls }; });
  expect(observed.maxActive).toBe(1);
  expect(observed.calls.map(call => call.manual)).toEqual([false, true, true]);
  expect(observed.calls[2].time - observed.calls[1].time).toBe(5_000);
  await page.clock.runFor(59_999);
  expect(await count(page)).toBe(3);
  await page.clock.runFor(1);
  await expect.poll(() => count(page)).toBe(4);
});

test('조회 실패는 마지막 정상값과 오래됨을 보존하며 기술 오류와 경로를 화면에 노출하지 않는다', async ({ page }) => {
  await fixture(page);
  await expect(page.locator('#weekly-usage-value')).toHaveText('42.7%');
  await page.evaluate(() => { (window as any).usageFixture.error = '<img src=x onerror=alert(1)> 다른 세션 조회가 진행 중입니다.'; });
  await page.clock.runFor(60_000);
  await expect(page.locator('#weekly-usage-status')).toHaveText(lastKnownFailure);
  await expect(page.locator('#weekly-usage-value')).toHaveText('42.7%');
  await expect(page.locator('#weekly-usage-meter')).toHaveAttribute('value', '42.7');
  await expect(page.locator('#weekly-usage-observed')).toContainText('1분 전');
  await expect(page.locator('#weekly-usage')).toHaveClass('usage-stale');
  const displayed = await page.locator('#weekly-usage').evaluate(element => ({ text: element.textContent, tooltips: [...element.querySelectorAll('[title]'), element].map(node => node.getAttribute('title')).join('\n') }));
  expect(displayed.text).not.toContain('onerror');
  expect(displayed.text).not.toContain('다른 세션 조회');
  expect(displayed.tooltips).not.toContain('onerror');
  expect(displayed.tooltips).not.toContain('다른 세션 조회');
  expect(displayed.tooltips).not.toContain('codex.exe');
  expect(await page.locator('#weekly-usage details, #weekly-usage img').count()).toBe(0);
  await page.clock.runFor(120_000);
  expect(await count(page)).toBe(4);
  await expect(page.locator('#weekly-usage-observed')).toContainText('3분 전');
  await page.evaluate(() => { const f = (window as any).usageFixture; f.error = ''; f.response.observedAt = Date.now(); f.response.weekly.usedPercent = 48; });
  await page.locator('#weekly-usage-refresh').click();
  await expect(page.locator('#weekly-usage-value')).toHaveText('48%');
  await expect(page.locator('#weekly-usage-observed')).toContainText('방금');
  await expect(page.locator('#weekly-usage-status')).not.toHaveText(lastKnownFailure);
  await expect(page.locator('#weekly-usage')).not.toHaveClass('usage-stale');
});

test('늦게 완료된 조회도 완료 후 1분을 기다려 서버 제한보다 이른 자동 조회를 막는다', async ({ page }) => {
  await fixture(page, { hold: true });
  await expect.poll(() => count(page)).toBe(1);
  await page.clock.runFor(10_000);
  await page.evaluate(() => { const f = (window as any).usageFixture; f.release(); f.hold = false; });
  await expect(page.locator('#weekly-usage-value')).toHaveText('42.7%');
  await expect(page.locator('#weekly-usage-refresh')).toBeEnabled();
  await page.clock.runFor(59_999);
  expect(await count(page)).toBe(1);
  await page.clock.runFor(1);
  await expect.poll(() => count(page)).toBe(2);
  const calls = await page.evaluate(() => (window as any).usageFixture.calls);
  expect(calls[1].time - calls[0].time).toBe(70_000);
  expect(calls[1].manual).toBe(false);
});
