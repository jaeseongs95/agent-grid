import { expect, test, type Page } from '@playwright/test';

async function conversation(page: Page) {
  await page.goto('/');
  await page.evaluate(async () => {
    const state = window as any;
    const { createConversation } = await import('/src/conversation.ts');
    const host = document.createElement('section'); host.id = 'conversation-fixture';
    host.style.cssText = 'position:fixed;inset:20px;z-index:9999;background:#141820;'; document.body.append(host);
    const fixture = state.conversationFixture = { calls: [] as any[], queue: [] as any[], pending: [] as any[], owners: [] as boolean[], rejectSend: false, rejectDetach: false, holdItems: false, releaseItems: () => {}, nextSeq: 1, unknownResult: undefined as boolean | undefined };
    state.conversation = createConversation({
      sessionId: 'conversation-owned', generation: 7, host,
      ownerChanged: (owned: boolean) => fixture.owners.push(owned),
      async call(command: string, args: any = {}) {
        fixture.calls.push({ command, args });
        if (command === 'choose_conversation_attachments') return [{ type: 'file', path: 'D:\\work\\notes.txt', name: 'notes.txt' }, { type: 'localImage', path: 'D:\\work\\image.png', name: 'image.png' }];
        if (command === 'stage_conversation_image') return { type: 'localImage', path: 'D:\\stage\\pasted.png', name: 'pasted.png' };
        if (command === 'cleanup_conversation_attachment') return null;
        if (command !== 'conversation_request') throw Error(`알 수 없는 명령 ${command}`);
        const request = args.request;
        if (request.type === 'attach') return { threadId: 'thread-a', owner: 'gui', model: 'runtime-model', reasoningEffort: 'medium' };
        if (request.type === 'detach') { if (fixture.rejectDetach) throw Error('승인 대기 중에는 전환할 수 없습니다'); return {}; }
        if (request.type === 'page') return { data: [{ id: request.cursor ? 'older-turn' : 'recent-turn', status: 'completed' }], nextCursor: request.cursor ? null : 'turn-page-2' };
        if (request.type === 'items') {
          if (fixture.holdItems) await new Promise<void>(resolve => { fixture.releaseItems = resolve; });
          return request.cursor ? { data: [{ id: 'earlier-message', type: 'agentMessage', text: '이전 페이지 원문' }], nextCursor: null } : { data: [
            { id: 'tool-a', type: 'commandExecution', command: 'npm test', aggregatedOutput: '검사 성공', status: 'completed' },
            { id: 'assistant-a', type: 'agentMessage', text: '답변\n```ts\nconst safe = true;\n```' },
            { id: 'user-a', type: 'userMessage', content: [{ type: 'text', text: '<img src=x onerror="window.injected=true">' }] },
          ], nextCursor: 'items-page-2' };
        }
        if (request.type === 'models') return { data: [{ id: 'runtime-model-id', model: 'runtime-model', displayName: '실제 런타임 모델', supportedReasoningEfforts: [{ reasoningEffort: 'low' }, { reasoningEffort: 'medium' }] }], nextCursor: null };
        if (request.type === 'poll') return { events: fixture.queue.splice(0), pendingRequests: fixture.pending, connected: true, gap: false, unknownResult: fixture.unknownResult };
        if (request.type === 'send') { if (fixture.rejectSend) throw Error('응답 시간 초과'); return { turn: { id: 'sent-turn', status: 'inProgress' } }; }
        if (request.type === 'interrupt') { fixture.queue.push({ seq: fixture.nextSeq++, method: 'turn/completed', params: { turn: { id: request.turnId, status: 'interrupted' } } }); return {}; }
        if (request.type === 'respond') { fixture.pending = fixture.pending.map((entry: any) => entry.requestId === request.requestId ? { ...entry, responded: true } : entry); return {}; }
        if (request.type === 'detail') return { text: request.offset ? '마지막 원문' : '첫 원문', nextOffset: request.offset ? null : 16, totalBytes: 30 };
        throw Error(`지원하지 않는 요청 ${request.type}`);
      },
    });
    await state.conversation.setVisible(true);
  });
  await expect(page.locator('#conversation-fixture .conversation-input')).toBeEnabled();
}
const view = (page: Page) => page.locator('#conversation-fixture');

