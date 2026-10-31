import { test, expect, type Page } from '@playwright/test';

const a = { title: '복원 첫째', cwd: 'D:\\work', threadId: 'thread-a', sessionId: 'old-a', resumeOnStartup: true };
const b = { title: '복원 둘째', cwd: 'D:\\work', threadId: 'thread-b', sessionId: 'old-b', resumeOnStartup: true };

async function workspace(page: Page, options: { sessions?: any[]; holdFirst?: boolean; holdSecond?: boolean; holdLayout?: boolean; holdStop?: boolean; holdError?: boolean; holdReconcile?: boolean; cancelStartOnStop?: boolean; lateSuccess?: boolean; project?: boolean; failFirst?: boolean; failActive?: boolean; live?: boolean; failSave?: boolean; failExit?: boolean } = {}) {
  await page.addInitScript(({ options, a, b }) => {
    const state = window as any; state.isTauri = true;
    let callback = 0;
    const callbacks = new Map<number, (value: any) => void>();
    const listeners = new Map<string, (value: any) => void>();
    const live = options.live ? [{ ...a, id: a.sessionId, generation: 1, state: 'idle', active: true }] : [];
    const projects = JSON.parse(localStorage.getItem('restore-test-projects') || 'null') || (options.project ? [{ id: 'p', name: '복원 프로젝트', cwd: a.cwd, createdAt: 1, favorite: false }, { id: 'q', name: '다른 프로젝트', cwd: null, createdAt: 2, favorite: false }] : []);
    const fixture = state.restoreFixture = { calls: [] as any[], live, starts: 0, current: 0, maxCurrent: 0, stoppedIds: [] as string[], release: () => {}, releaseSecond: () => {}, releaseStop: () => {}, releaseError: () => {}, releaseReconcile: () => {}, releaseLayout: () => {}, failSave: !!options.failSave, failExit: !!options.failExit,
      emit(info: any) { listeners.get('session-changed')?.({ event: 'session-changed', id: 1, payload: info }); },
    };
    state.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
    state.__TAURI_INTERNALS__ = {
      transformCallback(handler: (value: any) => void) { callbacks.set(++callback, handler); return callback; }, unregisterCallback() {},
      async invoke(command: string, args: any = {}) {
        fixture.calls.push({ command, args });
        if (command === 'plugin:event|listen') { listeners.set(args.event, callbacks.get(args.handler)!); return callback; }
        if (command.startsWith('plugin:event|')) return callback;
        if (command === 'get_ui_language') return 'ko';
        if (command === 'list_projects') return projects;
        if (command === 'list_sessions') {
          if (options.holdReconcile && fixture.starts === 1) await new Promise<void>(resolve => { fixture.releaseReconcile = resolve; });
          return fixture.live;
        }
        if (command === 'load_layout') {
          if (options.holdLayout) await new Promise<void>(resolve => { fixture.releaseLayout = resolve; });
          return JSON.parse(localStorage.getItem('restore-test-layout') || 'null') || { version: 1, columns: 3, columnSizes: [2, 1, 1], rowSizes: [1], sessions: options.sessions ?? [a, b] };
        }
        if (command === 'save_layout') {
          if (fixture.failSave) throw new Error('SAVE_FAILED');
          localStorage.setItem('restore-test-layout', JSON.stringify(args.layout)); return null;
        }
        if (command === 'start_session') {
          const index = ++fixture.starts; fixture.current++; fixture.maxCurrent = Math.max(fixture.maxCurrent, fixture.current);
          const info = { ...args.request, id: `new-${index}`, generation: 1, state: 'starting', active: true };
          fixture.live.push(info); fixture.emit(info);
          if (options.holdFirst && index === 1) await new Promise<void>(resolve => { fixture.release = resolve; });
          if (options.holdSecond && index === 2) await new Promise<void>(resolve => { fixture.releaseSecond = resolve; });
          fixture.current--;
          if (options.cancelStartOnStop && fixture.stoppedIds.includes(info.id) && !options.lateSuccess) { Object.assign(info, { state: 'stopped', active: false }); fixture.emit(info); throw new Error('START_CANCELLED'); }
          if (options.failFirst && index === 1) {
            Object.assign(info, { state: 'error', active: !!options.failActive }); fixture.emit(info);
            if (options.holdError) await new Promise<void>(resolve => { fixture.releaseError = resolve; });
            throw new Error('FIRST_RESTORE_FAILED');
          }
          Object.assign(info, { state: 'idle' }); fixture.emit(info); return { ...info };
        }
        if (command === 'session_stop') {
          fixture.stoppedIds.push(args.id);
          if (options.holdStop) await new Promise<void>(resolve => { fixture.releaseStop = resolve; });
          const info = fixture.live.find((item: any) => item.id === args.id); Object.assign(info, { state: 'stopped', active: false }); fixture.emit(info);
        }
        if (command === 'delete_project') {
          projects.splice(projects.findIndex((item: any) => item.id === args.id), 1); localStorage.setItem('restore-test-projects', JSON.stringify(projects));
          for (const info of fixture.live) if (info.projectId === args.id) Object.assign(info, { projectId: null, projectAssigned: true, projectRevision: (info.projectRevision || 0) + 1 });
          return null;
        }
        if (command === 'move_session_to_project') {
          const info = fixture.live.find((item: any) => item.id === args.id); Object.assign(info, { projectId: args.projectId, projectAssigned: true, projectRevision: (info.projectRevision || 0) + 1 }); return { ...info };
        }
        if (command === 'complete_exit') {
          for (const info of fixture.live) { Object.assign(info, { state: 'stopped', active: false }); fixture.emit(info); }
          if (fixture.failExit) throw new Error('REMOTE_STOP_UNKNOWN');
        }
        if (command === 'unconfirmed_exit_available') return false;
        return null;
      },
    };
  }, { options, a, b });
  await page.goto('/');
}

