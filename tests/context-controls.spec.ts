import { expect, test, type Page } from '@playwright/test';

async function fixture(page: Page) {
  await page.goto('/');
  await page.evaluate(async () => {
    const state = window as any;
    const { createContextControls } = await import('/src/context-controls.ts');
    const host = document.createElement('div'); host.id = 'context-fixture'; host.className = 'context-controls card-actions';
    host.style.cssText = 'position:fixed;top:20px;right:20px;z-index:9999;background:#141820'; document.body.append(host);
    const statusHost = document.createElement('span'); statusHost.hidden = true; document.body.append(statusHost);
    const data = state.contextFixture = {
      reads: 0, preparations: 0, mutations: 0, notices: [] as any[], failure: false, hold: false,
      release: () => {}, holdRead: false, releaseRead: () => {}, result: 'started',
      snapshot: { revision: 1, connected: true, guiConnected: true, tokenUsage: { total: { totalTokens: 900000 }, last: { totalTokens: 40000 }, modelContextWindow: 100000 }, tokenUsageFresh: true, compacting: false, active: false, canCompact: true } as any,
    };
    state.contextControls = createContextControls({
      host, statusHost, sessionId: 'context-session', generation: 9,
      async call(command: string, args: any) {
        if (command !== 'conversation_request' || args.id !== 'context-session' || args.generation !== 9 || args.request.type !== 'contextStatus') throw Error('관측 이외의 호출');
        data.reads++; const snapshot = structuredClone(data.snapshot);
        if (data.holdRead) { data.holdRead = false; await new Promise<void>(resolve => { data.releaseRead = resolve; }); }
        if (data.failure) throw Error('PRIVATE_TOKEN_PATH_NEVER_REAL');
        return snapshot;
      },
      async prepare() { data.preparations++; data.snapshot.guiConnected = true; data.snapshot.canCompact = true; delete data.snapshot.reason; },
      async compact() {
        data.mutations++;
        if (data.hold) await new Promise<void>(resolve => { data.release = resolve; });
        if (data.failure) throw Error('PRIVATE_TOKEN_PATH_NEVER_REAL');
        if (data.result !== 'started') return { status: data.result };
        data.snapshot.compacting = true; data.snapshot.canCompact = false; data.snapshot.tokenUsageFresh = false; data.snapshot.reason = 'compacting';
        return { status: 'started' };
      },
      notify(ko: string, error: boolean, en: string) { data.notices.push({ ko, error, en }); },
    });
    state.contextControls.setVisible(true);
  });
}

test('컨텍스트는 누적 사용량이 아닌 최근 사용량 비율을 표시하고 누락값을 0으로 표시하지 않는다', async ({ page }) => {
  await fixture(page);
  const usage = page.locator('#context-fixture .context-usage');
  await expect(usage).toHaveText('40%');
  await expect(usage).toHaveAttribute('title', /40,000 \/ 100,000/);
  await page.evaluate(() => { (window as any).contextFixture.snapshot.tokenUsage.modelContextWindow = null; });
  await usage.click();
  await expect(usage).toHaveText('—');
  await expect(usage).toHaveAttribute('aria-label', /미확인/);
});

test('0과 상한의 유효 비율을 표시하고 잘못된 수치·용량을 미확인으로 표시한다', async ({ page }) => {
  await fixture(page);
  const usage = page.locator('#context-fixture .context-usage');
  const cases: [unknown, unknown, string][] = [
    [0, 100000, '0%'], [100000, 100000, '100%'], [120000, 100000, '100%'],
    [56000, 100000, '56%'], [12000, 12000, '100%'], [-1, 100000, '—'],
    [0.5, 100000, '—'], [Number.MAX_SAFE_INTEGER + 1, 100000, '—'],
    [40000, 0, '—'], [40000, -1, '—'], [40000, 0.5, '—'],
    [40000, Number.MAX_SAFE_INTEGER + 1, '—'], [40000, null, '—'], ['40000', 100000, '—'],
  ];
  for (const [used, capacity, expected] of cases) {
    await page.evaluate(({ used, capacity }) => { const s = (window as any).contextFixture.snapshot; s.revision++; s.tokenUsage.last.totalTokens = used; s.tokenUsage.modelContextWindow = capacity; }, { used, capacity });
    await usage.click();
    await expect(usage).toHaveText(expected);
  }
});

test('늦게 돌아온 낮은 상태 판본이 더 최근 관측을 되돌리지 않는다', async ({ page }) => {
  await fixture(page);
  const usage = page.locator('#context-fixture .context-usage');
  await expect(usage).toHaveText('40%');
  await page.evaluate(() => { (window as any).contextFixture.holdRead = true; });
  await usage.click();
  await page.evaluate(() => { const s = (window as any).contextFixture; s.snapshot.revision = 2; s.snapshot.tokenUsage.last.totalTokens = 10000; s.result = 'skippedBusy'; });
  await page.locator('#context-fixture .context-compact').click();
  await expect(usage).toHaveText('10%');
  await page.evaluate(() => (window as any).contextFixture.releaseRead());
  await expect(usage).toBeEnabled();
  await expect(usage).toHaveText('10%');
});

