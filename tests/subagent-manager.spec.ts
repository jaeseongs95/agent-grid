import { expect, test, type Page } from '@playwright/test';

/**
 * 실제 createSubagentManager/DOM과 대체 subagents_request 경계를 검사한다.
 * 세션·세대 격리, 직접 부모 필터, 한 요청, 결과 미확인 정합화, 종료 확인,
 * 페이지 상한과 숨김/종료 뒤 타이머 해제를 검증한다.
 * 실제 CLI의 소유권 및 원자적 expectedTurnId 처리는 이 대체 시험의 검증 범위 밖이다.
 * 통합 담당이 실행 기록을 남긴다. 작성 시점에는 미실행이다.
 */
async function fixture(page: Page, options: { unsupported?: boolean; count?: number } = {}) {
  await page.goto('/');
  await page.clock.install({ time: new Date('2030-01-02T03:04:05Z') });
  await page.evaluate(async ({ unsupported, count }) => {
    const state = window as any;
    const { createSubagentManager } = await import('/src/subagent-manager.ts');
    state.subagentManager?.dispose(); document.querySelector('#subagents-dialog')?.remove();
    const data = state.subagentFixture = {
      session: { id: 'session-a', generation: 9, title: '에이전트 세션 A', active: true, threadId: 'thread-a' },
      calls: [] as any[], notices: [] as any[], completed: 0, reply: 'applied',
      holdRead: false, holdMutation: false, failRead: false, failMutation: false,
      releaseRead: () => {}, releaseMutation: () => {},
      snapshot: {
        supported: !unsupported, connected: !unsupported,
        ...(!unsupported ? { parentRuntimeId: '11111111-1111-4111-8111-111111111111' } : {}),
        agents: Array.from({ length: count ?? 1 }, (_, index) => ({
          id: `agent-${index + 1}`, parentThreadId: 'thread-a', nickname: `조사 담당 ${index + 1}`, role: '조사',
          runtimeId: `22222222-2222-4222-8222-${String(index + 1).padStart(12, '0')}`,
          status: 'running', model: 'configured-model', effort: 'configured-effort', activeTurnId: `turn-${index + 1}`,
          capabilities: { send: true, interrupt: true, close: true, resume: true },
        })),
      } as any,
    };
    state.subagentManager = createSubagentManager({
      async call(command: string, args: any) {
        if (command !== 'subagents_request') throw Error('허용 범위 밖 호출');
        data.calls.push(structuredClone({ command, ...args }));
        const reading = args.request.type === 'list';
        const result = reading ? structuredClone(data.snapshot) : { status: data.reply };
        const failure = reading ? data.failRead : data.failMutation;
        try {
          if (reading && data.holdRead) { data.holdRead = false; await new Promise<void>(resolve => { data.releaseRead = resolve; }); }
          if (!reading && data.holdMutation) { data.holdMutation = false; await new Promise<void>(resolve => { data.releaseMutation = resolve; }); }
          if (failure) throw Error('PRIVATE_TOKEN_PATH_NEVER_REAL');
          return result;
        } finally { data.completed++; }
      },
      notify(ko: string, error: boolean, en: string) { data.notices.push({ ko, error, en }); },
    });
    await state.subagentManager.open(data.session);
  }, options);
}

async function refresh(page: Page) {
  await page.locator('.subagent-refresh').click();
  await expect(page.locator('#subagents-dialog')).toHaveAttribute('aria-busy', 'false');
}
async function mutations(page: Page) { return page.evaluate(() => (window as any).subagentFixture.calls.filter((call: any) => call.request.type !== 'list')); }
async function submit(page: Page) { await page.locator('.subagent-form').evaluate(form => form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))); }

test('미지원은 빈 목록과 구별하고 관리 요청을 차단한다', async ({ page }) => {
  await fixture(page, { unsupported: true });
  await expect(page.locator('.subagent-status')).toContainText('지원하지 않습니다');
  await expect(page.locator('.subagent-list')).toContainText('아직 확인하지 못했습니다');
  await expect(page.locator('.subagent-send')).toBeDisabled();
  await submit(page); expect(await mutations(page)).toEqual([]);
  await page.evaluate(() => Object.assign((window as any).subagentFixture.snapshot, { supported: true, connected: true, parentRuntimeId: '11111111-1111-4111-8111-111111111111', agents: [] }));
  await refresh(page);
  await expect(page.locator('.subagent-list')).toHaveText('이 세션의 직접 하위 에이전트가 없습니다.');
  await expect(page.locator('.subagent-help')).toContainText('전체 대화는 불러오지 않습니다');
});