const calls = (page: Page, command: string) => page.evaluate(command => (window as any).restoreFixture.calls.filter((item: any) => item.command === command), command);
const saved = (page: Page) => page.evaluate(() => JSON.parse(localStorage.getItem('restore-test-layout') || 'null'));

test('저장된 실행 의도가 있는 원래 대화만 순차 복원하고 전체 목록을 보존한다', async ({ page }) => {
  await workspace(page, { holdFirst: true, sessions: [a, b, { title: 'ID 없음', cwd: a.cwd, resumeOnStartup: true }, { title: '종료됨', cwd: a.cwd, threadId: 'stopped', resumeOnStartup: false }] });
  await expect.poll(async () => (await calls(page, 'start_session')).length).toBe(1);
  await page.waitForTimeout(600); expect(await calls(page, 'save_layout')).toEqual([]);
  await page.evaluate(() => (window as any).restoreFixture.release());
  await expect(page.locator('.terminal-title')).toHaveText([a.title, b.title]);
  await expect.poll(async () => (await saved(page))?.sessions.length).toBe(4);
  expect((await calls(page, 'start_session')).map((item: any) => item.args.request.threadId)).toEqual(['thread-a', 'thread-b']);
  expect(await page.evaluate(() => (window as any).restoreFixture.maxCurrent)).toBe(1);
  expect(await calls(page, 'session_write')).toEqual([]);
  await page.locator('#restore-open').click();
  await expect(page.locator('#restore-list .restore-item strong')).toHaveText(['ID 없음', '종료됨']);
});