test('연결 단절과 압축 뒤에는 과거 수치를 현재 사용량으로 표시하지 않는다', async ({ page }) => {
  await fixture(page);
  const usage = page.locator('#context-fixture .context-usage');
  await expect(usage).toHaveText('40%');
  await page.evaluate(() => { (window as any).contextFixture.snapshot.tokenUsageFresh = false; });
  await usage.click();
  await expect(usage).toHaveText('—');
  await expect(usage).toHaveAttribute('title', /마지막 확인값/);
  await page.evaluate(() => { (window as any).contextFixture.failure = true; });
  await usage.click();
  await expect(page.locator('#context-fixture .context-compact')).toBeDisabled();
  expect(await page.locator('#context-fixture').evaluate(node => node.outerHTML)).not.toContain('PRIVATE_TOKEN_PATH_NEVER_REAL');
});

test('지원하지 않는 실행 환경과 활성 작업에서는 압축을 전송하지 않는다', async ({ page }) => {
  await fixture(page);
  const compact = page.locator('#context-fixture .context-compact');
  for (const reason of ['unsupported', 'active', 'unknownResult', 'pendingRequest', 'statusUnknown', 'closing', 'deleted']) {
    await page.evaluate(reason => { Object.assign((window as any).contextFixture.snapshot, { canCompact: false, reason }); }, reason);
    await page.locator('#context-fixture .context-usage').click();
    await expect(compact).toBeDisabled();
    await compact.evaluate((button: HTMLButtonElement) => button.click());
  }
  expect(await page.evaluate(() => (window as any).contextFixture.mutations)).toBe(0);
  expect(await page.evaluate(() => (window as any).contextFixture.preparations)).toBe(0);
});

test('원래 세션 연결 후에만 압축하고 중복 클릭과 완료 선표시를 막는다', async ({ page }) => {
  await fixture(page);
  const compact = page.locator('#context-fixture .context-compact');
  await expect(compact).toBeEnabled();
  await page.evaluate(() => { const d = (window as any).contextFixture; d.snapshot.guiConnected = false; d.snapshot.canCompact = false; d.snapshot.reason = 'guiDisconnected'; d.hold = true; });
  await page.locator('#context-fixture .context-usage').click();
  await expect(compact).toBeEnabled();
  await compact.click();
  await expect(compact).toBeDisabled();
  await compact.evaluate((button: HTMLButtonElement) => button.click());
  expect(await page.evaluate(() => (window as any).contextFixture.mutations)).toBe(1);
  expect(await page.evaluate(() => (window as any).contextFixture.preparations)).toBe(1);
  await page.evaluate(() => (window as any).contextFixture.release());
  await expect(compact).toHaveAttribute('aria-busy', 'true');
  await expect(page.locator('#context-fixture .context-usage')).toHaveText('—');
  expect(await page.evaluate(() => JSON.stringify((window as any).contextFixture.notices))).not.toContain('압축했습니다');
});

test('숨김과 폐기 후 조회를 멈추고 늦은 응답을 화면에 적용하지 않는다', async ({ page }) => {
  await fixture(page);
  await expect(page.locator('#context-fixture .context-usage')).toHaveText('40%');
  await page.evaluate(() => { const s = window as any; s.contextFixture.holdRead = true; s.contextFixture.snapshot.tokenUsage.last.totalTokens = 90000; });
  await page.locator('#context-fixture .context-usage').click();
  await page.evaluate(() => { const s = window as any; s.contextControls.setVisible(false); s.contextFixture.releaseRead(); });
  await expect(page.locator('#context-fixture .context-usage')).toHaveText('40%');
  await expect(page.locator('#context-fixture .context-usage')).toBeDisabled();
  const count = await page.evaluate(() => (window as any).contextFixture.reads);
  await page.waitForTimeout(2200);
  expect(await page.evaluate(() => (window as any).contextFixture.reads)).toBe(count);
  await page.evaluate(() => (window as any).contextControls.dispose());
  await expect(page.locator('#context-fixture')).toBeEmpty();
});

test('언어 변경 때 아이콘 설명과 차단 사유를 바꾸고 숫자는 보존한다', async ({ page }) => {
  await fixture(page);
  await page.evaluate(async () => { const { setLanguage } = await import('/src/i18n.ts'); setLanguage('en'); });
  const usage = page.locator('#context-fixture .context-usage');
  await expect(usage).toHaveText('40%');
  await expect(usage).toHaveAttribute('aria-label', /Context 40%/);
  await expect(page.locator('#context-fixture .context-compact')).toHaveAttribute('aria-label', 'Compact context');
});
