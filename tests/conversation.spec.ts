import { expect, test, type Page } from '@playwright/test';

async function conversation(page: Page, options: { historyUnavailable?: boolean } = {}) {
  await page.goto('/');
  await page.evaluate(async options => {
    const state = window as any;
    const { createConversation } = await import('/src/conversation.ts');
    const host = document.createElement('section'); host.id = 'conversation-fixture';
    host.style.cssText = 'position:fixed;inset:20px;z-index:9999;background:#141820;'; document.body.append(host);
    const fixture = state.conversationFixture = { calls: [] as any[], queue: [] as any[], pending: [] as any[], owners: [] as boolean[], rawFailure: '', failAt: '', pollError: '', rejectSend: false, rejectDetach: false, rejectHandoff: false, holdHandoff: false, releaseHandoff: () => {}, holdItems: false, releaseItems: () => {}, heldItems: [] as any[], historyTag: '', pageResult: undefined as any, holdPage: false, releasePage: () => {}, holdStart: false, releaseStart: () => {}, nextSeq: 1, unknownResult: undefined as boolean | undefined, detailReads: 0, largeHistory: false, invalidHistory: '', historyUnavailable: options.historyUnavailable === true, connected: true };
    state.conversation = createConversation({
      sessionId: 'conversation-owned', generation: 7, host,
      ownerChanged: (owned: boolean) => fixture.owners.push(owned),
      async call(command: string, args: any = {}) {
        fixture.calls.push({ command, args });
        if (fixture.failAt === command) throw Error(fixture.rawFailure);
        if (command === 'choose_conversation_attachments') return [{ type: 'file', path: 'D:\\work\\notes.txt', name: 'notes.txt' }, { type: 'localImage', path: 'D:\\work\\image.png', name: 'image.png' }];
        if (command === 'stage_conversation_image') return { type: 'localImage', path: 'D:\\stage\\pasted.png', name: 'pasted.png' };
        if (command === 'cleanup_conversation_attachment') return null;
        if (command !== 'conversation_request') throw Error(`알 수 없는 명령 ${command}`);
        const request = args.request;
        if (fixture.failAt === request.type) throw Error(fixture.rawFailure);
        if (request.type === 'attach') return { threadId: 'thread-a', owner: 'gui', model: 'runtime-model', reasoningEffort: 'medium', historyUnavailable: fixture.historyUnavailable, activeTurnKnown: !fixture.historyUnavailable, activeTurnId: null };
        if (request.type === 'handoffToTerminal') { if (fixture.holdHandoff) await new Promise<void>(resolve => { fixture.releaseHandoff = resolve; }); if (fixture.rejectHandoff) throw Error('인계할 수 없는 요청 또는 전송 결과 미확인이 남아 있습니다'); return {}; }
        if (request.type === 'detach') { if (fixture.rejectDetach) throw Error(fixture.rawFailure || '승인 대기 중에는 전환할 수 없습니다'); return {}; }
        if (request.type === 'page') { if (fixture.historyUnavailable && !fixture.pageResult) throw Error('대화 이력을 아직 조회할 수 없습니다.'); const snapshot = fixture.pageResult ?? { data: [{ id: request.cursor ? 'older-turn' : 'recent-turn', status: 'completed' }], nextCursor: request.cursor ? null : 'turn-page-2' }; if (fixture.holdPage) await new Promise<void>(resolve => { fixture.releasePage = resolve; }); return snapshot; }
        if (request.type === 'items') {
          const turnId = request.turnId === 'older-turn' ? 'older-turn' : 'recent-turn';
          const data = request.cursor ? [{ turnId, item: { id: 'earlier-message', type: 'agentMessage', text: '이전 페이지 원문' } }] : [
            { turnId, item: { id: 'tool-a', type: 'commandExecution', command: 'npm test', aggregatedOutput: '검사 성공', status: 'completed' } },
            { turnId, item: { id: 'assistant-a', type: 'agentMessage', text: '답변\n```ts\nconst safe = true;\n```' } },
            { turnId, item: { id: 'user-a', type: 'userMessage', content: [{ type: 'text', text: '<img src=x onerror="window.injected=true">' }] } },
          ];
          if (fixture.largeHistory) data.unshift({ turnId, item: { id: 'large-item', type: 'agentMessage', text: { __detail: true, bytes: 30000 } } } as any);
          if (fixture.invalidHistory === 'other-turn') data[0].turnId = 'unrelated-turn';
          if (fixture.invalidHistory === 'missing-id') delete (data[0].item as any).id;
          if (fixture.historyTag) (data.find(entry => entry.item.id === 'assistant-a')!.item as any).text = fixture.historyTag;
          const snapshot = { data, nextCursor: request.cursor ? null : 'items-page-2', backwardsCursor: null };
          if (fixture.holdItems) await new Promise<void>(resolve => { fixture.releaseItems = resolve; fixture.heldItems.push({ turnId, release: resolve }); });
          return snapshot;
        }
        if (request.type === 'models') return { data: [
          { id: 'runtime-model-id', model: 'runtime-model', displayName: '실제 런타임 모델', supportedReasoningEfforts: [{ reasoningEffort: 'low' }, { reasoningEffort: 'medium' }] },
          { id: 'server-model-2', model: 'server-model-2', displayName: '서버의 두 번째 모델', supportedReasoningEfforts: [{ reasoningEffort: 'high' }, { reasoningEffort: 'medium' }] },
        ], nextCursor: null };
        if (request.type === 'poll') return { events: fixture.queue.splice(0), pendingRequests: fixture.pending, connected: fixture.connected, gap: false, unknownResult: fixture.unknownResult, error: fixture.pollError || undefined };
        if (request.type === 'send' || request.type === 'compact') {
          if (fixture.rejectSend) throw Error(fixture.rawFailure || '응답 시간 초과');
          if (fixture.holdStart) await new Promise<void>(resolve => { fixture.releaseStart = resolve; });
          return { status: 'started', turn: { id: request.type === 'send' ? 'sent-turn' : 'compacted-turn', status: 'inProgress' } };
        }
        if (request.type === 'interrupt') { fixture.queue.push({ seq: fixture.nextSeq++, method: 'turn/completed', params: { turn: { id: request.turnId, status: 'interrupted' } } }); return {}; }
        if (request.type === 'respond') { fixture.pending = fixture.pending.map((entry: any) => entry.requestId === request.requestId ? { ...entry, responded: true } : entry); return {}; }
        if (request.type === 'detail') {
          if (!request.offset) fixture.detailReads++;
          const snapshotId = `detail-snapshot-${fixture.detailReads}`;
          if (request.offset && request.snapshotId !== snapshotId) throw Error('같은 원문 스냅샷 식별자가 필요합니다');
          return { text: request.offset ? '마지막 원문' : '첫 원문', nextOffset: request.offset ? null : 16, totalBytes: 30, snapshotId };
        }
        throw Error(`지원하지 않는 요청 ${request.type}`);
      },
    });
    await state.conversation.setVisible(true);
  }, options);
  await expect(page.locator('#conversation-fixture .conversation-input')).toBeEnabled();
}
const view = (page: Page) => page.locator('#conversation-fixture');
const privateFailure = 'PRIVATE_INTERNAL_FAILURE C:\\private-cli\\codex.exe token=FAKE_AUTH_TOKEN_DO_NOT_DISPLAY';
async function expectPrivateFailureHidden(page: Page) {
  const html = await view(page).evaluate(node => node.outerHTML);
  for (const marker of ['PRIVATE_INTERNAL_FAILURE', 'private-cli', 'FAKE_AUTH_TOKEN_DO_NOT_DISPLAY']) expect(html).not.toContain(marker);
}