test('일부 복원 실패는 저장 목록과 순서를 보존하며 명시 재시도와 목록 제거를 반영한다', async ({ page }) => {
  await workspace(page, { failFirst: true });
  await expect.poll(async () => (await calls(page, 'start_session')).length).toBe(2);
  await expect.poll(async () => (await saved(page))?.sessions.map((item: any) => item.title)).toEqual([a.title, b.title]);
  await page.locator('#restore-open').click();
  await expect(page.locator('#restore-list .restore-item strong')).toHaveText([a.title]);
  await page.locator('#restore-list').getByRole('button', { name: '이 대화 재개 →' }).click();
  await expect.poll(async () => (await calls(page, 'start_session')).length).toBe(3);
  await expect(page.locator('.terminal-title')).toHaveText([a.title, b.title]);
  await expect(page.locator('#restore-banner')).toBeHidden();
  expect((await saved(page)).sessions.map((item: any) => item.title)).toEqual([a.title, b.title]);
});

test('복원 도중 종료는 추가 시작을 막고 중단 이벤트 이후에도 다음 실행 의도를 보존한다', async ({ page }) => {
  await workspace(page, { holdFirst: true, failExit: true });
  await expect.poll(async () => (await calls(page, 'start_session')).length).toBe(1);
  await page.locator('#exit').click(); await page.locator('#confirm-exit').click();
  expect(await calls(page, 'complete_exit')).toEqual([]);
  await page.evaluate(() => (window as any).restoreFixture.release());
  await expect.poll(async () => (await calls(page, 'complete_exit')).length).toBe(1);
  await expect(page.locator('#confirm-exit')).toBeEnabled();
  await page.waitForTimeout(600);
  expect((await calls(page, 'start_session')).length).toBe(1);
  expect((await saved(page)).sessions.map((item: any) => item.resumeOnStartup)).toEqual([true, true]);
  await page.locator('#confirm-exit').click();
  await expect.poll(async () => (await calls(page, 'complete_exit')).length).toBe(2);
  expect((await saved(page)).sessions.map((item: any) => item.resumeOnStartup)).toEqual([true, true]);
  await page.reload();
  await expect.poll(async () => (await calls(page, 'start_session')).length).toBe(1);
  await page.evaluate(() => (window as any).restoreFixture.release());
  await expect(page.locator('.terminal-title')).toHaveText([a.title, b.title]);
});

test('이미 실행 중인 대화는 재사용하고 구형 저장 항목은 자동 시작하지 않는다', async ({ page }) => {
  await workspace(page, { live: true, sessions: [a, { title: '이전 버전', cwd: a.cwd, threadId: 'legacy' }] });
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await expect(page.locator('.terminal-title')).toHaveText([a.title]);
  expect(await calls(page, 'start_session')).toEqual([]);
  await page.locator('#restore-open').click();
  await expect(page.locator('#restore-list .restore-item strong')).toHaveText(['이전 버전']);
  await page.locator('#restore-list').getByRole('button', { name: '목록에서 제거' }).click();
  await expect(page.locator('#restore-banner')).toBeHidden();
  await expect.poll(async () => (await saved(page))?.sessions.length).toBe(1);
});

test('저장 실패는 실제 종료 호출을 막고 재시도할 때 전체 복원 정보를 저장한다', async ({ page }) => {
  await workspace(page, { failSave: true });
  await expect(page.locator('.terminal-title')).toHaveText([a.title, b.title]);
  await page.locator('#exit').click(); await page.locator('#confirm-exit').click();
  await expect(page.locator('.toast.error').last()).toContainText('SAVE_FAILED');
  expect(await calls(page, 'complete_exit')).toEqual([]);
  await page.evaluate(() => { (window as any).restoreFixture.failSave = false; });
  await page.locator('#confirm-exit').click();
  await expect.poll(async () => (await calls(page, 'complete_exit')).length).toBe(1);
  expect((await saved(page)).sessions.map((item: any) => item.threadId)).toEqual(['thread-a', 'thread-b']);
});

