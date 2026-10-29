import { test, expect, type Page } from '@playwright/test';

async function manager(page: Page, importedName?: string) {
  await page.route('**/project-management-harness', route => route.fulfill({ contentType: 'text/html', body: '<!doctype html><html><body></body></html>' }));
  await page.goto('/project-management-harness');
  await page.evaluate(async importedName => {
    const modulePath = '/src/project-manager.ts';
    const { createProjectManager } = await import(modulePath);
    const fixture = (window as any).projectFixture = {
      project: importedName === undefined ? undefined : { id: 'imported', name: importedName, cwd: 'D:\\가져온 폴더', createdAt: 1, favorite: false },
      calls: [] as { command: string; args: any }[], changed: [] as any[],
    };
    const widget = createProjectManager({
      async call(command: string, args: any) {
        fixture.calls.push({ command, args });
        if (command === 'create_project') fixture.project = { id: 'created', name: args.name, cwd: args.cwd, createdAt: 1, favorite: false };
        else if (command === 'rename_project') fixture.project = { ...fixture.project, name: args.name };
        else if (command === 'set_project_favorite') fixture.project = { ...fixture.project, favorite: args.favorite };
        else throw new Error(`시험에서 허용하지 않은 명령: ${command}`);
        return structuredClone(fixture.project);
      },
      projects: () => fixture.project ? [fixture.project] : [],
      async changed(project: any) { fixture.changed.push(structuredClone(project)); },
      moved() {},
    });
    widget.open(fixture.project);
  }, importedName);
}

for (const importedName of ['가'.repeat(125), ' ' + '나'.repeat(125)]) {
  test(`가져온 ${importedName.startsWith(' ') ? '앞 공백이 있는 ' : ''}긴 이름은 그대로 보존하며 즐겨찾기를 저장한다`, async ({ page }) => {
    await manager(page, importedName);
    await expect(page.locator('#project-name')).toHaveValue(importedName);
    await page.locator('#project-favorite').check();
    await page.locator('#project-save').click();
    await expect(page.locator('#project-dialog')).toBeHidden();
    expect(await page.evaluate(() => (window as any).projectFixture.calls)).toEqual([{ command: 'set_project_favorite', args: { id: 'imported', favorite: true } }]);
    expect(await page.evaluate(() => (window as any).projectFixture.changed)).toMatchObject([{ name: importedName, favorite: true }]);
  });
}

for (const mode of ['create', 'rename'] as const) {
  test(`${mode === 'create' ? '새 프로젝트' : '실제 이름 변경'}는 빈 이름·121자·제어문자를 차단하고 120자 이름을 허용한다`, async ({ page }) => {
    await manager(page, mode === 'rename' ? '가'.repeat(125) : undefined);
    for (const invalid of ['  ', '나'.repeat(121), '금지\u0001문자']) {
      await page.locator('#project-name').fill(invalid);
      await page.locator('#project-save').click();
      await expect(page.locator('#project-dialog')).toBeVisible();
      expect(await page.locator('#project-name').evaluate((input: HTMLInputElement) => input.validity.valid)).toBe(false);
      expect(await page.evaluate(() => (window as any).projectFixture.calls)).toEqual([]);
    }
    const boundary = '😀'.repeat(120);
    await page.locator('#project-name').fill(boundary);
    await page.locator('#project-save').click();
    await expect(page.locator('#project-dialog')).toBeHidden();
    expect(await page.evaluate(() => (window as any).projectFixture.calls)).toEqual([
      mode === 'create' ? { command: 'create_project', args: { name: boundary, cwd: null } } : { command: 'rename_project', args: { id: 'imported', name: boundary } },
    ]);
  });
}