test('실제 미지원 응답의 null 실행 세대·정합화 식별자는 조회 오류로 처리하지 않는다', async ({ page }) => {
  await fixture(page);
  await page.evaluate(() => { (window as any).subagentFixture.snapshot = { supported: false, connected: false, parentRuntimeId: null, agents: [], nextCursor: null, mutationState: 'clear', reconciledRequestId: null, reason: 'unsupported' }; });
  await refresh(page);
  await expect(page.locator('.subagent-error')).toBeHidden();
  await expect(page.locator('.subagent-status')).toContainText('지원하지 않습니다');
  await expect(page.locator('.subagent-send')).toBeDisabled();
  await submit(page); expect(await mutations(page)).toEqual([]);
});

test('지원되는 실제 목록의 null 정합화 식별자를 허용하고 잘못된 값은 차단한다', async ({ page }) => {
  await fixture(page);
  await page.evaluate(() => Object.assign((window as any).subagentFixture.snapshot, { nextCursor: null, mutationState: 'clear', reconciledRequestId: null }));
  await refresh(page);
  await expect(page.locator('.subagent-error')).toBeHidden();
  await expect(page.locator('.subagent-send')).toBeEnabled();
  await expect(page.locator('.subagent-row')).toHaveCount(1);
  for (const receipt of [42, {}, '']) {
    await page.evaluate(receipt => { (window as any).subagentFixture.snapshot.reconciledRequestId = receipt; }, receipt);
    await refresh(page); await expect(page.locator('.subagent-error')).toBeVisible(); await expect(page.locator('.subagent-send')).toBeDisabled();
  }
  expect(await mutations(page)).toEqual([]);
});

test('다른 관리 요청 결과가 미확인이면 전송하지 않고 명시적 clear 목록 뒤에만 허용한다', async ({ page }) => {
  await fixture(page); await page.locator('.subagent-prompt').fill('확인 후 한 번 전송');
  await page.evaluate(() => { (window as any).subagentFixture.snapshot.mutationState = 'unknown'; });
  await refresh(page);
  await expect(page.locator('.subagent-unknown')).toBeHidden();
  await expect(page.locator('.subagent-remote-unknown')).toContainText('다른 관리 요청');
  await expect(page.locator('.subagent-remote-unknown')).toBeVisible();
  for (const selector of ['.subagent-send', '.subagent-interrupt', '.subagent-resume', '.subagent-end']) {
    await expect(page.locator(selector)).toBeDisabled();
    await page.locator(selector).evaluate(button => button.dispatchEvent(new MouseEvent('click', { bubbles: true })));
  }
  await submit(page); expect(await mutations(page)).toEqual([]);
  await page.evaluate(() => { delete (window as any).subagentFixture.snapshot.mutationState; });
  await refresh(page); await expect(page.locator('.subagent-send')).toBeDisabled();
  await expect(page.locator('.subagent-remote-unknown')).toBeVisible();
  await submit(page); expect(await mutations(page)).toEqual([]);
  await page.evaluate(() => { (window as any).subagentFixture.snapshot.mutationState = 'clear'; });
  await refresh(page); await expect(page.locator('.subagent-remote-unknown')).toBeHidden();
  await expect(page.locator('.subagent-send')).toBeEnabled(); await page.locator('.subagent-send').click();
  await expect.poll(async () => (await mutations(page)).length).toBe(1);
  await expect(page.locator('.subagent-unknown')).toBeHidden();
});