test('초기 조회 중 빠르게 종료해도 저장 목록을 빈 화면으로 덮거나 대화를 시작하지 않는다', async ({ page }) => {
  await workspace(page, { holdLayout: true });
  await expect.poll(async () => (await calls(page, 'load_layout')).length).toBe(1);
  await page.locator('#column-options [data-columns="1"]').click();
  await page.locator('#exit').click(); await page.locator('#confirm-exit').click();
  await page.waitForTimeout(600);
  expect(await calls(page, 'save_layout')).toEqual([]); expect(await calls(page, 'complete_exit')).toEqual([]);
  await page.evaluate(() => (window as any).restoreFixture.releaseLayout());
  await expect.poll(async () => (await calls(page, 'complete_exit')).length).toBe(1);
  expect((await saved(page)).sessions.map((item: any) => item.threadId)).toEqual(['thread-a', 'thread-b']);
  expect(await calls(page, 'start_session')).toEqual([]);
});

test('사용자가 개별 종료하고 닫은 세션은 다음 실행에서 부활하지 않는다', async ({ page }) => {
  await workspace(page); await expect(page.locator('.terminal-title')).toHaveText([a.title, b.title]);
  page.on('dialog', dialog => dialog.accept());
  await page.locator('.terminal-card').first().locator('.stop').click();
  await expect(page.locator('.terminal-card').first().locator('.session-state')).toHaveText('종료됨');
  await expect.poll(async () => (await saved(page))?.sessions[0].resumeOnStartup).toBe(false);
  await page.locator('.terminal-card').first().locator('.stop').click();
  await expect.poll(async () => (await saved(page))?.sessions.length).toBe(1);
  await page.reload();
  await expect(page.locator('.terminal-title')).toHaveText([b.title]);
  expect((await calls(page, 'start_session')).map((item: any) => item.args.request.threadId)).toEqual(['thread-b']);
});

test('미실행 저장 64개는 새 실행을 막지 않으며 복원은 실제 카드32개에서 조용히 멈춘다', async ({ page }) => {
  await workspace(page, { sessions: Array.from({ length: 64 }, (_, index) => ({ title: `저장 ${index}`, cwd: a.cwd, threadId: `saved-${index}`, resumeOnStartup: false })) });
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await page.locator('#new-session').click();
  await page.locator('#session-title').fill('새 세션'); await page.locator('#session-cwd').fill(a.cwd);
  await page.locator('#create-submit').click();
  await expect.poll(async () => (await calls(page, 'start_session')).length).toBe(1);
  await expect.poll(async () => (await saved(page))?.sessions.length).toBe(65);
  await page.evaluate(() => { const layout = JSON.parse(localStorage.getItem('restore-test-layout')!); for (const item of layout.sessions) item.resumeOnStartup = !!item.threadId; localStorage.setItem('restore-test-layout', JSON.stringify(layout)); });
  await page.reload(); await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await expect(page.locator('.terminal-card')).toHaveCount(32);
  expect((await calls(page, 'start_session')).length).toBe(32);
  expect(await page.locator('.toast.error').count()).toBe(0);
  await expect.poll(async () => (await saved(page))?.sessions.length).toBe(65);
  await page.locator('#restore-open').click(); await expect(page.locator('#restore-list .restore-item')).toHaveCount(33);
});

test('메타데이터512개 한도와 잘못된 초과 입력은 실제 실행32개와 구분해 원본을 보존한다', async ({ page }) => {
  await workspace(page, { sessions: Array.from({ length: 512 }, (_, index) => ({ title: `저장 ${index}`, cwd: a.cwd, threadId: `saved-${index}` })) });
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await expect.poll(async () => (await saved(page))?.sessions.length).toBe(512);
  await page.locator('#new-session').click(); await page.locator('#session-title').fill('넘침'); await page.locator('#session-cwd').fill(a.cwd);
  await page.locator('#create-submit').click(); await expect(page.locator('#create-error')).toContainText('메타데이터는 최대 512개');
  expect(await calls(page, 'start_session')).toEqual([]);
  await page.evaluate(() => { const layout = JSON.parse(localStorage.getItem('restore-test-layout')!); layout.sessions.push({ title: '초과 원본', cwd: 'D:\\work' }); localStorage.setItem('restore-test-layout', JSON.stringify(layout)); });
  await page.reload(); await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await expect(page.locator('.toast.error')).toContainText('원본 저장을 덮어쓰지 않았습니다');
  await page.waitForTimeout(600);
  expect(await calls(page, 'save_layout')).toEqual([]);
  expect((await saved(page)).sessions.length).toBe(513);
});

