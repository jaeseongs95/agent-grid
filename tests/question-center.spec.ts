import { test, expect, type Page } from '@playwright/test';

const question = {
  id: 'request-one', sessionId: 'session-one', threadId: 'thread-one', generation: 4, requestId: 'r1', kind: 'question',
  title: '<img src=x onerror=alert(1)>', sessionTitle: '첫 세션', message: '작업 범위를 골라 주세요.', canRespond: true,
  questions: [{ id: 'scope', question: '범위는?', options: [{ label: '하나', description: '첫 선택' }, { label: '둘' }], isOther: true }, { id: 'note', question: '덧붙일 내용' }],
};
const approval = {
  id: 'approval-one', sessionId: 'session-two', threadId: 'thread-two', generation: 2, requestId: 'r1', kind: 'approval',
  title: '명령 실행 승인', message: '지정한 명령을 실행할까요?', sessionTitle: '둘째 세션', canRespond: true,
  approval: { options: [{ id: 'accept', label: '이번만 승인' }, { id: 'decline', label: '거절' }] },
};
const unsupported = { id: 'unknown', sessionId: 'terminal', generation: 1, kind: 'unsupported', canRespond: false, sessionTitle: '원문 없는 세션' };

async function fixture(page: Page, items: unknown[] = [question, approval, unsupported], options: { holdPoll?: boolean; holdResponse?: boolean; hidden?: boolean; pollError?: string } = {}) {
  await page.clock.install({ time: new Date('2026-10-30T00:00:00Z') });
  await page.clock.pauseAt(new Date('2026-10-30T00:00:01Z'));
  await page.route('**/question-harness', route => route.fulfill({ contentType: 'text/html', body: '<!doctype html><html><body></body></html>' }));
  await page.goto('/question-harness');
  await page.evaluate(async ({ items, options }) => {
    const modulePath = '/src/question-center.ts'; const { createQuestionCenter } = await import(modulePath);
    const languagePath = '/src/i18n.ts'; const { setLanguage } = await import(languagePath); setLanguage('ko');
    const f = (window as any).questionFixture = {
      items, hidden: options.hidden ?? false, holdPoll: options.holdPoll ?? false, holdResponse: options.holdResponse ?? false,
      calls: [] as { command: string; args?: any; time: number }[], opens: [] as string[], pollError: options.pollError ?? '', responseError: '', openError: '', active: 0, maxActive: 0,
      polls: [] as (() => void)[], responses: [] as (() => void)[], setLanguage,
      releasePoll() { this.polls.shift()?.(); }, releaseResponse() { this.responses.shift()?.(); },
      visibility(value: boolean) { this.hidden = value; document.dispatchEvent(new Event('visibilitychange')); },
    };
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => f.hidden });
    f.widget = createQuestionCenter({ openSession(id: string) { f.opens.push(id); if (f.openError) throw new Error(f.openError); }, async call(command: string, args?: any) {
      f.calls.push({ command, args, time: performance.now() });
      if (command === 'respond_pending_question') {
        if (f.holdResponse) await new Promise<void>(resolve => f.responses.push(resolve));
        if (f.responseError) throw new Error(f.responseError); return {};
      }
      const result = structuredClone({ items: f.items }); const error = f.pollError;
      f.active++; f.maxActive = Math.max(f.maxActive, f.active);
      try { if (f.holdPoll) await new Promise<void>(resolve => f.polls.push(resolve)); if (error) throw new Error(error); return result; }
      finally { f.active--; }
    } });
    document.body.append(f.widget.button, f.widget.element);
  }, { items, options });
}
const open = (page: Page) => page.getByRole('button', { name: /^질문함 ·/ }).click();
const polls = (page: Page) => page.evaluate(() => (window as any).questionFixture.calls.filter((call: any) => call.command === 'list_pending_questions').length);
const responses = (page: Page) => page.evaluate(() => (window as any).questionFixture.calls.filter((call: any) => call.command === 'respond_pending_question'));

test('세션별 질문 항목만 중복 제거해 세고 원문 없는 세션과 승인·질문을 안전하게 구분한다', async ({ page }) => {
  await fixture(page, [question, question, approval, unsupported]);
  await expect(page.locator('#question-count')).toHaveText('2'); await open(page);
  await expect(page.locator('.question-item')).toHaveCount(3);
  await expect(page.locator('.question-summary')).toHaveText('미응답 2건 · 확인 필요 세션 1개');
  await expect(page.getByRole('heading', { name: question.title })).toBeVisible(); await expect(page.locator('.question-item img')).toHaveCount(0);
  await expect(page.locator('.question-item').last()).toContainText('질문 원문을 제공하지 않는 세션');
  expect(await responses(page)).toHaveLength(0);
  await page.getByRole('button', { name: '질문함 닫기' }).click(); await open(page);
  await expect(page.locator('#question-count')).toHaveText('2');
  await page.evaluate(() => (window as any).questionFixture.setLanguage('en'));
  await expect(page.getByRole('heading', { name: 'Questions', exact: true })).toBeVisible();
  await expect(page.locator('.question-summary')).toHaveText('2 unanswered · 1 sessions need checking');
});