test('원격 미확인 안내는 미지원·연결 실패·세션 전환·닫힘에서 정리한다', async ({ page }) => {
  await fixture(page);
  for (const mode of ['unsupported', 'disconnected', 'failure', 'switch', 'close']) {
    await page.evaluate(() => { const data = (window as any).subagentFixture; data.failRead = false; Object.assign(data.snapshot, { supported: true, connected: true, mutationState: 'unknown' }); });
    await refresh(page); await expect(page.locator('.subagent-remote-unknown')).toBeVisible();
    if (mode === 'switch') {
      await page.evaluate(async () => { const state = window as any; const data = state.subagentFixture; data.session = { ...data.session, id: 'session-b', generation: data.session.generation + 1 }; data.snapshot.mutationState = 'clear'; await state.subagentManager.open(data.session); });
      await expect(page.locator('.subagent-send')).toBeEnabled();
    } else if (mode === 'close') {
      await page.locator('.subagent-close').click();
    } else {
      await page.evaluate(mode => { const data = (window as any).subagentFixture; if (mode === 'unsupported') data.snapshot.supported = false; else if (mode === 'disconnected') data.snapshot.connected = false; else data.failRead = true; }, mode);
      await refresh(page); await expect(page.locator('.subagent-send')).toBeDisabled();
    }
    await expect(page.locator('.subagent-remote-unknown')).toBeHidden();
  }
  expect(await mutations(page)).toEqual([]);
});

test('직접 부모가 같은 에이전트만 표시하고 선택된 식별자로 전송한다', async ({ page }) => {
  await fixture(page, { count: 3 });
  await page.evaluate(() => {
    const agents = (window as any).subagentFixture.snapshot.agents;
    agents[0].parentThreadId = 'other-parent'; agents[0].nickname = '다른 세션 비공개 이름';
    agents[2].parentThreadId = 'agent-2';
  });
  await refresh(page);
  await expect(page.locator('.subagent-row')).toHaveCount(1);
  await expect(page.locator('.subagent-list')).not.toContainText('다른 세션 비공개 이름');
  await page.locator('.subagent-prompt').fill('확인할 내용');
  await page.locator('.subagent-send').click();
  await expect.poll(async () => (await mutations(page)).length).toBe(1);
  expect((await mutations(page))[0]).toMatchObject({ id: 'session-a', generation: 9, request: { type: 'send', agentId: 'agent-2', text: '확인할 내용', expectedParentRuntimeId: '11111111-1111-4111-8111-111111111111', expectedRuntimeId: '22222222-2222-4222-8222-000000000002' } });
  expect((await mutations(page))[0].request.requestId).toMatch(/^[0-9a-f-]{36}$/);
  await expect(page.locator('.subagent-prompt')).toHaveValue('');
  await expect(page.locator('.subagent-send')).toBeDisabled();
  await expect(page.locator('.subagent-status')).toContainText('새로고침');
});

test('한 페이지 100개와 현재 페이지 검색·다음·처음 커서를 유지한다', async ({ page }) => {
  await fixture(page, { count: 101 });
  await page.evaluate(() => { (window as any).subagentFixture.snapshot.nextCursor = 'page-two-token'; });
  await refresh(page);
  await expect(page.locator('.subagent-row')).toHaveCount(100);
  await expect(page.locator('.subagent-limit')).toBeVisible();
  await page.locator('.subagent-search').fill('조사 담당 100'); await expect(page.locator('.subagent-row')).toHaveCount(1);
  await page.locator('.subagent-search').fill('');
  await page.evaluate(() => { const data = (window as any).subagentFixture; data.snapshot.agents = [{ ...data.snapshot.agents[0], id: 'page-two-agent', nickname: '두 번째 페이지' }]; data.snapshot.nextCursor = null; });
  await page.locator('.subagent-next').click();
  await expect(page.locator('.subagent-page')).toHaveText('2쪽');
  await expect(page.locator('.subagent-row')).toHaveCount(1);
  expect(await page.evaluate(() => (window as any).subagentFixture.calls.at(-1).request)).toEqual({ type: 'list', cursor: 'page-two-token' });
  await refresh(page);
  expect(await page.evaluate(() => (window as any).subagentFixture.calls.at(-1).request.cursor)).toBe('page-two-token');
  await page.locator('.subagent-first').click();
  await expect(page.locator('.subagent-page')).toHaveText('1쪽');
  expect(await page.evaluate(() => (window as any).subagentFixture.calls.at(-1).request.cursor)).toBeNull();
});