test('모델·첨부·승인·상세·인계 오류는 원문과 경로 대신 행동 안내만 표시한다', async ({ page }) => {
  for (const operation of ['models', 'choose_conversation_attachments', 'stage_conversation_image', 'respond', 'detail', 'handoffToTerminal']) {
    await test.step(operation, async () => {
      await conversation(page);
      await view(page).locator('.conversation-input').fill('오류 뒤 보존할 초안');
      await page.evaluate(({ operation, privateFailure }) => {
        const fixture = (window as any).conversationFixture; fixture.failAt = operation; fixture.rawFailure = privateFailure;
        if (operation === 'respond') fixture.pending = [{ seq: 1, requestId: 'approval-private', method: 'item/commandExecution/requestApproval', params: { command: '검사 명령' } }];
        if (operation === 'detail') fixture.largeHistory = true;
      }, { operation, privateFailure });
      if (operation === 'models') await page.evaluate(async () => { const state = window as any; await state.conversation.setVisible(false); await state.conversation.setVisible(true); });
      else if (operation === 'choose_conversation_attachments') await view(page).locator('.conversation-attach').click();
      else if (operation === 'stage_conversation_image') await view(page).locator('.conversation-input').evaluate(node => {
        const data = new DataTransfer(); data.items.add(new File(['image'], 'pasted.png', { type: 'image/png' }));
        node.dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true }));
      });
      else if (operation === 'respond') await view(page).getByRole('button', { name: '이번만 승인', exact: true }).click();
      else if (operation === 'detail') {
        await view(page).locator('.conversation-refresh').click();
        await view(page).locator('[data-item-id="large-item"]').getByRole('button', { name: '전체 내용 보기', exact: true }).click();
      } else await page.evaluate(async () => { try { await (window as any).conversation.handoffToTerminal(); } catch {} });
      await expect(view(page).locator('.conversation-status')).toContainText('연결과 진행 중인 요청을 확인해 주세요');
      await expect(view(page).locator('.conversation-input')).toHaveValue('오류 뒤 보존할 초안');
      await expectPrivateFailureHidden(page);
      if (operation === 'models') await expect(view(page).locator('.conversation-model')).toBeDisabled();
      if (operation === 'respond') await expect(view(page).getByRole('button', { name: '이번만 승인', exact: true })).toBeDisabled();
      if (operation === 'handoffToTerminal') expect(await page.evaluate(() => (window as any).conversationFixture.owners)).toEqual([true]);
      await page.evaluate(async () => { const { setLanguage } = await import('/src/i18n.ts'); setLanguage('en'); });
      await expect(view(page).locator('.conversation-status')).toContainText('Check the connection and any pending requests');
      await expectPrivateFailureHidden(page);
      await page.evaluate(async () => { const { setLanguage } = await import('/src/i18n.ts'); setLanguage('ko'); });
    });
  }
});

test('전송 원문 오류는 숨기면서 결과 미확인 차단과 확정 거절의 초안 보존을 구분한다', async ({ page }) => {
  await conversation(page);
  await view(page).locator('.conversation-input').fill('중복 전송하면 안 되는 초안');
  await page.evaluate(privateFailure => { const fixture = (window as any).conversationFixture; fixture.rejectSend = true; fixture.rawFailure = privateFailure; }, privateFailure);
  await view(page).locator('.conversation-send').click();
  await expect(view(page).locator('.conversation-status')).toContainText('자동으로 다시 보내지 않습니다');
  await expect(view(page).locator('.conversation-send')).toBeDisabled();
  await expectPrivateFailureHidden(page);
  await page.waitForTimeout(850);
  expect(await page.evaluate(() => (window as any).conversationFixture.calls.filter((entry: any) => entry.args.request?.type === 'send').length)).toBe(1);
  await page.evaluate(() => { (window as any).conversationFixture.unknownResult = false; });
  await expect(view(page).locator('.conversation-status')).toContainText('입력 내용을 확인한 뒤 다시 전송할 수 있습니다');
  await expect(view(page).locator('.conversation-input')).toBeEnabled();
  await expect(view(page).locator('.conversation-input')).toHaveValue('중복 전송하면 안 되는 초안');
  await expectPrivateFailureHidden(page);
});

test('작업·연결·삭제 해제 오류는 원문을 숨기고 도구 본문과 입력 차단을 보존한다', async ({ page }) => {
  await conversation(page);
  await view(page).locator('.conversation-input').fill('삭제 뒤 보존할 초안');
  await page.evaluate(privateFailure => {
    const fixture = (window as any).conversationFixture; fixture.rawFailure = privateFailure;
    fixture.queue.push(
      { seq: fixture.nextSeq++, method: 'turn/completed', params: { turn: { id: 'recent-turn', status: 'failed', error: { message: privateFailure, additionalDetails: privateFailure } } } },
      { seq: fixture.nextSeq++, method: 'error', params: { error: { message: privateFailure }, privateDetails: privateFailure } },
      { seq: fixture.nextSeq++, method: 'item/completed', params: { turnId: 'recent-turn', item: { id: 'tool-public', type: 'commandExecution', command: '검사 명령', aggregatedOutput: '도구 본문 PUBLIC_TOOL_OUTPUT 유지', status: 'completed' } } },
    );
  }, privateFailure);
  await expect(view(page).locator('.conversation-error')).toHaveCount(2);
  await expect(view(page).locator('.conversation-error').first()).toContainText('연결과 진행 중인 요청을 확인해 주세요');
  await view(page).locator('[data-item-id="tool-public"] summary').click();
  await expect(view(page).locator('[data-item-id="tool-public"]')).toContainText('도구 본문 PUBLIC_TOOL_OUTPUT 유지');
  await expectPrivateFailureHidden(page);
  await page.evaluate(privateFailure => { const fixture = (window as any).conversationFixture; fixture.pollError = privateFailure; fixture.connected = false; }, privateFailure);
  await expect(view(page).locator('.conversation-status')).toContainText('연결이 복구될 때까지 기다려 주세요');
  await expect(view(page).locator('.conversation-send')).toBeDisabled();
  await expectPrivateFailureHidden(page);
  await page.evaluate(() => { const fixture = (window as any).conversationFixture; fixture.pollError = ''; fixture.rejectDetach = true; fixture.queue.push({ seq: fixture.nextSeq++, method: 'thread/deleted', params: { threadId: 'thread-a' } }); });
  await expect(view(page).locator('.conversation-status')).toContainText('연결 해제를 확인하지 못해 입력을 제한합니다');
  await expect(view(page).locator('.conversation-input')).toHaveValue('삭제 뒤 보존할 초안');
  await expect(view(page).locator('.conversation-input')).toBeDisabled();
  expect(await page.evaluate(() => (window as any).conversationFixture.owners)).toEqual([true]);
  await expectPrivateFailureHidden(page);
});