test('질문 초안·포커스를 갱신 중 보존하고 전송·responded 상태 뒤에도 서버 해결 전까지 건수를 유지한다', async ({ page }) => {
  await fixture(page, [question], { holdResponse: true }); await open(page);
  await page.getByRole('radio', { name: /^하나/ }).check(); await page.getByRole('textbox', { name: '덧붙일 내용' }).fill('초안');
  await page.clock.runFor(3_000);
  await expect(page.getByRole('textbox', { name: '덧붙일 내용' })).toHaveValue('초안');
  await expect(page.getByRole('textbox', { name: '덧붙일 내용' })).toBeFocused();
  await page.getByRole('button', { name: '응답 보내기' }).click();
  await expect(page.getByRole('button', { name: '응답 보내기' })).toBeDisabled();
  await page.locator('form').dispatchEvent('submit');
  expect(await responses(page)).toEqual([expect.objectContaining({ args: { sessionId: 'session-one', generation: 4, requestId: 'r1', answers: { scope: { answers: ['하나'] }, note: { answers: ['초안'] } } } })]);
  await expect(page.locator('#question-count')).toHaveText('1');
  await page.evaluate(() => { const f = (window as any).questionFixture; f.releaseResponse(); f.items[0].responded = true; });
  await page.clock.runFor(3_000); await expect(page.locator('.question-response-status')).toContainText('서버의 해결 확인');
  await expect(page.locator('#question-count')).toHaveText('1');
  await page.evaluate(() => { (window as any).questionFixture.items = []; }); await page.clock.runFor(3_000);
  await expect(page.locator('.question-item')).toHaveCount(0); await expect(page.locator('#question-count')).toBeHidden();
});

test('승인은 명시한 버튼 값만 전송하고 같은 requestId를 가진 다른 세션·세대와 섞지 않는다', async ({ page }) => {
  await fixture(page, [approval, { ...approval, id: 'another', generation: 3 }, { ...approval, id: 'third', sessionId: 'third' }]); await open(page);
  await expect(page.locator('#question-count')).toHaveText('3');
  await page.locator('.question-item').nth(1).getByRole('button', { name: '거절', exact: true }).click();
  expect(await responses(page)).toEqual([expect.objectContaining({ args: { sessionId: 'session-two', generation: 3, requestId: 'r1', decision: 'decline' } })]);
  await expect(page.locator('.question-item').first().getByRole('button', { name: '거절', exact: true })).toBeEnabled();
  await expect(page.locator('.question-item').nth(1).getByRole('button', { name: '거절', exact: true })).toBeDisabled();
});

test('응답 불명과 목록 오류·잘못된 응답은 질문을 지우거나 자동 재전송하지 않는다', async ({ page }) => {
  await fixture(page, [approval]); await open(page);
  await page.evaluate(() => { (window as any).questionFixture.responseError = 'socket closed'; });
  await page.getByRole('button', { name: '이번만 승인' }).click();
  await expect(page.locator('.question-response-status')).toContainText('응답 결과를 확인하지 못했습니다');
  await expect(page.getByRole('button', { name: '이번만 승인' })).toBeDisabled();
  await page.evaluate(() => { (window as any).questionFixture.pollError = '조회 중단'; }); await page.clock.runFor(3_000);
  await expect(page.locator('.question-list-status')).toContainText('이전 목록을 유지'); await expect(page.locator('#question-count')).toHaveText('1');
  await page.evaluate(() => { const f = (window as any).questionFixture; f.pollError = ''; f.items = [{ ...f.items[0], generation: null }]; }); await page.clock.runFor(3_000);
  await expect(page.locator('.question-list-status')).toContainText('이전 목록을 유지'); await expect(page.locator('#question-count')).toHaveText('1');
  expect(await responses(page)).toHaveLength(1);
});

test('지원하지 않는 세션은 원래 세션으로만 이동하며 알려진 미응답으로 세지 않는다', async ({ page }) => {
  await fixture(page, [unsupported]); await open(page);
  await expect(page.locator('#question-count')).toBeHidden(); await expect(page.locator('form button')).toHaveCount(0);
  await page.getByRole('button', { name: '원래 세션 열기 →' }).click();
  expect(await page.evaluate(() => (window as any).questionFixture.opens)).toEqual(['terminal']);
  await expect(page.locator('#question-center')).not.toBeVisible(); expect(await responses(page)).toHaveLength(0);
});