test('현재 작업 중단은 관측한 turn을 보내고 없는 turn은 요청하지 않는다', async ({ page }) => {
  await fixture(page);
  await page.locator('.subagent-interrupt').click();
  await expect.poll(async () => (await mutations(page)).length).toBe(1);
  expect((await mutations(page))[0].request).toMatchObject({ type: 'interrupt', agentId: 'agent-1', expectedTurnId: 'turn-1' });
  await expect(page.locator('.subagent-status')).toContainText('종료 여부를 확인');
  await page.evaluate(() => { (window as any).subagentFixture.snapshot.agents[0].activeTurnId = null; });
  await refresh(page);
  await expect(page.locator('.subagent-interrupt')).toBeDisabled();
  await page.locator('.subagent-interrupt').evaluate((button: HTMLButtonElement) => button.click());
  expect((await mutations(page)).length).toBe(1);
});

test('종료는 두 단계 확인하고 취소·선택 변경은 확인을 해제한다', async ({ page }) => {
  await fixture(page, { count: 2 });
  await page.locator('.subagent-end').click();
  await expect(page.locator('.subagent-confirm')).toBeVisible(); expect(await mutations(page)).toEqual([]);
  await expect(page.locator('.subagent-confirm')).toContainText('이 에이전트와 하위 에이전트의 작업이 중단됩니다');
  await expect(page.locator('.subagent-confirm')).toContainText('저장된 대화를 삭제하지 않습니다');
  await page.locator('.subagent-confirm-cancel').click();
  await expect(page.locator('.subagent-confirm')).toBeHidden();
  await page.locator('.subagent-end').click(); await page.locator('.subagent-row[data-agent-id="agent-2"]').click();
  await expect(page.locator('.subagent-confirm')).toBeHidden(); expect(await mutations(page)).toEqual([]);
  await page.locator('.subagent-end').click(); await page.locator('.subagent-confirm-end').click();
  await expect.poll(async () => (await mutations(page)).length).toBe(1);
  expect((await mutations(page))[0].request).toMatchObject({ type: 'close', agentId: 'agent-2' });
  await expect(page.locator('.subagent-status')).toContainText('종료를 요청');
  await refresh(page); await page.locator('.subagent-resume').click();
  await expect.poll(async () => (await mutations(page)).length).toBe(2);
  expect((await mutations(page))[1].request).toMatchObject({ type: 'resume', agentId: 'agent-2' });
});

test('확정 미적용 응답은 초안을 보존하고 clear 목록 뒤 같은 세션의 새 요청을 허용한다', async ({ page }) => {
  await fixture(page);
  await page.evaluate(() => { (window as any).subagentFixture.reply = 'notApplied'; });
  await page.locator('.subagent-prompt').fill('송신 전 거절 뒤 보존할 초안');
  await page.locator('.subagent-send').click();
  await expect.poll(async () => (await mutations(page)).length).toBe(1);
  await expect(page.locator('.subagent-error')).toContainText('요청이 적용되지 않았습니다');
  await expect(page.locator('.subagent-unknown')).toBeHidden();
  await expect(page.locator('.subagent-remote-unknown')).toBeHidden();
  await expect(page.locator('.subagent-prompt')).toHaveValue('송신 전 거절 뒤 보존할 초안');
  await expect(page.locator('.subagent-send')).toBeDisabled();
  await page.evaluate(() => {
    const data = (window as any).subagentFixture;
    data.reply = 'applied'; Object.assign(data.snapshot, { mutationState: 'clear', reconciledRequestId: null });
  });
  await refresh(page);
  await expect(page.locator('.subagent-send')).toBeEnabled();
  await expect(page.locator('.subagent-prompt')).toHaveValue('송신 전 거절 뒤 보존할 초안');
  await page.locator('.subagent-send').click();
  await expect.poll(async () => (await mutations(page)).length).toBe(2);
  const calls = await mutations(page);
  expect(calls.map((call: any) => ({ id: call.id, generation: call.generation, type: call.request.type, agentId: call.request.agentId, text: call.request.text }))).toEqual([
    { id: 'session-a', generation: 9, type: 'send', agentId: 'agent-1', text: '송신 전 거절 뒤 보존할 초안' },
    { id: 'session-a', generation: 9, type: 'send', agentId: 'agent-1', text: '송신 전 거절 뒤 보존할 초안' },
  ]);
  expect(calls[1].request.requestId).not.toBe(calls[0].request.requestId);
  await expect(page.locator('.subagent-prompt')).toHaveValue('');
});