test('명시 인계만 전용 요청을 보내고 성공 확인 뒤 화면과 입력 소유권을 해제한다', async ({ page }) => {
  await conversation(page);
  await view(page).locator('.conversation-input').fill('인계 뒤에도 보존할 초안');
  await page.evaluate(() => { (window as any).conversationFixture.pending = [{ seq: 1, requestId: 'ordinary-approval', method: 'item/commandExecution/requestApproval', params: { command: '검사 명령' } }]; });
  await expect(view(page).locator('.conversation-request')).toContainText('검사 명령');
  await page.evaluate(() => {
    const state = window as any; state.conversationFixture.holdHandoff = true;
    state.handoff = state.conversation.handoffToTerminal();
  });
  await expect.poll(() => page.evaluate(() => (window as any).conversationFixture.calls.filter((entry: any) => entry.args.request?.type === 'handoffToTerminal').length)).toBe(1);
  await expect(view(page)).toBeVisible();
  await expect(view(page).locator('.conversation-request')).toContainText('검사 명령');
  expect(await page.evaluate(() => (window as any).conversationFixture.owners)).toEqual([true]);
  await expect(view(page).locator('.conversation-input')).toHaveValue('인계 뒤에도 보존할 초안');
  await page.evaluate(async () => { const state = window as any; state.conversationFixture.releaseHandoff(); await state.handoff; });
  await expect(view(page)).toBeHidden();
  expect(await page.evaluate(() => (window as any).conversationFixture.owners)).toEqual([true, false]);
  expect(await page.evaluate(() => (window as any).conversationFixture.calls.filter((entry: any) => entry.args.request?.type === 'detach').length)).toBe(0);
  await page.evaluate(async () => { await (window as any).conversation.setVisible(true); });
  await expect(view(page).locator('.conversation-input')).toHaveValue('인계 뒤에도 보존할 초안');
});

test('자동 숨김과 dispose는 명시 인계를 사용하지 않고 엄격 detach를 유지한다', async ({ page }) => {
  await conversation(page);
  await page.evaluate(async () => { const state = window as any; await state.conversation.setVisible(false); await state.conversation.setVisible(true); await state.conversation.dispose(); });
  const releaseTypes = await page.evaluate(() => (window as any).conversationFixture.calls.map((entry: any) => entry.args.request?.type).filter((type: string) => type === 'detach' || type === 'handoffToTerminal'));
  expect(releaseTypes).toEqual(['detach', 'detach']);
});

for (const unknown of [false, true]) {
  test(`명시 인계 거절은 연결·결과 미확인 ${unknown} 상태에서 화면·소유권·초안을 보존한다`, async ({ page }) => {
    await conversation(page);
    await view(page).locator('.conversation-input').fill('거절 뒤 보존할 초안');
    await page.evaluate(unknown => {
      const fixture = (window as any).conversationFixture; fixture.rejectHandoff = true; fixture.unknownResult = unknown;
      fixture.pending = [{ seq: 1, requestId: 'owner-only', method: 'future/ownerOnlyRequest', params: { purpose: '원래 클라이언트 전용' } }];
    }, unknown);
    await expect(view(page).locator('.conversation-request')).toContainText('요청이 시작된 클라이언트에서 확인하세요');
    if (unknown) await expect(view(page).locator('.conversation-input')).toBeDisabled();
    const rejected = await page.evaluate(async () => { try { await (window as any).conversation.handoffToTerminal(); return false; } catch { return true; } });
    expect(rejected).toBe(true);
    await expect(view(page)).toBeVisible();
    await expect(view(page).locator('.conversation-input')).toHaveValue('거절 뒤 보존할 초안');
    await expect(view(page).locator('.conversation-request')).toContainText('원래 클라이언트 전용');
    expect(await page.evaluate(() => (window as any).conversationFixture.owners)).toEqual([true]);
    expect(await page.evaluate(() => (window as any).conversationFixture.calls.filter((entry: any) => entry.args.request?.type === 'handoffToTerminal').length)).toBe(1);
    expect(await page.evaluate(() => (window as any).conversationFixture.calls.filter((entry: any) => entry.args.request?.type === 'detach').length)).toBe(0);
    if (unknown) await expect(view(page).locator('.conversation-input')).toBeDisabled();
    else await expect(view(page).locator('.conversation-input')).toBeEnabled();
  });
}

test('지연 항목 스냅샷은 먼저 표시된 실시간 완료·델타를 덮지 않는다', async ({ page }) => {
  await conversation(page);
  await page.evaluate(() => { const state = window as any; state.conversationFixture.holdItems = true; state.delayedRefresh = state.conversation.refresh(); });
  await expect.poll(() => page.evaluate(() => (window as any).conversationFixture.heldItems.length)).toBe(1);
  await page.evaluate(() => {
    const fixture = (window as any).conversationFixture;
    fixture.queue.push(
      { seq: fixture.nextSeq++, method: 'item/completed', params: { turnId: 'recent-turn', item: { id: 'live-completed', type: 'agentMessage', text: '실시간 완료 원문' } } },
      { seq: fixture.nextSeq++, method: 'item/agentMessage/delta', params: { turnId: 'recent-turn', itemId: 'assistant-a', delta: ' · 최신 델타' } },
    );
  });
  await expect(view(page).locator('[data-item-id="live-completed"]')).toHaveText('도우미실시간 완료 원문');
  await expect(view(page).locator('[data-item-id="assistant-a"]')).toContainText('최신 델타');
  await page.evaluate(async () => { const state = window as any; state.conversationFixture.releaseItems(); await state.delayedRefresh; });
  await expect(view(page).locator('[data-item-id="live-completed"]')).toHaveCount(1);
  await expect(view(page).locator('[data-item-id="assistant-a"]')).toContainText('최신 델타');
  await expect(view(page).locator('[data-item-id="assistant-a"]')).toContainText('const safe = true;');
  await expect(view(page).locator('.conversation-status')).toContainText('이전 이력 응답을 적용하지 않았습니다');
  await page.waitForTimeout(500);
  expect(await page.evaluate(() => (window as any).conversationFixture.calls.filter((entry: any) => entry.args.request?.type === 'items').length)).toBe(2);
});

test('지연 구간 스냅샷은 새 turn 시작을 유휴 상태로 되돌리지 않는다', async ({ page }) => {
  await conversation(page);
  await page.evaluate(() => { const state = window as any; state.conversationFixture.holdPage = true; state.delayedRefresh = state.conversation.refresh(); });
  await expect.poll(() => page.evaluate(() => (window as any).conversationFixture.calls.filter((entry: any) => entry.args.request?.type === 'page').length)).toBe(2);
  await page.evaluate(() => {
    const fixture = (window as any).conversationFixture;
    fixture.queue.push(
      { seq: fixture.nextSeq++, method: 'turn/started', params: { turn: { id: 'newer-turn', status: 'inProgress' } } },
      { seq: fixture.nextSeq++, method: 'item/completed', params: { turnId: 'newer-turn', item: { id: 'newer-item', type: 'agentMessage', text: '새 실행 출력' } } },
    );
  });
  await expect(view(page).locator('[data-item-id="newer-item"]')).toHaveCount(1);
  await page.evaluate(async () => { const state = window as any; state.conversationFixture.releasePage(); await state.delayedRefresh; });
  await expect(view(page).locator('.conversation-stop')).toBeEnabled();
  await expect(view(page).locator('.conversation-send')).toBeDisabled();
  await expect(view(page).locator('[data-item-id="newer-item"]')).toHaveCount(1);
  await expect(view(page).locator('.conversation-status')).toContainText('이전 이력 응답을 적용하지 않았습니다');
  expect(await page.evaluate(() => (window as any).conversationFixture.calls.filter((entry: any) => entry.args.request?.type === 'items').length)).toBe(1);
});