test('최근 페이지와 구조화 출력은 안전한 텍스트로 보이고 페이지 단위로 교체된다', async ({ page }, testInfo) => {
  await conversation(page);
  await expect(view(page).locator('.conversation-message')).toHaveCount(3);
  await expect(view(page).locator('.conversation-user')).toContainText('<img src=x');
  expect(await page.evaluate(() => (window as any).injected)).toBeUndefined();
  await expect(view(page).locator('img')).toHaveCount(0);
  await expect(view(page).locator('.conversation-code code')).toHaveText('const safe = true;\n');
  await expect(view(page).locator('.conversation-tool details')).not.toHaveAttribute('open');
  await view(page).locator('.conversation-tool summary').click();
  await expect(view(page).locator('.conversation-tool')).toContainText('검사 성공');
  await page.screenshot({ path: testInfo.outputPath('conversation.png') });
  await view(page).locator('.conversation-more').click();
  await expect(view(page).locator('.conversation-message')).toHaveCount(1);
  await expect(view(page).locator('.conversation-messages')).toHaveText('도우미이전 페이지 원문');
  await view(page).locator('.conversation-refresh').click();
  await expect(view(page).locator('.conversation-message')).toHaveCount(3);
  await view(page).locator('.conversation-older').click();
  await expect(view(page).locator('.conversation-turns')).toHaveValue('older-turn');
  const calls = await page.evaluate(() => (window as any).conversationFixture.calls);
  expect(calls.find((entry: any) => entry.args.request?.cursor === 'items-page-2').args).toMatchObject({ id: 'conversation-owned', generation: 7 });
});

test('여러 줄과 한글 조합은 전송하지 않으며 모델·추론·첨부는 실제 전송 요청에 반영된다', async ({ page }) => {
  await conversation(page);
  const input = view(page).locator('.conversation-input');
  await input.fill('첫 줄'); await input.press('Shift+Enter'); await expect(input).toHaveValue('첫 줄\n');
  await input.evaluate(node => { node.dispatchEvent(new CompositionEvent('compositionstart', { bubbles: true })); node.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true, isComposing: true })); node.dispatchEvent(new CompositionEvent('compositionend', { bubbles: true })); });
  expect(await page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'send').length)).toBe(0);
  await input.fill('첫 줄\n둘째 줄');
  await view(page).locator('.conversation-attach').click();
  await expect(view(page).locator('.conversation-attachment')).toHaveCount(2);
  await view(page).locator('.conversation-effort').selectOption('low');
  await input.press('Enter');
  await expect(input).toHaveValue('');
  const sent = await page.evaluate(() => (window as any).conversationFixture.calls.find((value: any) => value.args.request?.type === 'send').args.request);
  expect(sent).toMatchObject({ text: '첫 줄\n둘째 줄', model: 'runtime-model', effort: 'low', attachments: [{ type: 'file', path: 'D:\\work\\notes.txt' }, { type: 'localImage', path: 'D:\\work\\image.png' }] });
  expect(sent.attachments[0]).not.toHaveProperty('name');
  await expect(view(page).locator('.conversation-stop')).toBeEnabled();
  await view(page).locator('.conversation-stop').click();
  await expect(view(page).locator('.conversation-stop')).toBeDisabled();
});

test('전송 결과 불명은 초안을 보존하고 자동 재전송하지 않는다', async ({ page }) => {
  await conversation(page);
  await page.evaluate(() => { (window as any).conversationFixture.rejectSend = true; });
  await view(page).locator('.conversation-input').fill('중복되면 안 되는 요청');
  await view(page).locator('.conversation-send').click();
  await expect(view(page).locator('.conversation-status')).toContainText('자동으로 다시 보내지 않습니다');
  await expect(view(page).locator('.conversation-input')).toHaveValue('중복되면 안 되는 요청');
  await expect(view(page).locator('.conversation-send')).toBeDisabled();
  await page.waitForTimeout(900);
  expect(await page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'send').length)).toBe(1);
});