test('결과 미확인은 닫기·재열기 뒤에도 같은 요청의 정합화만 해제한다', async ({ page }) => {
  await fixture(page);
  await page.evaluate(() => { const data = (window as any).subagentFixture; data.reply = 'unknown'; data.holdMutation = true; });
  await page.locator('.subagent-prompt').fill('한 번만 전송'); await page.locator('.subagent-send').click();
  await expect.poll(async () => (await mutations(page)).length).toBe(1);
  await submit(page); await expect(page.locator('.subagent-send')).toBeDisabled();
  await page.locator('.subagent-close').click();
  await page.evaluate(() => (window as any).subagentFixture.releaseMutation());
  await expect.poll(() => page.evaluate(() => (window as any).subagentFixture.completed)).toBe(2);
  await page.evaluate(async () => { const state = window as any; await state.subagentManager.open(state.subagentFixture.session); });
  await expect(page.locator('.subagent-unknown')).toBeVisible(); await expect(page.locator('.subagent-send')).toBeDisabled();
  const requestId = (await mutations(page))[0].request.requestId;
  for (const ack of [{ mutationState: 'clear' }, { mutationState: 'clear', reconciledRequestId: 'another-request' }, { mutationState: 'unknown', reconciledRequestId: requestId }]) {
    await page.evaluate(ack => { const snapshot = (window as any).subagentFixture.snapshot; delete snapshot.reconciledRequestId; Object.assign(snapshot, ack); }, ack);
    await refresh(page); await expect(page.locator('.subagent-send')).toBeDisabled();
  }
  await page.clock.fastForward(15001); expect((await mutations(page)).length).toBe(1);
  await page.evaluate(requestId => Object.assign((window as any).subagentFixture.snapshot, { mutationState: 'clear', reconciledRequestId: requestId, parentRuntimeId: '33333333-3333-4333-8333-333333333333' }), requestId);
  await refresh(page); await expect(page.locator('.subagent-send')).toBeDisabled(); await expect(page.locator('.subagent-unknown')).toBeVisible();
  await page.evaluate(requestId => Object.assign((window as any).subagentFixture.snapshot, { mutationState: 'clear', reconciledRequestId: requestId, parentRuntimeId: '11111111-1111-4111-8111-111111111111' }), requestId);
  await refresh(page); await expect(page.locator('.subagent-send')).toBeEnabled(); await expect(page.locator('.subagent-unknown')).toBeHidden();
  expect((await mutations(page)).length).toBe(1);
});

test('늦은 이전 세대 목록은 새 세션 화면을 덮지 않는다', async ({ page }) => {
  await fixture(page);
  await page.evaluate(() => { const data = (window as any).subagentFixture; data.holdRead = true; data.snapshot.agents[0].nickname = '이전 세대 이름'; });
  await page.locator('.subagent-refresh').click();
  await expect(page.locator('#subagents-dialog')).toHaveAttribute('aria-busy', 'true');
  await page.evaluate(async () => {
    const state = window as any; const data = state.subagentFixture;
    data.session = { id: 'session-b', generation: 12, title: '에이전트 세션 B', active: true, threadId: 'thread-b' };
    data.snapshot.agents = []; await state.subagentManager.open(data.session);
  });
  await expect(page.locator('.subagent-session')).toHaveText('세션: 에이전트 세션 B');
  await page.evaluate(() => (window as any).subagentFixture.releaseRead());
  await expect(page.locator('.subagent-list')).toHaveText('이 세션의 직접 하위 에이전트가 없습니다.');
  await expect(page.locator('#subagents-dialog')).not.toContainText('이전 세대 이름');
  await page.evaluate(() => { const state = window as any; state.subagentManager.updateSessions([{ ...state.subagentFixture.session, generation: 13 }]); });
  await expect(page.locator('.subagent-refresh')).toBeDisabled(); await expect(page.locator('.subagent-status')).toContainText('종료되거나 바뀌었습니다');
  await submit(page); expect(await mutations(page)).toEqual([]);
});