test('여러 항목 조회의 역전은 같은 구간으로 돌아와도 최신 조회 의도를 보존한다', async ({ page }) => {
  await conversation(page);
  async function selectSnapshot(turnId: string, tag: string) {
    await page.evaluate(({ turnId, tag }) => {
      const fixture = (window as any).conversationFixture; fixture.holdItems = true; fixture.historyTag = tag;
      const select = document.querySelector('#conversation-fixture .conversation-turns') as HTMLSelectElement;
      if (![...select.options].some(option => option.value === turnId)) select.add(new Option(turnId, turnId));
      select.value = turnId; select.dispatchEvent(new Event('change', { bubbles: true }));
    }, { turnId, tag });
  }
  await selectSnapshot('recent-turn', '첫 조회 원문');
  await selectSnapshot('older-turn', '다른 구간 원문');
  await selectSnapshot('recent-turn', '최신 조회 원문');
  await expect.poll(() => page.evaluate(() => (window as any).conversationFixture.heldItems.length)).toBe(3);
  await page.evaluate(() => { (window as any).conversationFixture.heldItems[2].release(); });
  await expect(view(page).locator('[data-item-id="assistant-a"]')).toContainText('최신 조회 원문');
  await page.evaluate(() => { (window as any).conversationFixture.heldItems[1].release(); (window as any).conversationFixture.heldItems[0].release(); });
  await page.waitForTimeout(100);
  await expect(view(page).locator('[data-item-id="assistant-a"]')).toHaveCount(1);
  await expect(view(page).locator('[data-item-id="assistant-a"]')).toContainText('최신 조회 원문');
});

test('최근 페이지와 구조화 출력은 안전한 텍스트로 보이고 페이지 단위로 교체된다', async ({ page }, testInfo) => {
  await conversation(page);
  await expect(view(page).locator('.conversation-message')).toHaveCount(3);
  expect(await view(page).locator('.conversation-message').evaluateAll(rows => rows.map(row => row.getAttribute('data-item-id')))).toEqual(['user-a', 'assistant-a', 'tool-a']);
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
  await expect(view(page).locator('.conversation-status')).toContainText('진행 중인 요청을 확인해 주세요');
});

test('승인·선택 질문은 명시 응답만 보내고 poll 동안 선택을 보존한다', async ({ page }) => {
  await conversation(page);
  await page.evaluate(() => {
    const fixture = (window as any).conversationFixture;
    fixture.pending = [
      { seq: 1, method: 'item/commandExecution/requestApproval', requestId: 'approval-1', params: { command: 'npm test' } },
      { seq: 2, method: 'item/tool/requestUserInput', requestId: 'question-1', params: { questions: [{ id: 'scope', header: '범위', question: '범위를 선택하세요', isOther: true, isSecret: false, options: [{ label: '좁게', description: '작은 범위' }, { label: '넓게', description: '전체 범위' }] }] } },
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
  await page.evaluate(() => { (window as any).conversationFixture.largeHistory = true; });
  await view(page).locator('.conversation-refresh').click();
  await expect(view(page).getByRole('button', { name: '전체 내용 보기' })).toBeVisible();
  await view(page).getByRole('button', { name: '전체 내용 보기' }).click();
  await expect(view(page).locator('[data-item-id="large-item"]')).toContainText('첫 원문');
  await view(page).getByRole('button', { name: '다음 내용' }).click();
  await expect(view(page).locator('[data-item-id="large-item"]')).toContainText('마지막 원문');
  await view(page).getByRole('button', { name: '처음부터 보기', exact: true }).click();
  await expect(view(page).locator('[data-item-id="large-item"]')).toContainText('첫 원문');
  const detailRequests = await page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'detail').map((value: any) => value.args.request));
  expect(detailRequests).toEqual([
    { type: 'detail', turnId: 'recent-turn', itemId: 'large-item', offset: 0 },
    { type: 'detail', turnId: 'recent-turn', itemId: 'large-item', offset: 16, snapshotId: 'detail-snapshot-1' },
    { type: 'detail', turnId: 'recent-turn', itemId: 'large-item', offset: 0 },
  ]);
  expect(await page.evaluate(() => (window as any).conversationFixture.detailReads)).toBe(2);
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

for (const mode of ['other-turn', 'missing-id'] as const) {
  test(`실제 이력 항목 구조의 ${mode} 응답은 거부하고 기존 표시를 보존한다`, async ({ page }) => {
    await conversation(page);
    const previous = await view(page).locator('.conversation-message').evaluateAll(rows => rows.map(row => ({ id: row.getAttribute('data-item-id'), text: row.textContent })));
    await page.evaluate(mode => { (window as any).conversationFixture.invalidHistory = mode; }, mode);
    await view(page).locator('.conversation-refresh').click();
    await expect(view(page).locator('.conversation-status')).toContainText('연결과 진행 중인 요청을 확인해 주세요');
    expect(await view(page).locator('.conversation-message').evaluateAll(rows => rows.map(row => ({ id: row.getAttribute('data-item-id'), text: row.textContent })))).toEqual(previous);
    const details = await page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'detail'));
    expect(details).toEqual([]);
  });
}

test('isOther 선택 질문은 직접 입력한 초안을 보존하고 선택값 대신 해당 답을 전송한다', async ({ page }) => {
  await conversation(page);
  await page.evaluate(() => {
    (window as any).conversationFixture.pending = [{ seq: 1, method: 'item/tool/requestUserInput', requestId: 'question-other', params: { questions: [{ id: 'scope', header: '범위', question: '작업 범위를 선택하세요', isOther: true, isSecret: false, options: [{ label: '좁게', description: '작은 범위' }, { label: '넓게', description: '전체 범위' }] }] } }];
  });
  const select = view(page).locator('.conversation-request select');
  await expect(select.locator('option')).toHaveText(['선택하세요', '좁게 — 작은 범위', '넓게 — 전체 범위', '직접 입력']);
  await select.selectOption({ label: '직접 입력' });
  const other = view(page).getByRole('textbox', { name: '작업 범위를 선택하세요 · 직접 입력', exact: true });
  await other.fill('먼저 문서만 살펴보세요');
  await page.waitForTimeout(900); await expect(other).toHaveValue('먼저 문서만 살펴보세요');
  await select.selectOption('좁게'); await expect(other).toBeHidden();
  await select.selectOption({ label: '직접 입력' }); await expect(other).toHaveValue('먼저 문서만 살펴보세요');
  await view(page).getByRole('button', { name: '답변 보내기', exact: true }).click();
  const responses = await page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'respond').map((value: any) => value.args.request));
  expect(responses).toEqual([{ type: 'respond', requestId: 'question-other', result: { answers: { scope: { answers: ['먼저 문서만 살펴보세요'] } } } }]);
});