test('전환은 세션별 초안을 보존하며 detach 거절 때 소유권과 대화 화면을 유지한다', async ({ page }) => {
  await conversation(page);
  await view(page).locator('.conversation-input').fill('보존할 초안');
  await page.evaluate(async () => { await (window as any).conversation.setVisible(false); });
  await expect(view(page)).toBeHidden();
  await page.evaluate(async () => { await (window as any).conversation.setVisible(true); });
  await expect(view(page).locator('.conversation-input')).toHaveValue('보존할 초안');
  const rejected = await page.evaluate(async () => { const state = window as any; state.conversationFixture.rejectDetach = true; try { await state.conversation.setVisible(false); return false; } catch { return true; } });
  expect(rejected).toBe(true);
  await expect(view(page)).toBeVisible();
  expect(await page.evaluate(() => (window as any).conversationFixture.owners.at(-1))).toBe(true);
  await expect(view(page).locator('.conversation-status')).toContainText('승인 대기');
});

test('승인·선택 질문은 명시 응답만 보내고 poll 동안 선택을 보존한다', async ({ page }) => {
  await conversation(page);
  await page.evaluate(() => {
    const fixture = (window as any).conversationFixture;
    fixture.pending = [
      { seq: 1, method: 'item/commandExecution/requestApproval', requestId: 'approval-1', params: { command: 'npm test' } },
      { seq: 2, method: 'item/tool/requestUserInput', requestId: 'question-1', params: { questions: [{ id: 'scope', question: '범위를 선택하세요', options: [{ label: '좁게' }, { label: '넓게' }] }] } },
    ];
  });
  await expect(view(page).locator('.conversation-request')).toHaveCount(2);
  const select = view(page).locator('.conversation-request select'); await select.selectOption('좁게');
  await page.waitForTimeout(900); await expect(select).toHaveValue('좁게');
  await view(page).getByRole('button', { name: '이번만 승인', exact: true }).click();
  await view(page).getByRole('button', { name: '답변 보내기', exact: true }).click();
  await expect(view(page).getByRole('button', { name: '이번만 승인', exact: true })).toBeDisabled();
  const responses = await page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'respond').map((value: any) => value.args.request));
  expect(responses).toEqual([{ type: 'respond', requestId: 'approval-1', result: { decision: 'accept' } }, { type: 'respond', requestId: 'question-1', result: { answers: { scope: { answers: ['좁게'] } } } }]);
});

test('큰 항목은 상세 구간으로 끝까지 접근하고 dispose 후 조회·화면 갱신이 멈춘다', async ({ page }) => {
  await conversation(page);
  await page.evaluate(() => {
    const fixture = (window as any).conversationFixture;
    fixture.queue.push({ seq: fixture.nextSeq++, method: 'item/completed', params: { turnId: 'recent-turn', item: { id: 'large-item', type: 'agentMessage', text: { __detail: true, bytes: 30000 } } } });
  });
  await expect(view(page).getByRole('button', { name: '전체 내용 보기' })).toBeVisible();
  await view(page).getByRole('button', { name: '전체 내용 보기' }).click();
  await expect(view(page).locator('[data-item-id="large-item"]')).toContainText('첫 원문');
  await view(page).getByRole('button', { name: '다음 내용' }).click();
  await expect(view(page).locator('[data-item-id="large-item"]')).toContainText('마지막 원문');
  await page.evaluate(async () => { await (window as any).conversation.dispose(); });
  const calls = await page.evaluate(() => (window as any).conversationFixture.calls.length);
  await page.waitForTimeout(900);
  expect(await page.evaluate(() => (window as any).conversationFixture.calls.length)).toBe(calls);
  await expect(view(page).locator('*')).toHaveCount(0);
  expect(await page.evaluate(() => (window as any).conversationFixture.owners.at(-1))).toBe(false);
});

test('백그라운드에서는 소유권을 유지하고 조회·표시를 비운 뒤 최근 페이지로 복구한다', async ({ page }) => {
  await conversation(page);
  await view(page).locator('.conversation-input').fill('백그라운드 초안');
  await page.evaluate(async () => { await (window as any).conversation.setForeground(false); });
  await expect(view(page).locator('.conversation-message')).toHaveCount(0);
  const count = await page.evaluate(() => (window as any).conversationFixture.calls.length);
  await page.waitForTimeout(900);
  expect(await page.evaluate(() => (window as any).conversationFixture.calls.length)).toBe(count);
  expect(await page.evaluate(() => (window as any).conversationFixture.owners)).toEqual([true]);
  await page.evaluate(async () => { await (window as any).conversation.setForeground(true); });
  await expect(view(page).locator('.conversation-message')).toHaveCount(3);
  await expect(view(page).locator('.conversation-input')).toHaveValue('백그라운드 초안');
});