test('실패한 시작의 소유권이 남으면 재시도가 중복 실행하지 않고 회수 뒤 원래 대화를 다시 연결한다', async ({ page }) => {
  await workspace(page, { failFirst: true, failActive: true });
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await expect.poll(async () => (await saved(page))?.sessions.length).toBe(2);
  await page.locator('#restore-open').click();
  await expect(page.locator('#restore-list .restore-item strong')).toHaveText([a.title]);
  await page.locator('#restore-list').getByRole('button', { name: '이 대화 재개 →' }).click();
  await expect(page.locator('.toast.error').last()).toContainText('세션 연결이 아직 완료되지 않았습니다');
  expect((await calls(page, 'start_session')).length).toBe(2);
  await page.evaluate(() => { const f = (window as any).restoreFixture; const info = f.live.find((item: any) => item.id === 'new-1'); Object.assign(info, { active: false }); f.emit(info); });
  await page.locator('#restore-list').getByRole('button', { name: '이 대화 재개 →' }).click();
  await expect.poll(async () => (await calls(page, 'start_session')).length).toBe(3);
  await expect(page.locator('.terminal-title')).toHaveText([a.title, b.title]);
  await expect.poll(async () => (await saved(page))?.sessions.length).toBe(2);
  expect(await calls(page, 'session_stop')).toEqual([]);
});

for (const active of [true, false]) test(`실패한 복원 카드를 ${active ? '소유 중 종료' : '회수 뒤 닫기'}하면 대기 복원 항목도 제거하여 다시 부활하지 않는다`, async ({ page }) => {
  await workspace(page, { failFirst: true, failActive: true });
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  page.on('dialog', dialog => dialog.accept());
  if (!active) await page.evaluate(() => { const f = (window as any).restoreFixture; const info = f.live.find((item: any) => item.id === 'new-1'); Object.assign(info, { active: false }); f.emit(info); });
  await page.locator('.terminal-card').filter({ has: page.locator('.terminal-title', { hasText: a.title }) }).locator('.stop').click();
  await expect(page.locator('.terminal-title')).toHaveText([b.title]);
  await expect(page.locator('#restore-banner')).toBeHidden();
  await expect.poll(async () => (await saved(page))?.sessions.map((item: any) => item.threadId)).toEqual(['thread-b']);
  await page.reload(); await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  expect((await calls(page, 'start_session')).map((item: any) => item.args.request.threadId)).toEqual(['thread-b']);
});

for (const lateSuccess of [false, true]) test(`시작 중 명시 종료는 ${lateSuccess ? '늦은 성공 응답과 종료 저장' : '시작 취소와 자동 회수'} 뒤에도 재시작 의도를 되살리지 않는다`, async ({ page }) => {
  await workspace(page, { sessions: [a], holdFirst: true, cancelStartOnStop: true, holdStop: lateSuccess, lateSuccess, failExit: true });
  await expect.poll(async () => (await calls(page, 'start_session')).length).toBe(1);
  page.on('dialog', dialog => dialog.accept());
  await page.locator('.terminal-card .stop').click();
  await expect.poll(async () => (await calls(page, 'session_stop')).length).toBe(1);
  await page.locator('#exit').click(); await page.locator('#confirm-exit').click();
  expect(await calls(page, 'complete_exit')).toEqual([]);
  await page.evaluate(() => (window as any).restoreFixture.release());
  await expect.poll(async () => (await calls(page, 'complete_exit')).length).toBe(1);
  expect((await saved(page)).sessions.filter((item: any) => item.threadId === a.threadId && item.resumeOnStartup === true)).toEqual([]);
  if (lateSuccess) await page.evaluate(() => (window as any).restoreFixture.releaseStop());
  await page.reload(); await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  expect(await calls(page, 'start_session')).toEqual([]);
});