test('미지원 권한·향후 승인 요청은 응답 없이 원문과 대기를 보존하고 명시 인계만 시도한다', async ({ page }) => {
  await conversation(page);
  const pending = [
    { seq: 1, requestId: 'permission-original', method: 'item/permissions/requestApproval', params: { permissions: { network: { enabled: true } }, reason: '권한 원문 유지', availableDecisions: ['accept', 'decline'] } },
    { seq: 2, requestId: 'future-original', method: 'item/future/requestApproval', params: { futurePayload: { scope: '향후 요청 원문 유지' } } },
  ];
  await page.evaluate(pending => { const fixture = (window as any).conversationFixture; fixture.pending = pending; fixture.rejectHandoff = true; }, pending);
  await expect(view(page).locator('.conversation-request')).toHaveCount(2);
  for (const [index, request] of pending.entries()) {
    const panel = view(page).locator('.conversation-request').nth(index);
    await expect(panel.locator('strong')).toHaveText(request.method);
    await expect(panel.locator('pre')).toHaveText(JSON.stringify(request.params, null, 2));
    await expect(panel.locator('button,input,select')).toHaveCount(0);
    await expect(panel).toContainText('요청이 시작된 클라이언트에서 확인하세요');
  }
  await page.waitForTimeout(850);
  const rejected = await page.evaluate(async () => { try { await (window as any).conversation.handoffToTerminal(); return false; } catch { return true; } });
  expect(rejected).toBe(true);
  await expect(view(page)).toBeVisible();
  await expect(view(page).locator('.conversation-request')).toHaveCount(2);
  expect(await page.evaluate(() => (window as any).conversationFixture.pending)).toEqual(pending);
  expect(await page.evaluate(() => (window as any).conversationFixture.owners)).toEqual([true]);
  await page.evaluate(() => { const fixture = (window as any).conversationFixture; fixture.pending = fixture.pending.slice(0, 1); fixture.rejectHandoff = false; });
  await expect(view(page).locator('.conversation-request')).toHaveCount(1);
  await page.evaluate(async () => { await (window as any).conversation.handoffToTerminal(); });
  await expect(view(page)).toBeHidden();
  const state = await page.evaluate(() => { const fixture = (window as any).conversationFixture; return { pending: fixture.pending, owners: fixture.owners, requests: fixture.calls.map((entry: any) => entry.args.request?.type) }; });
  expect(state.pending).toEqual(pending.slice(0, 1));
  expect(state.owners).toEqual([true, false]);
  expect(state.requests.filter((type: string) => type === 'respond')).toEqual([]);
  expect(state.requests.filter((type: string) => type === 'handoffToTerminal')).toHaveLength(2);
  expect(state.requests.filter((type: string) => type === 'detach')).toEqual([]);
});

for (const method of ['item/commandExecution/requestApproval', 'item/fileChange/requestApproval']) {
test(`${method} 승인 선택지는 필드 부재와 명시적인 빈 값·미지원 값을 구분하고 안전한 교집합만 표시한다`, async ({ page }) => {
  await conversation(page);
  const cases = [
    { key: '필드 없음', present: false, value: null, labels: ['이번만 승인', '거절', '취소'] },
    { key: '명시 null', present: true, value: null, labels: [] },
    { key: '빈 배열', present: true, value: [], labels: [] },
    { key: '미지원 객체', present: true, value: { accept: true }, labels: [] },
    { key: '미지원 문자열', present: true, value: ['futureDecision'], labels: [] },
    { key: '영구 정책 객체', present: true, value: [{ acceptWithExecpolicyAmendment: { execpolicyAmendment: ['command'] } }], labels: [] },
    { key: '혼합 목록', present: true, value: ['decline', 'acceptForSession', { acceptWithExecpolicyAmendment: {} }, 'futureDecision', 'decline'], labels: ['세션 동안 승인', '거절'] },
  ];
  for (const candidate of cases) {
    await page.evaluate(({ candidate, method }) => {
      const params: any = { reason: candidate.key };
      if (candidate.present) params.availableDecisions = candidate.value;
      (window as any).conversationFixture.pending = [{ seq: 1, method, requestId: `approval-${candidate.key}`, params }];
    }, { candidate, method });
    const panel = view(page).locator('.conversation-request'); await expect(panel.locator('pre')).toContainText(candidate.key);
    await expect(panel.locator('button')).toHaveText(candidate.labels);
    if (candidate.labels.length) await panel.getByRole('button', { name: '거절', exact: true }).click();
  }
  const responses = await page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'respond').map((value: any) => value.args.request));
  expect(responses).toEqual([
    { type: 'respond', requestId: 'approval-필드 없음', result: { decision: 'decline' } },
    { type: 'respond', requestId: 'approval-혼합 목록', result: { decision: 'decline' } },
  ]);
});
}

test('같은 스레드의 확정 설정 알림은 현재값을 갱신하고 미전송 모델·추론 선택과 구분한다', async ({ page }) => {
  await conversation(page);
  async function settings(threadId: string, values: Record<string, unknown>) {
    await page.evaluate(({ threadId, values }) => {
      const fixture = (window as any).conversationFixture;
      fixture.queue.push({ seq: fixture.nextSeq++, method: 'thread/settings/updated', params: { threadId, threadSettings: {
        approvalPolicy: 'on-request', approvalsReviewer: 'user', cwd: 'D:\\fixture', modelProvider: 'fixture-provider',
        sandboxPolicy: { type: 'workspaceWrite', writableRoots: ['D:\\fixture'], networkAccess: false, excludeSlashTmp: false, excludeTmpdirEnvVar: false },
        collaborationMode: { mode: 'default', settings: { model: values.model ?? 'runtime-model', reasoning_effort: 'high', developer_instructions: null } },
        ...values,
      } } });
    }, { threadId, values });
  }
  const model = view(page).locator('.conversation-model'); const effort = view(page).locator('.conversation-effort');
  await settings('thread-a', { model: 'server-model-2', effort: 'high' });
  await expect(model).toHaveValue('server-model-2'); await expect(effort).toHaveValue('high');
  const confirmed = view(page).locator('.conversation-current-settings');
  await expect(confirmed).toContainText('server-model-2'); await expect(confirmed).toContainText('high');
  await settings('foreign-thread', { model: 'unrelated-model', effort: 'unrelated-effort' });
  await page.waitForTimeout(500); await expect(model).toHaveValue('server-model-2'); await expect(confirmed).not.toContainText('unrelated');
  await effort.selectOption('medium');
  await settings('thread-a', { model: 'runtime-model', effort: 'low' });
  await expect(confirmed).toContainText('runtime-model'); await expect(model).toHaveValue('server-model-2'); await expect(effort).toHaveValue('medium');
  await settings('thread-a', { model: 'server-model-2', effort: 'high' }); await expect(confirmed).toContainText('high');
  await view(page).locator('.conversation-input').fill('아직 보내지 않은 초안');
  await model.selectOption('runtime-model'); await effort.selectOption('low');
  await expect(confirmed).toContainText('server-model-2'); await expect(confirmed).not.toContainText('low');
  await settings('thread-a', { model: 'server-model-2', effort: 'medium' });
  await expect(confirmed).toContainText('medium'); await expect(model).toHaveValue('runtime-model'); await expect(effort).toHaveValue('low');
  await settings('thread-a', { model: 'server-model-2' });
  await expect(confirmed).toContainText('추론: 미확인'); await expect(confirmed).not.toContainText('high');
  await expect(model).toHaveValue('runtime-model'); await expect(effort).toHaveValue('low');
  await settings('thread-a', { model: 'server-model-2', effort: null });
  await expect(confirmed).toContainText('추론: 미확인');
  await expect(view(page).locator('.conversation-input')).toHaveValue('아직 보내지 않은 초안');
  expect(await page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'send').length)).toBe(0);
});