test('실패 원문을 노출하지 않고 목록·입력 초안·커서를 보존한다', async ({ page }) => {
  await fixture(page);
  const prompt = page.locator('.subagent-prompt'); await prompt.fill('보존할 초안'); await prompt.focus();
  await prompt.evaluate((input: HTMLTextAreaElement) => input.setSelectionRange(2, 4));
  await page.evaluate(() => { (window as any).subagentFixture.holdRead = true; });
  await page.clock.fastForward(5001);
  await expect(page.locator('#subagents-dialog')).toHaveAttribute('aria-busy', 'true');
  await expect(prompt).toBeEnabled(); await expect(prompt).toBeFocused();
  expect(await prompt.evaluate((input: HTMLTextAreaElement) => [input.selectionStart, input.selectionEnd])).toEqual([2, 4]);
  await page.evaluate(() => (window as any).subagentFixture.releaseRead());
  await expect(page.locator('#subagents-dialog')).toHaveAttribute('aria-busy', 'false');
  await expect(prompt).toBeFocused(); await expect(prompt).toHaveValue('보존할 초안');
  await page.evaluate(() => { (window as any).subagentFixture.failRead = true; }); await refresh(page);
  await expect(page.locator('.subagent-error')).toBeVisible(); await expect(page.locator('.subagent-row')).toHaveCount(1);
  await page.evaluate(() => { const data = (window as any).subagentFixture; data.failRead = false; data.failMutation = true; }); await refresh(page);
  await page.locator('.subagent-send').click(); await expect(page.locator('.subagent-unknown')).toBeVisible(); await expect(prompt).toHaveValue('보존할 초안');
  const exposed = await page.evaluate(() => ({ html: document.querySelector('#subagents-dialog')!.outerHTML, notices: (window as any).subagentFixture.notices }));
  expect(JSON.stringify(exposed)).not.toContain('PRIVATE_TOKEN_PATH_NEVER_REAL');
});

test('잘못된 목록과 비활성 세션은 새 변경을 허용하지 않는다', async ({ page }) => {
  await fixture(page);
  const agent = await page.evaluate(() => structuredClone((window as any).subagentFixture.snapshot.agents[0]));
  for (const agents of [[agent, agent], [{ ...agent, status: 'toString' }], [{ ...agent, capabilities: { send: 'true' } }]]) {
    await page.evaluate(agents => { (window as any).subagentFixture.snapshot.agents = agents; }, agents); await refresh(page);
    await expect(page.locator('.subagent-error')).toBeVisible(); await expect(page.locator('.subagent-send')).toBeDisabled(); await expect(page.locator('.subagent-row')).toHaveCount(1);
  }
  await page.evaluate(async () => { const state = window as any; await state.subagentManager.open({ ...state.subagentFixture.session, threadId: null }); });
  await expect(page.locator('.subagent-status')).toContainText('대화 식별자'); await submit(page); expect(await mutations(page)).toEqual([]);
});

test('부모·하위 실행 세대가 누락되거나 잘못되면 차단하고 null 하위 세대는 그대로 전달한다', async ({ page }) => {
  await fixture(page);
  const agent = await page.evaluate(() => structuredClone((window as any).subagentFixture.snapshot.agents[0]));
  const parentRuntimeId = '11111111-1111-4111-8111-111111111111';
  for (const invalid of [
    { parentRuntimeId: null, runtimeId: agent.runtimeId },
    { parentRuntimeId: 'wrong-runtime', runtimeId: agent.runtimeId },
    { parentRuntimeId, runtimeId: undefined },
    { parentRuntimeId, runtimeId: 'wrong-child-runtime' },
  ]) {
    await page.evaluate(({ invalid, agent }) => { const snapshot = (window as any).subagentFixture.snapshot; snapshot.parentRuntimeId = invalid.parentRuntimeId; snapshot.agents = [{ ...agent, runtimeId: invalid.runtimeId }]; }, { invalid, agent });
    await refresh(page); await expect(page.locator('.subagent-error')).toBeVisible(); await expect(page.locator('.subagent-send')).toBeDisabled();
    await submit(page); expect(await mutations(page)).toEqual([]);
  }
  await page.evaluate(({ agent, parentRuntimeId }) => { const snapshot = (window as any).subagentFixture.snapshot; snapshot.parentRuntimeId = parentRuntimeId; snapshot.agents = [{ ...agent, runtimeId: null }]; }, { agent, parentRuntimeId });
  await refresh(page); await page.locator('.subagent-resume').click();
  await expect.poll(async () => (await mutations(page)).length).toBe(1);
  expect((await mutations(page))[0].request).toMatchObject({ type: 'resume', expectedParentRuntimeId: parentRuntimeId, expectedRuntimeId: null });
});

