import { test, expect } from '@playwright/test';

test('design demo preserves terminals through layout, reorder, keyboard selection and resize', async ({ page }, testInfo) => {
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.goto('/?demo=1');
  await expect(page.getByText('디자인 데모 · 아래 터미널은 정적인 예시입니다.', { exact: false })).toBeVisible();
  const cards = page.locator('.terminal-card');
  await expect(cards).toHaveCount(4);
  await expect(page.locator('.xterm-screen')).toHaveCount(4);
  await expect(page.locator('.broadcast-panel, [id^=broadcast-], #clear-selection, #selected-count, .select-session, .terminal-header input[type=checkbox]')).toHaveCount(0);
  await page.keyboard.press('Alt+2');
  await expect(cards.nth(1)).toHaveClass(/focused/);
  await cards.nth(1).getByLabel('세션 최대화').click();
  await expect(cards.nth(0)).toBeHidden();
  await page.keyboard.press('Escape');
  await expect(cards.nth(0)).toBeVisible();
  const before = await cards.nth(0).boundingBox();
  const handle = page.getByRole('separator', { name: '1번째 열 크기 조절' });
  const box = (await handle.boundingBox())!;
  await page.mouse.move(box.x + box.width / 2, box.y + 80);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 + 90, box.y + 80, { steps: 5 });
  await page.mouse.up();
  await expect.poll(async () => (await cards.nth(0).boundingBox())!.width).toBeGreaterThan(before!.width + 50);
  await cards.nth(0).getByLabel('뒤로 이동').click();
  await expect(cards.nth(0).locator('.terminal-title')).toHaveText('대시보드 UI');
  await page.getByRole('button', { name: '3열', exact: true }).click();
  await expect(cards).toHaveCount(4);
  await page.getByRole('button', { name: '2열', exact: true }).click();
  await page.screenshot({ path: testInfo.outputPath('agent-grid-demo.png'), fullPage: true });
  expect(errors).toEqual([]);
});

test('browser preview shows a real runtime diagnostic and external sessions stay read only', async ({ page }) => {
  await page.goto('/');
  await expect(page.getByText('브라우저 미리보기입니다.', { exact: false })).toBeVisible();
  await expect(page.locator('.terminal-card')).toHaveCount(0);
  await page.getByRole('button', { name: '+ 새 세션', exact: true }).click();
  await page.getByLabel('세션 이름', { exact: true }).fill('미리보기 확인');
  await page.getByLabel('프로젝트 경로', { exact: true }).fill('C:\\workspace');
  await page.getByRole('button', { name: '세션 시작 →', exact: true }).click();
  await expect(page.getByRole('alert')).toHaveText('데스크톱 앱에서 사용할 수 있는 기능입니다.');
  await page.getByRole('button', { name: '취소', exact: true }).click();
  await page.locator('#settings-open').click();
  await page.locator('#external-toggle').click();
  await expect(page.locator('#external-dialog')).toBeVisible();
  await expect(page.locator('#external-list')).toContainText('데스크톱 앱에서 사용할 수 있는 기능입니다.');
  await expect(page.locator('#external-dialog input')).toHaveCount(0);
});

for (const width of [1440, 800]) {
  test(`compact workspace toolbar keeps session actions on the top row at ${width}px`, async ({ page }, testInfo) => {
    await page.setViewportSize({ width, height: 1000 });
    await page.goto('/?demo=1');
    await expect(page.locator('.terminal-card')).toHaveCount(4);
    await expect(page.locator('.workspace-header')).toHaveCount(0);
    await expect(page.locator('.workspace-title')).toHaveCount(0);
    await expect(page.locator('.sidebar .brand')).toBeVisible();
    const topbar = (await page.locator('.topbar').boundingBox())!;
    expect(topbar.y).toBe(0);
    expect(topbar.height).toBeLessThanOrEqual(60);
    for (const selector of ['#catalog-open', '#new-session', '#notifications', '#exit']) {
      const action = page.locator(`.topbar ${selector}`);
      await expect(action).toBeVisible();
      const box = (await action.boundingBox())!;
      expect(box.y).toBeGreaterThanOrEqual(topbar.y);
      expect(box.y + box.height).toBeLessThanOrEqual(topbar.y + topbar.height);
      expect(box.x + box.width).toBeLessThanOrEqual(width);
    }
    const tabs = (await page.locator('#project-tabs').boundingBox())!;
    expect(tabs.y).toBeLessThan(80);
    expect((await page.locator('#grid-area').boundingBox())!.height).toBeGreaterThan(700);
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBe(width);
    await page.screenshot({ path: testInfo.outputPath(`compact-toolbar-${width}.png`), fullPage: true });
    await page.locator('#catalog-open').focus();
    await page.keyboard.press('Enter');
    await expect(page.locator('#catalog-dialog')).toBeVisible();
    await page.locator('#catalog-close').click();
    await page.locator('#new-session').focus();
    await page.keyboard.press('Enter');
    await expect(page.locator('#create-dialog')).toBeVisible();
  });
}