test('미물질화 이력과 실행 상태 미확인은 빈 대화·유휴로 확정하지 않고 초안을 유지한다', async ({ page }) => {
  await conversation(page, { historyUnavailable: true });
  await expect(view(page).locator('.conversation-placeholder')).toContainText('이력을 아직 조회할 수 없습니다');
  await expect(view(page).locator('.conversation-status')).toContainText('실행 상태 미확인');
  await expect(view(page).locator('.conversation-placeholder')).not.toContainText('표시할 대화가 없습니다');
  const initialPages = await page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'page').length);
  expect(initialPages).toBe(0);
  await view(page).locator('.conversation-input').fill('아직 보내지 않은 작업');
  await expect(view(page).locator('.conversation-send')).toBeEnabled();
  await expect(view(page).locator('.conversation-stop')).toBeDisabled();
  await page.evaluate(() => { (window as any).conversationFixture.historyUnavailable = false; });
  await view(page).locator('.conversation-refresh').click();
  await expect(view(page).locator('.conversation-message')).toHaveCount(3);
  await expect(view(page).locator('.conversation-input')).toHaveValue('아직 보내지 않은 작업');
});

test('정확한 첫 페이지 저장 준비 대기는 미확인을 보존하고 수동 재조회로 복구한다', async ({ page }) => {
  await conversation(page, { historyUnavailable: true });
  await view(page).locator('.conversation-input').fill('저장 준비 중 보존할 초안');
  await view(page).locator('.conversation-attach').click();
  await page.evaluate(() => { (window as any).conversationFixture.pageResult = { historyUnavailable: true, reason: 'unmaterialized' }; });
  await view(page).locator('.conversation-refresh').click();
  await expect(view(page).locator('.conversation-status')).toContainText('이력을 저장할 준비를 기다리고 있습니다');
  await expect(view(page).locator('.conversation-placeholder')).toContainText('비어 있거나 실행 중인 작업이 없다고 확인된 상태가 아닙니다');
  await expect(view(page).locator('.conversation-input')).toHaveValue('저장 준비 중 보존할 초안');
  await expect(view(page).locator('.conversation-attachment')).toHaveCount(2);
  await expect(view(page).locator('.conversation-send')).toBeEnabled();
  await expect(view(page).locator('.conversation-stop')).toBeDisabled();
  await page.evaluate(async () => { const { setLanguage } = await import('/src/i18n.ts'); setLanguage('en'); });
  await expect(view(page).locator('.conversation-status')).toContainText('Waiting for conversation history to be ready for storage');
  await page.evaluate(async () => { const { setLanguage } = await import('/src/i18n.ts'); setLanguage('ko'); });
  const polls = await page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'poll').length);
  await expect.poll(() => page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'poll').length)).toBeGreaterThan(polls + 1);
  const before = await page.evaluate(() => ({ owners: (window as any).conversationFixture.owners, types: (window as any).conversationFixture.calls.map((value: any) => value.args.request?.type) }));
  expect(before.owners).toEqual([true]); expect(before.types.filter((value: string) => value === 'page')).toHaveLength(1); expect(before.types).not.toContain('items'); expect(before.types).not.toContain('send');
  await page.evaluate(() => { const fixture = (window as any).conversationFixture; fixture.pageResult = undefined; fixture.historyUnavailable = false; });
  await view(page).locator('.conversation-refresh').click();
  await expect(view(page).locator('.conversation-message')).toHaveCount(3);
  await expect(view(page).locator('.conversation-status')).toContainText('대화 이력과 실행 상태를 확인했습니다');
  await expect(view(page).locator('.conversation-input')).toHaveValue('저장 준비 중 보존할 초안');
  await expect(view(page).locator('.conversation-attachment')).toHaveCount(2);
});

test('저장 준비 대기는 기존 구간·항목·커서·초안·첨부·실행 소유권을 보존한다', async ({ page }) => {
  await conversation(page);
  await view(page).locator('.conversation-input').fill('첫 작업'); await view(page).locator('.conversation-send').click();
  await expect(view(page).locator('.conversation-stop')).toBeEnabled();
  await view(page).locator('.conversation-input').fill('다음 작업 초안'); await view(page).locator('.conversation-attach').click();
  const before = await view(page).locator('.conversation-messages').innerHTML();
  await page.evaluate(() => { (window as any).conversationFixture.pageResult = { historyUnavailable: true, reason: 'unmaterialized' }; });
  await view(page).locator('.conversation-refresh').click();
  await expect(view(page).locator('.conversation-status')).toContainText('이력을 저장할 준비를 기다리고 있습니다');
  expect(await view(page).locator('.conversation-messages').innerHTML()).toBe(before);
  await expect(view(page).locator('.conversation-turns')).toHaveValue('recent-turn');
  await expect(view(page).locator('.conversation-older')).toBeVisible(); await expect(view(page).locator('.conversation-more')).toBeVisible();
  await expect(view(page).locator('.conversation-input')).toHaveValue('다음 작업 초안'); await expect(view(page).locator('.conversation-attachment')).toHaveCount(2);
  await expect(view(page).locator('.conversation-stop')).toBeEnabled(); await expect(view(page).locator('.conversation-send')).toBeDisabled();
  await view(page).locator('.conversation-more').click();
  await expect(view(page).locator('[data-item-id="earlier-message"]')).toHaveCount(1);
  const calls = await page.evaluate(() => (window as any).conversationFixture.calls);
  expect(calls.filter((value: any) => value.args.request?.type === 'items').at(-1).args.request.cursor).toBe('items-page-2');
  expect(calls.filter((value: any) => value.args.request?.type === 'send')).toHaveLength(1);
  expect(await page.evaluate(() => (window as any).conversationFixture.owners)).toEqual([true]);
});