test('같은 이벤트에서 닫으면 대기 중 변경을 시작하지 않는다', async ({ page }) => {
  await fixture(page); await page.locator('.subagent-prompt').fill('닫기 전 작성한 내용');
  await page.evaluate(() => { document.querySelector('.subagent-form')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })); (document.querySelector('#subagents-dialog') as HTMLDialogElement).close(); });
  await expect(page.locator('#subagents-dialog')).not.toBeVisible(); expect(await mutations(page)).toEqual([]);
});

test('숨김·닫힘·종료·폐기는 목록 조회와 행 참조를 정리한다', async ({ page }) => {
  await fixture(page, { count: 100 });
  await page.clock.fastForward(5001); await expect.poll(() => page.evaluate(() => (window as any).subagentFixture.calls.length)).toBe(2);
  await page.evaluate(() => (window as any).subagentManager.setVisible(false));
  const hiddenCount = await page.evaluate(() => (window as any).subagentFixture.calls.length);
  await page.clock.fastForward(20001); expect(await page.evaluate(() => (window as any).subagentFixture.calls.length)).toBe(hiddenCount);
  await page.evaluate(() => (window as any).subagentManager.setVisible(true)); await expect.poll(() => page.evaluate(() => (window as any).subagentFixture.calls.length)).toBe(hiddenCount + 1);
  await page.evaluate(() => { (window as any).closedSubagentRow = new WeakRef(document.querySelector('.subagent-row')!); });
  await page.locator('.subagent-close').click(); await expect(page.locator('.subagent-row')).toHaveCount(0);
  await page.clock.fastForward(20001); expect(await page.evaluate(() => (window as any).subagentFixture.calls.length)).toBe(hiddenCount + 1);
  const gc = await page.context().newCDPSession(page);
  try { await expect.poll(async () => { await gc.send('HeapProfiler.collectGarbage'); return page.evaluate(() => (window as any).closedSubagentRow.deref() === undefined); }).toBe(true); } finally { await gc.detach(); }
  await page.evaluate(async () => { const state = window as any; await state.subagentManager.open(state.subagentFixture.session); await state.subagentManager.beginExit(); });
  const exitCount = await page.evaluate(() => (window as any).subagentFixture.calls.length);
  await page.clock.fastForward(20001); expect(await page.evaluate(() => (window as any).subagentFixture.calls.length)).toBe(exitCount);
  await page.evaluate(() => { const state = window as any; state.subagentManager.cancelExit(); state.subagentManager.dispose(); });
  await expect(page.locator('#subagents-dialog')).toHaveCount(0);
  await page.clock.fastForward(20001); expect(await page.evaluate(() => (window as any).subagentFixture.calls.length)).toBe(exitCount); expect(await mutations(page)).toEqual([]);
});

test('언어 전환은 관리 라벨을 바꾸고 초안을 보존한다', async ({ page }) => {
  await fixture(page); await page.locator('.subagent-prompt').fill('작성 중');
  await page.evaluate(async () => { const { setLanguage } = await import('/src/i18n.ts'); setLanguage('en'); });
  await expect(page.locator('#subagent-title')).toHaveText('Subagents'); await expect(page.locator('.subagent-row small')).toContainText('Running'); await expect(page.locator('.subagent-send')).toHaveText('Send');
  await expect(page.locator('.subagent-prompt')).toHaveValue('작성 중');
  await page.evaluate(async () => { const { setLanguage } = await import('/src/i18n.ts'); setLanguage('ko'); });
  await expect(page.locator('.subagent-send')).toHaveText('보내기');
});