test('이벤트 순번·보존 상한과 이미지 붙여넣기·참조 제거 경계를 지킨다', async ({ page }) => {
  await conversation(page);
  await view(page).locator('.conversation-input').evaluate(node => {
    const data = new DataTransfer(); data.items.add(new File([new Uint8Array([137, 80, 78, 71])], 'paste.png', { type: 'image/png' }));
    node.dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }));
  });
  await expect(view(page).locator('.conversation-attachment')).toHaveCount(1);
  await view(page).locator('.conversation-attachment button').click();
  await expect(view(page).locator('.conversation-attachment')).toHaveCount(0);
  const cleanup = await page.evaluate(() => (window as any).conversationFixture.calls.find((value: any) => value.command === 'cleanup_conversation_attachment'));
  expect(cleanup.args).toEqual({ id: 'conversation-owned', generation: 7, path: 'D:\\stage\\pasted.png' });
  await page.evaluate(() => {
    const fixture = (window as any).conversationFixture;
    for (let seq = 1; seq <= 120; seq++) fixture.queue.push({ seq, method: 'item/completed', params: { turnId: 'recent-turn', item: { id: `unique-${seq}`, type: 'agentMessage', text: `${seq} 고유 원문` } } });
    fixture.queue.push({ seq: 119, method: 'item/completed', params: { turnId: 'recent-turn', item: { id: 'unique-119', type: 'agentMessage', text: '역순 덮어쓰기 금지' } } });
    fixture.queue.push({ seq: 123, method: 'item/completed', params: { turnId: 'recent-turn', item: { id: 'huge-delta', type: 'agentMessage', text: '가'.repeat(25000) } } });
  });
  await expect(view(page).locator('.conversation-message')).toHaveCount(100);
  await expect(view(page).locator('[data-item-id="unique-119"]')).toContainText('119 고유 원문');
  await expect(view(page).locator('[data-item-id="huge-delta"]')).toContainText('미리보기');
  await expect(view(page).locator('.conversation-refresh')).toHaveClass(/needs-refresh/);
  expect((await view(page).locator('[data-item-id="huge-delta"]').textContent())!.length).toBeLessThan(17000);
});

test('명시 거절은 초안을 편집할 수 있고 늦은 조회는 숨긴 화면을 다시 채우지 않는다', async ({ page }) => {
  await conversation(page);
  await page.evaluate(() => { const fixture = (window as any).conversationFixture; fixture.rejectSend = true; fixture.unknownResult = false; });
  await view(page).locator('.conversation-input').fill('거절되어도 보존할 초안');
  await view(page).locator('.conversation-send').click();
  await expect(view(page).locator('.conversation-status')).toContainText('전송이 거절되었습니다');
  await expect(view(page).locator('.conversation-input')).toBeEnabled();
  await expect(view(page).locator('.conversation-input')).toHaveValue('거절되어도 보존할 초안');
  await page.evaluate(() => { const state = window as any; state.conversationFixture.holdItems = true; state.refreshPromise = state.conversation.refresh(); });
  await expect.poll(() => page.evaluate(() => (window as any).conversationFixture.calls.filter((entry: any) => entry.args.request?.type === 'items').length)).toBe(2);
  await page.evaluate(async () => { await (window as any).conversation.setForeground(false); (window as any).conversationFixture.releaseItems(); await (window as any).refreshPromise; });
  await expect(view(page).locator('.conversation-message')).toHaveCount(0);
  await page.evaluate(async () => { await (window as any).conversation.dispose({ sessionEnded: true }); });
  const count = await page.evaluate(() => (window as any).conversationFixture.calls.length);
  await page.waitForTimeout(900);
  expect(await page.evaluate(() => (window as any).conversationFixture.calls.length)).toBe(count);
});