test('다른 대기 형태와 커서가 있는 페이지는 준비 안내로 위장하지 않는다', async ({ page }) => {
  for (const entry of [
    { value: { historyUnavailable: true, reason: 'other' }, older: false },
    { value: { historyUnavailable: true }, older: false },
    { value: { historyUnavailable: true, reason: 'unmaterialized', data: [] }, older: false },
    { value: { historyUnavailable: true, reason: 'unmaterialized' }, older: true },
  ]) {
    await conversation(page); await view(page).locator('.conversation-input').fill('실패 뒤 보존할 초안');
    await page.evaluate(value => { (window as any).conversationFixture.pageResult = value; }, entry.value);
    await view(page).locator(entry.older ? '.conversation-older' : '.conversation-refresh').click();
    await expect(view(page).locator('.conversation-status')).toContainText('요청을 완료하지 못했습니다');
    await expect(view(page).locator('.conversation-message')).toHaveCount(3); await expect(view(page).locator('.conversation-turns')).toHaveValue('recent-turn');
    await expect(view(page).locator('.conversation-input')).toHaveValue('실패 뒤 보존할 초안');
    if (entry.older) expect(await page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'page').at(-1).args.request.cursor)).toBe('turn-page-2');
  }
});

test('지연 저장 준비 응답은 먼저 받은 실시간 실행과 항목을 덮지 않는다', async ({ page }) => {
  await conversation(page);
  await page.evaluate(() => { const state = window as any; state.conversationFixture.pageResult = { historyUnavailable: true, reason: 'unmaterialized' }; state.conversationFixture.holdPage = true; state.waitingPage = state.conversation.refresh(); });
  await expect.poll(() => page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'page').length)).toBe(2);
  await page.evaluate(() => { const fixture = (window as any).conversationFixture; fixture.queue.push({ seq: fixture.nextSeq++, method: 'turn/started', params: { turn: { id: 'live-turn' } } }, { seq: fixture.nextSeq++, method: 'item/completed', params: { turnId: 'live-turn', item: { id: 'live-waiting-item', type: 'agentMessage', text: '대기 응답보다 새로운 원문' } } }); });
  await expect(view(page).locator('[data-item-id="live-waiting-item"]')).toHaveCount(1);
  await page.evaluate(async () => { const state = window as any; state.conversationFixture.releasePage(); await state.waitingPage; });
  await expect(view(page).locator('.conversation-status')).toContainText('이전 이력 응답을 적용하지 않았습니다');
  await expect(view(page).locator('[data-item-id="live-waiting-item"]')).toHaveCount(1); await expect(view(page).locator('.conversation-stop')).toBeEnabled();
});

test('지연 저장 준비 응답은 앞선 started 응답의 새 실행 상태를 덮지 않는다', async ({ page }) => {
  await conversation(page); await view(page).locator('.conversation-input').fill('새 실행');
  await page.evaluate(() => { (window as any).conversationFixture.holdStart = true; });
  await view(page).locator('.conversation-send').click();
  await expect.poll(() => page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'send').length)).toBe(1);
  await page.evaluate(() => { const state = window as any; state.conversationFixture.pageResult = { historyUnavailable: true, reason: 'unmaterialized' }; state.conversationFixture.holdPage = true; state.waitingPage = state.conversation.refresh(); });
  await expect.poll(() => page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'page').length)).toBe(2);
  await page.evaluate(() => { (window as any).conversationFixture.releaseStart(); });
  await expect(view(page).locator('.conversation-input')).toHaveValue('');
  await page.evaluate(async () => { const state = window as any; state.conversationFixture.releasePage(); await state.waitingPage; });
  await expect(view(page).locator('.conversation-status')).toContainText('이전 이력 응답을 적용하지 않았습니다');
  await expect(view(page).locator('.conversation-stop')).toBeEnabled(); await view(page).locator('.conversation-input').fill('새 실행 뒤 초안'); await expect(view(page).locator('.conversation-send')).toBeDisabled();
});

test('자기 스레드 삭제는 표시 참조와 늦은 조회를 정리하고 세션·첨부 원본과 초안을 보존한다', async ({ page }) => {
  await conversation(page);
  await page.evaluate(() => { (window as any).conversationFixture.largeHistory = true; });
  await view(page).locator('.conversation-refresh').click();
  await view(page).getByRole('button', { name: '전체 내용 보기' }).click();
  await expect(view(page).locator('[data-item-id="large-item"]')).toContainText('첫 원문');
  await view(page).locator('.conversation-input').fill('삭제 뒤에도 보존할 초안');
  await view(page).locator('.conversation-attach').click();
  await page.evaluate(() => {
    const fixture = (window as any).conversationFixture;
    fixture.queue.push({ seq: fixture.nextSeq++, method: 'thread/deleted', params: { threadId: 'foreign-thread' } });
  });
  await page.waitForTimeout(500); await expect(view(page).locator('.conversation-message')).toHaveCount(4);
  await page.evaluate(() => {
    const state = window as any; const fixture = state.conversationFixture;
    fixture.pending = [{ seq: 1, method: 'item/tool/requestUserInput', requestId: 'deleted-question', params: { questions: [{ id: 'scope', header: '범위', question: '삭제 전 질문', options: null, isOther: false, isSecret: false }] } }];
  });
  await expect(view(page).locator('.conversation-request')).toHaveCount(1);
  await page.evaluate(() => { const state = window as any; state.conversationFixture.holdItems = true; state.latePage = state.conversation.refresh(); });
  await expect.poll(() => page.evaluate(() => (window as any).conversationFixture.calls.filter((value: any) => value.args.request?.type === 'items').length)).toBe(3);
  await page.evaluate(() => {
    const fixture = (window as any).conversationFixture; fixture.connected = false; fixture.pending = [];
    fixture.queue.push({ seq: fixture.nextSeq++, method: 'thread/deleted', params: { threadId: 'thread-a' } });
  });
  await expect(view(page).locator('.conversation-status')).toContainText('삭제');
  await page.evaluate(async () => { const state = window as any; state.conversationFixture.releaseItems(); await state.latePage; });
  await expect(view(page).locator('.conversation-message')).toHaveCount(0); await expect(view(page).locator('.conversation-turns option')).toHaveCount(0);
  await expect(view(page).locator('.conversation-request')).toHaveCount(0); await expect(view(page).locator('.conversation-model')).toHaveValue('');
  await expect(view(page).locator('.conversation-current-settings')).not.toContainText('runtime-model');
  await expect(view(page).locator('.conversation-send')).toBeDisabled(); await expect(view(page).locator('.conversation-stop')).toBeDisabled();
  await expect(view(page).locator('.conversation-input')).toHaveValue('삭제 뒤에도 보존할 초안'); await expect(view(page).locator('.conversation-attachment')).toHaveCount(2);
  await expect.poll(() => page.evaluate(() => (window as any).conversationFixture.owners.at(-1))).toBe(false);
  const calls = await page.evaluate(() => (window as any).conversationFixture.calls);
  expect(calls.filter((value: any) => ['session_stop', 'complete_exit', 'cleanup_conversation_attachment'].includes(value.command))).toEqual([]);
  const count = calls.length; await page.waitForTimeout(900); expect(await page.evaluate(() => (window as any).conversationFixture.calls.length)).toBe(count);
});