test('수동 복원 중 프로젝트 삭제는 같은 대기 항목을 갱신하고 완료와 닫기 뒤 중복 복원을 남기지 않는다', async ({ page }) => {
  await workspace(page, { sessions: [{ ...a, projectId: 'p', projectAssigned: true }], project: true, failFirst: true, holdSecond: true });
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await page.locator('#restore-open').click(); await page.locator('#restore-list').getByRole('button', { name: '이 대화 재개 →' }).click();
  await expect.poll(async () => (await calls(page, 'start_session')).length).toBe(2);
  await page.locator('#restore-dialog [data-close]').click();
  await page.locator('#projects [data-project-id="p"]').click(); await page.locator('#project-delete').click(); await page.locator('#project-delete-confirm').click();
  await expect(page.locator('#project-dialog')).toBeHidden();
  await page.evaluate(() => (window as any).restoreFixture.releaseSecond());
  await expect.poll(async () => (await saved(page))?.sessions.length).toBe(1);
  expect((await saved(page)).sessions[0]).toMatchObject({ threadId: a.threadId, projectId: null, projectAssigned: true });
  await expect(page.locator('#restore-banner')).toBeHidden();
  page.on('dialog', dialog => dialog.accept());
  await page.locator('.terminal-card .stop').click(); await expect(page.locator('.session-state')).toHaveText('종료됨');
  await page.locator('.terminal-card .stop').click();
  await expect.poll(async () => (await saved(page))?.sessions.length).toBe(0);
  await page.reload(); await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false'); expect(await calls(page, 'start_session')).toEqual([]);
});

for (const projectRevision of [0, 7]) test(`실패 소유권 카드의 프로젝트 이동은 이전 실행revision${projectRevision}과 구분하여 최신 소속을 저장한다`, async ({ page }) => {
  await workspace(page, { sessions: [{ ...a, projectId: 'p', projectAssigned: true, projectRevision }], project: true, failFirst: true, failActive: true });
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await page.locator('.terminal-card .project-move').click(); await page.locator('#project-move-target').selectOption('q'); await page.locator('#project-move-confirm').click();
  await expect(page.locator('#project-move-dialog')).toBeHidden();
  await expect.poll(async () => (await saved(page))?.sessions[0]?.projectId).toBe('q');
  expect((await saved(page)).sessions).toHaveLength(1);
  expect((await saved(page)).sessions[0]).toMatchObject({ projectAssigned: true, projectRevision: 1 });
  await page.reload(); await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  expect((await calls(page, 'start_session'))[0].args.request.projectId).toBe('q');
});

for (const reconcile of [false, true]) test(`비활성 오류 카드의 명시 닫기는 ${reconcile ? '상태 재조회 대기 중' : '시작 거절 응답 전'}에도 저장된 자동 복원을 제거한다`, async ({ page }) => {
  await workspace(page, { sessions: [a], failFirst: true, holdError: !reconcile, holdReconcile: reconcile });
  await expect(page.locator('.session-state')).toHaveText('오류');
  if (reconcile) await expect.poll(async () => (await calls(page, 'list_sessions')).length).toBe(2);
  await page.locator('.terminal-card .stop').click();
  await expect(page.locator('.terminal-card')).toHaveCount(0);
  await page.evaluate(reconcile => { const fixture = (window as any).restoreFixture; if (reconcile) fixture.releaseReconcile(); else fixture.releaseError(); }, reconcile);
  await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  await expect.poll(async () => (await saved(page))?.sessions.length).toBe(0);
  await expect(page.locator('#restore-banner')).toBeHidden();
  await page.reload(); await expect(page.locator('#app')).toHaveAttribute('aria-busy', 'false');
  expect(await calls(page, 'start_session')).toEqual([]);
});