test('조회 한 개·최소 간격·숨김 중단·해제 후 늦은 결과 무시를 지킨다', async ({ page }) => {
  await fixture(page, [question], { holdPoll: true }); await expect.poll(() => polls(page)).toBe(1);
  await page.evaluate(() => { const f = (window as any).questionFixture; f.widget.refresh(); f.widget.refresh(); });
  await page.clock.runFor(30_000); expect(await polls(page)).toBe(1);
  await page.evaluate(() => { const f = (window as any).questionFixture; f.widget.setVisible(false); f.releasePoll(); f.holdPoll = false; });
  await page.clock.runFor(30_000); expect(await polls(page)).toBe(1); await expect(page.locator('#question-count')).toBeHidden();
  await page.evaluate(() => (window as any).questionFixture.widget.setVisible(true)); await expect(page.locator('#question-count')).toHaveText('1');
  await page.evaluate(() => { const f = (window as any).questionFixture; f.widget.refresh(); f.widget.refresh(); });
  expect(await polls(page)).toBe(2); await page.clock.runFor(3_000); await expect.poll(() => polls(page)).toBe(3);
  expect(await page.evaluate(() => (window as any).questionFixture.maxActive)).toBe(1);
  await page.evaluate(() => { const f = (window as any).questionFixture; f.holdPoll = true; }); await page.clock.runFor(3_000);
  await page.evaluate(() => { const f = (window as any).questionFixture; f.widget.dispose(); f.releasePoll(); }); await page.clock.runFor(30_000);
  expect(await polls(page)).toBe(4); await expect(page.locator('#question-center')).toHaveCount(0);
});

test('최초 숨김은 조회하지 않고 종료는 진행 응답을 기다린다', async ({ page }) => {
  await fixture(page, [approval], { hidden: true, holdResponse: true }); await page.clock.runFor(30_000); expect(await polls(page)).toBe(0);
  await page.evaluate(() => (window as any).questionFixture.visibility(false)); await expect(page.locator('#question-count')).toHaveText('1'); await open(page);
  await page.getByRole('button', { name: '이번만 승인' }).click();
  await page.evaluate(() => { const f = (window as any).questionFixture; f.exited = false; f.widget.beginExit().then(() => { f.exited = true; }); });
  await page.clock.runFor(30_000); expect(await polls(page)).toBe(1); expect(await page.evaluate(() => (window as any).questionFixture.exited)).toBe(false);
  await page.evaluate(() => (window as any).questionFixture.releaseResponse()); await expect.poll(() => page.evaluate(() => (window as any).questionFixture.exited)).toBe(true);
  await page.evaluate(() => (window as any).questionFixture.widget.cancelExit()); await expect.poll(() => polls(page)).toBe(2);
  await expect(page.locator('#question-count')).toHaveText('1'); expect(await responses(page)).toHaveLength(1);
});

test('끊긴 연결의 마지막 확인 목록은 건수를 유지하고 재연결하면 경고만 해제한다', async ({ page }) => {
  await fixture(page, [{ ...approval, metadataAvailable: false, canRespond: false }]); await open(page);
  await expect(page.locator('#question-count')).toHaveText('1');
  await expect(page.locator('.question-summary')).toHaveText('미응답 1건 · 마지막 확인 목록 포함');
  await expect(page.locator('.question-center-button')).toHaveAttribute('title', /마지막 확인 목록 포함/);
  await expect(page.locator('.question-metadata-status')).toHaveText('연결이 끊겨 마지막 확인 내용을 표시합니다. 원래 세션에서 상태를 확인하세요.');
  await expect(page.getByRole('button', { name: '이번만 승인' })).toBeDisabled();
  await page.evaluate(() => (window as any).questionFixture.setLanguage('en'));
  await expect(page.locator('.question-metadata-status')).toContainText('Connection lost. Showing the last confirmed details.');
  await expect(page.locator('.question-center-button')).toHaveAttribute('title', /Includes last confirmed requests/);
  await page.evaluate(() => { const f = (window as any).questionFixture; f.items[0].metadataAvailable = true; f.items[0].canRespond = true; });
  await page.clock.runFor(3_000);
  await expect(page.locator('#question-count')).toHaveText('1');
  await expect(page.locator('.question-summary')).toHaveText('1 unanswered');
  await expect(page.locator('.question-metadata-status')).toHaveCount(0);
  await expect(page.locator('.question-center-button')).not.toHaveAttribute('title', /Includes last confirmed requests/);
  await expect(page.getByRole('button', { name: '이번만 승인' })).toBeEnabled();
  expect(await responses(page)).toHaveLength(0);
});