for (const operation of ['send', 'compact'] as const) for (const replacement of [false, true]) {
  test(`시작 응답보다 먼저 수신한 완료·후속 실행을 보존한다: ${operation} / 후속 ${replacement}`, async ({ page }) => {
    await conversation(page);
    const host = page.locator('#conversation-fixture');
    await host.locator('.conversation-input').fill('응답 순서 시험 초안');
    await page.evaluate(() => { (window as any).conversationFixture.holdStart = true; });
    if (operation === 'send') await host.locator('.conversation-send').click();
    else await page.evaluate(() => { void (window as any).conversation.compact(); });
    await expect.poll(() => page.evaluate(type => (window as any).conversationFixture.calls.some((entry: any) => entry.args?.request?.type === type), operation)).toBe(true);
    await page.evaluate(({ operation, replacement }) => {
      const f = (window as any).conversationFixture; const id = operation === 'send' ? 'sent-turn' : 'compacted-turn';
      f.queue.push({ seq: f.nextSeq++, method: 'turn/started', params: { turn: { id, status: 'inProgress' } } });
      f.queue.push({ seq: f.nextSeq++, method: 'turn/completed', params: { turn: { id, status: 'completed' } } });
      if (replacement) f.queue.push({ seq: f.nextSeq++, method: 'turn/started', params: { turn: { id: 'newer-turn', status: 'inProgress' } } });
      f.queue.push({ seq: f.nextSeq++, method: 'item/completed', params: { turnId: replacement ? 'newer-turn' : id, item: { id: 'activity-marker', type: 'agentMessage', text: '시작 응답 전에 처리한 실행 표식' } } });
    }, { operation, replacement });
    await expect(host.locator('.conversation-messages')).toContainText('시작 응답 전에 처리한 실행 표식');
    await page.evaluate(() => (window as any).conversationFixture.releaseStart());
    await expect(host.locator('.conversation-input')).toBeEnabled();
    if (operation === 'send') await host.locator('.conversation-input').fill('다음 전송 초안');
    else await expect(host.locator('.conversation-input')).toHaveValue('응답 순서 시험 초안');
    if (replacement) {
      await expect(host.locator('.conversation-send')).toBeDisabled();
      await host.locator('.conversation-stop').click();
      await expect.poll(() => page.evaluate(() => (window as any).conversationFixture.calls.filter((entry: any) => entry.args?.request?.type === 'interrupt').at(-1)?.args.request.turnId)).toBe('newer-turn');
    } else {
      await expect(host.locator('.conversation-send')).toBeEnabled();
      await expect(host.locator('.conversation-stop')).toBeDisabled();
    }
  });
}


test('화면 갱신은 변하지 않은 메시지를 이동하지 않고 중간 행의 순서와 초점을 보존한다', async ({ page }) => {
  await conversation(page);
  const tool = view(page).locator('[data-item-id="tool-a"]');
  await tool.locator('summary').click();
  await tool.locator('summary').focus();
  await page.evaluate(() => {
    const state = window as any; const host = document.querySelector('#conversation-fixture .conversation-messages')!;
    state.stableRowMutations = []; state.originalTool = host.querySelector('[data-item-id="tool-a"]');
    state.messageObserver = new MutationObserver(records => {
      for (const record of records) for (const node of [...record.addedNodes, ...record.removedNodes]) {
        if (node instanceof HTMLElement && ['user-a', 'tool-a'].includes(node.dataset.itemId ?? '')) state.stableRowMutations.push(node.dataset.itemId);
      }
    });
    state.messageObserver.observe(host, { childList: true });
  });
  for (let index = 1; index <= 4; index++) {
    await page.evaluate(index => { const f = (window as any).conversationFixture; f.queue.push({ seq: f.nextSeq++, method: 'item/agentMessage/delta', params: { turnId: 'recent-turn', itemId: 'assistant-a', delta: ' 변화-' + index } }); }, index);
    await expect(view(page).locator('[data-item-id="assistant-a"]')).toContainText('변화-' + index);
    expect(await view(page).locator('.conversation-message').evaluateAll(nodes => nodes.map(node => (node as HTMLElement).dataset.itemId))).toEqual(['user-a', 'assistant-a', 'tool-a']);
    await expect(tool.locator('details')).toHaveAttribute('open', '');
    await expect(tool.locator('summary')).toBeFocused();
  }
  expect(await page.evaluate(() => (window as any).stableRowMutations)).toEqual([]);
  expect(await tool.evaluate(node => node === (window as any).originalTool)).toBe(true);
  await page.evaluate(() => (window as any).messageObserver.disconnect());
});

test('이력 재조회는 내용이 같을 때 메시지 DOM을 변경하지 않고 확장한 원문을 보존한다', async ({ page }) => {
  await conversation(page);
  await page.evaluate(() => { (window as any).conversationFixture.largeHistory = true; });
  await view(page).locator('.conversation-refresh').click();
  const detail = view(page).locator('[data-item-id="large-item"]');
  await detail.getByRole('button', { name: '전체 내용 보기', exact: true }).click();
  await expect(detail).toContainText('첫 원문');
  await page.evaluate(() => {
    const state = window as any; state.historyDomChanges = 0;
    state.historyObserver = new MutationObserver(records => { state.historyDomChanges += records.length; });
    state.historyObserver.observe(document.querySelector('#conversation-fixture .conversation-messages')!, { childList: true });
  });
  await view(page).locator('.conversation-refresh').click();
  await expect(view(page).locator('.conversation-refresh')).toBeEnabled();
  await expect(detail).toContainText('첫 원문');
  expect(await page.evaluate(() => (window as any).historyDomChanges)).toBe(0);
  await page.evaluate(() => (window as any).historyObserver.disconnect());
});

test('스트리밍 동안 위로 읽는 위치를 유지하고 하단에 있으면 최신 출력을 따라간다', async ({ page }) => {
  await conversation(page);
  const messages = view(page).locator('.conversation-messages');
  await page.evaluate(() => { const f = (window as any).conversationFixture; f.queue.push({ seq: f.nextSeq++, method: 'item/agentMessage/delta', params: { turnId: 'recent-turn', itemId: 'assistant-a', delta: '\n' + '기존 대화 내용\n'.repeat(160) } }); });
  await expect(view(page).locator('[data-item-id="assistant-a"]')).toContainText('기존 대화 내용');
  await messages.evaluate(node => { node.scrollTop = 0; });
  await page.evaluate(() => { const f = (window as any).conversationFixture; f.queue.push({ seq: f.nextSeq++, method: 'item/completed', params: { turnId: 'recent-turn', item: { id: 'tail-item', type: 'agentMessage', text: '화면 아래에 추가된 최근 내용' } } }); });
  await expect(view(page).locator('[data-item-id="tail-item"]')).toContainText('화면 아래에 추가된 최근 내용');
  expect(await messages.evaluate(node => node.scrollTop)).toBeLessThanOrEqual(1);
  await messages.evaluate(node => { node.scrollTop = node.scrollHeight; });
  await page.evaluate(() => { const f = (window as any).conversationFixture; f.queue.push({ seq: f.nextSeq++, method: 'item/agentMessage/delta', params: { turnId: 'recent-turn', itemId: 'tail-item', delta: '\n' + '추가 출력\n'.repeat(30) } }); });
  await expect(view(page).locator('[data-item-id="tail-item"]')).toContainText('추가 출력');
  await expect.poll(() => messages.evaluate(node => node.scrollHeight - node.scrollTop - node.clientHeight)).toBeLessThanOrEqual(1);
});