test('조회·응답·세션 이동 실패의 기술정보는 전체 화면과 속성에 노출하지 않고 원문과 마지막 확인 건수는 보존한다', async ({ page }) => {
  const fault = 'D:\\private-fixture\\runtime.ts fake-token-question-849 INTERNAL_OBSERVER_FAILURE';
  const source = '도구 본문 원문: D:\\user-work\\keep-this-file.ts';
  await fixture(page, [{ ...question, message: source }, approval]); await open(page);
  const hiddenFault = async () => {
    const dom = await page.locator('html').evaluate(element => element.outerHTML);
    for (const value of ['private-fixture', 'fake-token-question-849', 'INTERNAL_OBSERVER_FAILURE']) expect(dom).not.toContain(value);
    await expect(page.locator('.question-item').first()).toContainText(source);
  };
  await page.evaluate(fault => { const f = (window as any).questionFixture; f.responseError = fault; f.pollError = fault; f.openError = fault; }, fault);
  await page.getByRole('button', { name: '거절', exact: true }).click();
  await expect(page.locator('.question-item').nth(1).getByRole('status')).toContainText('응답 결과를 확인하지 못했습니다');
  await expect(page.getByRole('button', { name: '거절', exact: true })).toBeDisabled();
  await page.locator('.question-item').first().getByRole('button', { name: '원래 세션 열기 →' }).click();
  await expect(page.locator('.question-item').first().getByRole('status')).toContainText('세션 목록에서 해당 세션을 선택');
  await page.clock.runFor(3_000);
  await expect(page.locator('.question-list-status')).toContainText('이전 목록을 유지');
  await expect(page.locator('.question-list-status')).toContainText('마지막 확인:');
  const lastChecked = (await page.locator('.question-list-status').textContent())!.split('마지막 확인:')[1];
  await expect(page.locator('#question-count')).toHaveText('2'); await hiddenFault();
  await page.clock.runFor(3_000);
  expect((await page.locator('.question-list-status').textContent())!.split('마지막 확인:')[1]).toBe(lastChecked);
  await page.evaluate(() => (window as any).questionFixture.setLanguage('en'));
  await expect(page.locator('.question-list-status')).toContainText('Last checked:');
  await expect(page.locator('.question-item').first().getByRole('status')).toContainText('Select it in the session list');
  await expect(page.locator('.question-item').nth(1).getByRole('status')).toContainText('Response result is unknown'); await hiddenFault();
  await page.evaluate(() => { const f = (window as any).questionFixture; f.pollError = ''; f.items[1].responded = true; });
  await page.clock.runFor(3_000); await expect(page.locator('.question-list-status')).toHaveText('');
  await expect(page.locator('#question-count')).toHaveText('2'); await hiddenFault();
  await page.evaluate(() => { const f = (window as any).questionFixture; f.items.pop(); }); await page.clock.runFor(3_000);
  await expect(page.locator('#question-count')).toHaveText('1'); await hiddenFault();
  expect(await responses(page)).toEqual([expect.objectContaining({ args: { sessionId: 'session-two', generation: 2, requestId: 'r1', decision: 'decline' } })]);
});

test('첫 조회 연결 실패는 확인된 0건과 구분하고 재조회 성공 뒤에만 빈 목록으로 표시한다', async ({ page }) => {
  const fault = 'D:\\unknown-fixture\\server.exe fake-token-initial-357 INTERNAL_CONNECT_ERROR';
  await fixture(page, [], { pollError: fault }); await open(page);
  await expect(page.locator('.question-summary')).toHaveText('미응답 상태를 확인할 수 없습니다.');
  await expect(page.locator('.question-center-button')).toHaveAttribute('title', '질문함 · 상태 확인 불가');
  await expect(page.locator('.question-list-status')).toContainText('질문 목록에 연결할 수 없습니다');
  await expect(page.locator('.question-list-status')).not.toContainText('미응답 질문이 없습니다');
  for (const language of ['en', 'ko']) {
    await page.evaluate(language => (window as any).questionFixture.setLanguage(language), language);
    const dom = await page.locator('html').evaluate(element => element.outerHTML);
    for (const value of ['unknown-fixture', 'fake-token-initial-357', 'INTERNAL_CONNECT_ERROR']) expect(dom).not.toContain(value);
  }
  await page.evaluate(() => { (window as any).questionFixture.pollError = ''; }); await page.clock.runFor(3_000);
  await expect(page.locator('.question-summary')).toHaveText('미응답 0건');
  await expect(page.locator('.question-list-status')).toHaveText('미응답 질문이 없습니다.');
  await expect(page.locator('.question-center-button')).toHaveAttribute('title', '질문함 · 미응답 0건');
  expect(await responses(page)).toHaveLength(0);
});
