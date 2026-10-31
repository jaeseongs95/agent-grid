import { expect, test, type Page } from '@playwright/test';

/**
 * 시험 계획: 공개 createCronManager + 실제 DOM, cron_request만 대체한다.
 * CRON-OWNER: 세션·세대·jobId와 늦은 응답 격리.
 * CRON-ONCE/POPUP: 중복 금지, close/reopen 이후 unknown, 같은 requestId 정합화.
 * CRON-EFFECT: pause/remove/cancel 각각의 요청과 수락 후 안내.
 * CRON-SCHEDULE: 반복 초·현지시각의 epoch 변환 및 잘못된 입력 무전송.
 * CRON-BOUND: 100행·512자와 숨김/종료 뒤 polling 중단.
 * 기존 playwright.config.ts와 context-controls.spec.ts 브라우저 fixture 방식을 사용한다.
 * 실제 CLI 소유권·원자성·타이머 실행은 mock 밖이므로 이 파일로 입증하지 않는다.
 * 실행/원시 근거 수집은 통합 담당 소유이며 작성 시점 NOT_RUN이다.
 */
test.use({ timezoneId: 'Asia/Seoul' });

async function fixture(page: Page, options: { unsupported?: boolean; jobs?: number } = {}) {
  await page.goto('/');
  await page.clock.install({ time: new Date('2030-01-02T03:04:05Z') });
  await page.evaluate(async ({ unsupported, jobs }) => {
    const state = window as any;
    const { createCronManager } = await import('/src/cron-manager.ts');
    document.querySelector('#cron-dialog')?.remove();
    const data = state.cronFixture = {
      session: { id: 'cron-session-a', generation: 9, title: '예약 세션 A', active: true, threadId: 'thread-a' },
      calls: [] as any[], notices: [] as any[], completed: 0,
      holdRead: false, holdMutation: false, releaseRead: () => {}, releaseMutation: () => {},
      failRead: false, failMutation: false, reply: 'applied',
      snapshot: {
        supported: !unsupported, connected: !unsupported, scope: 'cliLifetime',
        supportsExpectedTurnInterrupt: true,
        jobs: Array.from({ length: jobs ?? 0 }, (_, index) => ({
          id: String(index + 1), promptPreview: '저장된 작업',
          schedule: { type: 'every', seconds: 120 }, state: 'running',
          paused: false, removed: false,
        })),
      } as any,
    };
    state.cronManager = createCronManager({
      async call(command: string, args: any) {
        if (command !== 'cron_request') throw Error('예약 이외의 호출');
        data.calls.push(structuredClone({ command, ...args }));
        const reading = args.request.type === 'list';
        const result = reading ? structuredClone(data.snapshot) : { status: data.reply };
        const failure = reading ? data.failRead : data.failMutation;
        try {
          if (reading && data.holdRead) {
            data.holdRead = false;
            await new Promise<void>(resolve => { data.releaseRead = resolve; });
          }
          if (!reading && data.holdMutation) {
            data.holdMutation = false;
            await new Promise<void>(resolve => { data.releaseMutation = resolve; });
          }
          if (failure) throw Error('PRIVATE_TOKEN_PATH_NEVER_REAL');
          return result;
        } finally { data.completed++; }
      },
      notify(ko: string, error: boolean, en: string) { data.notices.push({ ko, error, en }); },
    });
    await state.cronManager.open(data.session);
  }, options);
}

async function refresh(page: Page) {
  await page.locator('#cron-dialog .cron-refresh').click();
  await expect(page.locator('#cron-dialog')).toHaveAttribute('aria-busy', 'false');
}

async function mutations(page: Page) {
  return page.evaluate(() => (window as any).cronFixture.calls.filter((call: any) => call.request.type !== 'list'));
}

async function submit(page: Page) {
  await page.locator('#cron-dialog .cron-form').evaluate((form: HTMLFormElement) =>
    form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })));
}

test('예약 미지원과 확인된 빈 목록을 구분하고 변경을 차단한다', async ({ page }) => {
  await fixture(page, { unsupported: true });
  await expect(page.locator('.cron-status')).toContainText('지원하지 않습니다');
  await expect(page.locator('.cron-jobs')).toContainText('아직 확인하지 못했습니다');
  await expect(page.locator('.cron-add')).toBeDisabled();
  await submit(page);
  expect(await mutations(page)).toEqual([]);

  await page.evaluate(() => Object.assign((window as any).cronFixture.snapshot, { supported: true, connected: true }));
  await refresh(page);
  await expect(page.locator('.cron-jobs')).toHaveText('이 세션에 등록된 예약이 없습니다.');
  await expect(page.locator('.cron-add')).toBeEnabled();
  await expect(page.locator('.cron-lifetime')).toContainText('CLI가 종료되면 예약도 끝납니다');
});

test('예약 미리보기는 유니코드 512문자를 보존하고 서로게이트 쌍을 자르지 않는다', async ({ page }) => {
  await fixture(page, { jobs: 1 });
  const expected = '가'.repeat(511) + '🐈';
  await page.evaluate(value => { (window as any).cronFixture.snapshot.jobs[0].promptPreview = value; }, expected + '뒤');
  await refresh(page);
  await expect(page.locator('.cron-preview')).toHaveText(expected);
  expect(await page.locator('.cron-preview').evaluate(node => Array.from(node.textContent ?? '').length)).toBe(512);
});

test('현재 연결에서 안전한 중단 지원이 확인돼야 현재 회차 중단을 허용한다', async ({ page }) => {
  await fixture(page, { jobs: 1 });
  const cancel = page.getByRole('button', { name: '현재 회차 중단', exact: true });
  await expect(cancel).toBeEnabled();
  for (const cap of ['missing', false, null, 'true', 1, true] as const) {
    await page.evaluate(cap => {
      const snapshot = (window as any).cronFixture.snapshot;
      if (cap === 'missing') delete snapshot.supportsExpectedTurnInterrupt;
      else snapshot.supportsExpectedTurnInterrupt = cap;
    }, cap);
    await refresh(page);
    if (cap === true) await expect(cancel).toBeEnabled();
    else {
      await expect(cancel).toBeDisabled();
      await cancel.evaluate((button: HTMLButtonElement) => button.dispatchEvent(new MouseEvent('click', { bubbles: true })));
      expect(await mutations(page)).toEqual([]);
    }
  }
});

test('현재 회차 중단만 미지원이면 예약 관리 전체가 미지원이라고 안내하지 않는다', async ({ page }) => {
  await fixture(page, { jobs: 1 });
  await page.locator('.cron-prompt').fill('보존할 예약 초안');
  await page.evaluate(() => { (window as any).cronFixture.reply = 'unsupported'; });
  await page.getByRole('button', { name: '현재 회차 중단', exact: true }).click();
  await expect(page.locator('.cron-status')).toContainText('안전한 작업 중단');
  await expect(page.locator('.cron-status')).not.toContainText('예약 관리를 지원하지 않습니다');
  await expect(page.locator('.cron-prompt')).toHaveValue('보존할 예약 초안');
  expect((await mutations(page))[0].request.type).toBe('cancel');
});

test('반복은 초 단위로, 단발 현지 시각은 정확한 epoch로 원래 세션에 전송한다', async ({ page }) => {
  await fixture(page);
  await page.locator('.cron-prompt').fill('  원문 작업\n둘째 줄  ');
  await page.locator('.cron-amount').fill('2');
  await page.locator('.cron-unit').selectOption('60');
  await page.locator('.cron-add').click();
  await expect.poll(async () => (await mutations(page)).length).toBe(1);
  const first = (await mutations(page))[0];
  expect(first).toEqual({
    command: 'cron_request', id: 'cron-session-a', generation: 9,
    request: { type: 'add', requestId: expect.any(String), schedule: { type: 'every', seconds: 120 }, prompt: '  원문 작업\n둘째 줄  ' },
  });
  expect(first.request.requestId).toMatch(/^[0-9a-f-]{36}$/);
  await expect(page.locator('.cron-prompt')).toHaveValue('');
  await refresh(page);
  await page.locator('.cron-kind').selectOption('at');
  await page.locator('.cron-time').fill('2032-06-07T08:09:10');
  await page.locator('.cron-prompt').fill('단발 작업');
  await page.locator('.cron-add').click();
  await expect.poll(async () => (await mutations(page)).length).toBe(2);
  const second = (await mutations(page))[1];
  expect(second.request.schedule).toEqual({ type: 'at', atUnixMs: Date.parse('2032-06-06T23:09:10.000Z') });
  expect(second.request.requestId).not.toBe(first.request.requestId);
  expect(second.id).toBe('cron-session-a');
  expect(second.generation).toBe(9);
});

test('잘못된 간격·지난 시각·빈 작업·UTF-8 상한 초과를 등록하지 않는다', async ({ page }) => {
  await fixture(page);
  await page.locator('.cron-prompt').fill('작업');
  for (const amount of ['0', '-1', '1.5', '9007199254740992']) {
    await page.locator('.cron-amount').fill(amount);
    await submit(page);
  }
  await page.locator('.cron-kind').selectOption('at');
  await page.locator('.cron-time').fill('2029-01-01T00:00');
  await submit(page);
  await page.locator('.cron-kind').selectOption('every');
  await page.locator('.cron-amount').fill('1');
  await page.locator('.cron-prompt').fill('   ');
  await submit(page);
  await page.locator('.cron-prompt').fill('가'.repeat(21846)); // 65,538 UTF-8 바이트
  await submit(page);
  expect(await mutations(page)).toEqual([]);
  await expect(page.locator('.cron-error')).toBeVisible();
});

test('pause·resume·remove·cancel은 같은 문자열 jobId를 보내고 실행 취소와 예약 삭제를 구분한다', async ({ page }) => {
  await fixture(page, { jobs: 1 });
  const jobId = '18446744073709551615';
  await page.evaluate(jobId => { (window as any).cronFixture.snapshot.jobs[0].id = jobId; }, jobId);
  await refresh(page);
  await page.getByRole('button', { name: '일시정지', exact: true }).click();
  await expect.poll(async () => (await mutations(page)).length).toBe(1);
  await page.evaluate(() => { (window as any).cronFixture.snapshot.jobs[0].paused = true; });
  await refresh(page);
  await page.getByRole('button', { name: '재개', exact: true }).click();
  await expect.poll(async () => (await mutations(page)).length).toBe(2);
  await refresh(page);
  await page.getByRole('button', { name: '예약 삭제', exact: true }).click();
  await expect(page.locator('.cron-status')).toContainText('이미 실행 중인 회차는 계속됩니다');
  await page.evaluate(() => { (window as any).cronFixture.snapshot.jobs[0].removed = true; });
  await refresh(page);
  await expect(page.getByRole('button', { name: '예약 삭제', exact: true })).toBeDisabled();
  await page.getByRole('button', { name: '현재 회차 중단', exact: true }).click();
  await expect(page.locator('.cron-status')).toContainText('종료 확인을 기다립니다');
  await expect(page.locator('.cron-status')).toContainText('반복 예약은 유지됩니다');
  await expect(page.locator('.cron-job strong')).toContainText('실행 중');
  const calls = await mutations(page);
  expect(calls.map((call: any) => ({ id: call.id, generation: call.generation, type: call.request.type, jobId: call.request.jobId }))).toEqual(
    ['pause', 'resume', 'remove', 'cancel'].map(type => ({ id: 'cron-session-a', generation: 9, type, jobId })),
  );
  expect(new Set(calls.map((call: any) => call.request.requestId)).size).toBe(4);
});

test('중복 전송을 막고 결과 불명은 close/reopen 뒤에도 같은 requestId 정합화만 해제한다', async ({ page }) => {
  await fixture(page);
  await page.evaluate(() => { const d = (window as any).cronFixture; d.holdMutation = true; d.reply = 'unknown'; });
  await page.locator('.cron-prompt').fill('중복 금지 작업');
  await page.locator('.cron-add').click();
  await expect.poll(async () => (await mutations(page)).length).toBe(1);
  await submit(page);
  await expect(page.locator('.cron-add')).toBeDisabled();
  await page.locator('.cron-close').click();
  await expect(page.locator('#cron-dialog')).not.toBeVisible();
  await page.evaluate(() => (window as any).cronFixture.releaseMutation());
  await expect.poll(() => page.evaluate(() => (window as any).cronFixture.completed)).toBe(2);
  await page.evaluate(async () => { const s = window as any; await s.cronManager.open(s.cronFixture.session); });
  await expect(page.locator('.cron-unknown')).toBeVisible();
  await expect(page.locator('.cron-add')).toBeDisabled();

  const requestId = (await mutations(page))[0].request.requestId;
  for (const ack of [{ mutationState: 'clear' }, { mutationState: 'clear', reconciledRequestId: 'different-request' }]) {
    await page.evaluate(ack => { const d = (window as any).cronFixture; delete d.snapshot.reconciledRequestId; Object.assign(d.snapshot, ack); }, ack);
    await refresh(page);
    await expect(page.locator('.cron-add')).toBeDisabled();
  }
  await page.clock.fastForward(15001);
  expect((await mutations(page)).length).toBe(1);
  await page.evaluate(requestId => Object.assign((window as any).cronFixture.snapshot, { mutationState: 'clear', reconciledRequestId: requestId }), requestId);
  await refresh(page);
  await expect(page.locator('.cron-add')).toBeEnabled();
  await expect(page.locator('.cron-unknown')).toBeHidden();
  expect((await mutations(page)).length).toBe(1);
});

test('늦은 목록과 원래 세션 종료가 새 세션·세대 화면을 되돌리지 않는다', async ({ page }) => {
  await fixture(page);
  await page.evaluate(() => {
    const d = (window as any).cronFixture; d.holdRead = true;
    d.snapshot.jobs = [{ id: '99', promptPreview: '이전 세대 비밀 작업', schedule: { type: 'every', seconds: 60 }, state: 'ready', paused: false, removed: false }];
  });
  await page.locator('.cron-refresh').click();
  await expect(page.locator('#cron-dialog')).toHaveAttribute('aria-busy', 'true');
  await page.locator('.cron-refresh').evaluate((button: HTMLButtonElement) => button.click());
  expect(await page.evaluate(() => (window as any).cronFixture.calls.length)).toBe(2);
  await page.locator('.cron-close').click();
  await expect(page.locator('#cron-dialog')).not.toBeVisible();
  await page.evaluate(() => (window as any).cronFixture.releaseRead());
  await expect.poll(() => page.evaluate(() => (window as any).cronFixture.completed)).toBe(2);
  await page.evaluate(async () => {
    const s = window as any; s.cronFixture.snapshot.jobs = [];
    s.cronFixture.session = { id: 'cron-session-b', generation: 12, title: '예약 세션 B', active: true, threadId: 'thread-b' };
    await s.cronManager.open(s.cronFixture.session);
  });
  await expect(page.locator('.cron-session')).toHaveText('세션: 예약 세션 B');
  await expect(page.locator('.cron-jobs')).not.toContainText('이전 세대 비밀 작업');
  await page.evaluate(() => (window as any).cronManager.updateSessions([{ ...(window as any).cronFixture.session, generation: 13 }]));
  await expect(page.locator('.cron-add')).toBeDisabled();
  await expect(page.locator('.cron-status')).toContainText('원래 세션이 종료되거나 바뀌었습니다');
  await submit(page);
  expect(await mutations(page)).toEqual([]);
  const last = await page.evaluate(() => (window as any).cronFixture.calls.at(-1));
  expect(last).toMatchObject({ id: 'cron-session-b', generation: 12, request: { type: 'list' } });
});

test('목록은 최대 100행·미리보기 512자로 제한하고 넘침을 빈 목록으로 숨기지 않는다', async ({ page }) => {
  await fixture(page, { jobs: 101 });
  await page.evaluate(() => { (window as any).cronFixture.snapshot.jobs[0].promptPreview = '<script>비밀</script>' + '가'.repeat(600); });
  await refresh(page);
  await expect(page.locator('.cron-job')).toHaveCount(100);
  await expect(page.locator('.cron-job[data-job-id="101"]')).toHaveCount(0);
  await expect(page.locator('.cron-preview').first()).toHaveText(('<script>비밀</script>' + '가'.repeat(600)).slice(0, 512));
  await expect(page.locator('.cron-preview script')).toHaveCount(0);
  await expect(page.locator('.cron-limit')).toBeVisible();
  await page.evaluate(() => { const s = (window as any).cronFixture.snapshot; s.jobs = s.jobs.slice(0, 100); });
  await refresh(page);
  await expect(page.locator('.cron-limit')).toBeHidden();
  await page.evaluate(() => { (window as any).cronFixture.snapshot.hasMore = true; });
  await refresh(page);
  await expect(page.locator('.cron-limit')).toBeVisible();
});

test('조회·변경 실패 원문을 노출하지 않고 마지막 목록·초안을 보존한다', async ({ page }) => {
  await fixture(page, { jobs: 1 });
  await page.locator('.cron-prompt').fill('보존할 초안');
  await page.evaluate(() => { (window as any).cronFixture.failRead = true; });
  await refresh(page);
  await expect(page.locator('.cron-error')).toBeVisible();
  await expect(page.locator('.cron-job')).toHaveCount(1);
  await expect(page.locator('.cron-add')).toBeDisabled();
  await page.evaluate(() => { const d = (window as any).cronFixture; d.failRead = false; d.failMutation = true; });
  await refresh(page);
  await page.locator('.cron-add').click();
  await expect(page.locator('.cron-unknown')).toBeVisible();
  await expect(page.locator('.cron-prompt')).toHaveValue('보존할 초안');
  const exposed = await page.evaluate(() => ({ html: document.querySelector('#cron-dialog')!.outerHTML, notices: (window as any).cronFixture.notices }));
  expect(JSON.stringify(exposed)).not.toContain('PRIVATE_TOKEN_PATH_NEVER_REAL');
  expect((await mutations(page)).length).toBe(1);
});

test('숨김·닫힘·종료 뒤 polling을 멈추고 다시 보일 때만 조회를 재개한다', async ({ page }) => {
  await fixture(page);
  await page.clock.fastForward(5001);
  await expect.poll(() => page.evaluate(() => (window as any).cronFixture.calls.length)).toBe(2);
  await page.evaluate(() => (window as any).cronManager.setVisible(false));
  const hiddenCount = await page.evaluate(() => (window as any).cronFixture.calls.length);
  await page.clock.fastForward(20001);
  expect(await page.evaluate(() => (window as any).cronFixture.calls.length)).toBe(hiddenCount);
  await page.evaluate(() => (window as any).cronManager.setVisible(true));
  await expect.poll(() => page.evaluate(() => (window as any).cronFixture.calls.length)).toBe(hiddenCount + 1);
  await page.locator('.cron-close').click();
  await expect(page.locator('#cron-dialog')).not.toBeVisible();
  await page.clock.fastForward(20001);
  expect(await page.evaluate(() => (window as any).cronFixture.calls.length)).toBe(hiddenCount + 1);
  await page.evaluate(async () => { const s = window as any; await s.cronManager.open(s.cronFixture.session); await s.cronManager.beginExit(); });
  const exitCount = await page.evaluate(() => (window as any).cronFixture.calls.length);
  await page.clock.fastForward(20001);
  expect(await page.evaluate(() => (window as any).cronFixture.calls.length)).toBe(exitCount);
  expect(await mutations(page)).toEqual([]);
});


test('형식이 잘못된 예약 목록은 마지막 목록을 덮거나 변경 기능을 열지 않는다', async ({ page }) => {
  await fixture(page, { jobs: 1 });
  const validJob = await page.evaluate(() => structuredClone((window as any).cronFixture.snapshot.jobs[0]));
  for (const jobs of [
    [{ ...validJob, state: 'toString' }], [{ ...validJob, id: '18446744073709551616' }],
    [validJob, validJob], [{ ...validJob, skipped: -1 }], [{ ...validJob, nextRunAt: {} }],
  ]) {
    await page.evaluate(jobs => { (window as any).cronFixture.snapshot.jobs = jobs; }, jobs);
    await refresh(page);
    await expect(page.locator('.cron-error')).toBeVisible();
    await expect(page.locator('.cron-job')).toHaveCount(1);
    await expect(page.locator('.cron-add')).toBeDisabled();
  }
  expect(await mutations(page)).toEqual([]);
});

test('같은 이벤트에서 창을 닫으면 대기 중 등록 요청을 보내지 않는다', async ({ page }) => {
  await fixture(page);
  await page.locator('.cron-prompt').fill('창을 닫기 전 작성한 작업');
  await page.evaluate(() => {
    document.querySelector('.cron-form')!.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }));
    (document.querySelector('#cron-dialog') as HTMLDialogElement).close();
  });
  await expect(page.locator('#cron-dialog')).not.toBeVisible();
  expect(await mutations(page)).toEqual([]);
});


test('동일 예약 조회는 행 자식 변경 없이 같은 버튼 초점과 스크롤을 보존한다', async ({ page }) => {
  await fixture(page, { jobs: 100 });
  await page.evaluate(() => {
    const state = window as any;
    state.cronDom = { rows: Array.from(document.querySelectorAll('.cron-job')), records: 0 };
    state.cronDom.observer = new MutationObserver(records => { state.cronDom.records += records.filter(record => record.type === 'childList').length; });
    state.cronDom.observer.observe(document.querySelector('#cron-dialog')!, { childList: true, subtree: true });
    (document.querySelector('.cron-job[data-job-id="20"] button') as HTMLButtonElement).focus();
    const dialog = document.querySelector('#cron-dialog')!;
    dialog.scrollTop = 120;
    state.cronDom.scroll = dialog.scrollTop;
  });
  await page.clock.fastForward(5001);
  await expect.poll(() => page.evaluate(() => (window as any).cronFixture.calls.length)).toBe(2);
  await expect(page.locator('#cron-dialog')).toHaveAttribute('aria-busy', 'false');
  expect(await page.evaluate(() => {
    const state = window as any;
    return {
      records: state.cronDom.records,
      sameRows: Array.from(document.querySelectorAll('.cron-job')).every((row, index) => row === state.cronDom.rows[index]),
      sameFocus: document.activeElement === state.cronDom.rows[19].querySelector('button'),
      sameScroll: document.querySelector('#cron-dialog')!.scrollTop === state.cronDom.scroll,
    };
  })).toEqual({ records: 0, sameRows: true, sameFocus: true, sameScroll: true });
  expect(await mutations(page)).toEqual([]);
});

test('바뀐 예약 행만 교체하고 삭제·추가·재정렬에서는 나머지 행을 재사용한다', async ({ page }) => {
  await fixture(page, { jobs: 3 });
  await page.evaluate(() => {
    const state = window as any;
    state.cronRows = Array.from(document.querySelectorAll('.cron-job'));
    state.cronFixture.snapshot.jobs[1].promptPreview = '두 번째 작업 수정';
  });
  await refresh(page);
  expect(await page.evaluate(() => {
    const state = window as any;
    const rows = Array.from(document.querySelectorAll('.cron-job'));
    return rows.map((row, index) => row === state.cronRows[index]);
  })).toEqual([true, false, true]);
  await expect(page.locator('.cron-job[data-job-id="2"] .cron-preview')).toHaveText('두 번째 작업 수정');
  await page.evaluate(() => {
    const state = window as any;
    state.cronRemoved = new WeakRef(document.querySelector('.cron-job[data-job-id="2"]')!);
    const [one, , three] = state.cronFixture.snapshot.jobs;
    state.cronFixture.snapshot.jobs = [three, one, { ...one, id: '4', promptPreview: '새 예약' }];
  });
  await refresh(page);
  expect(await page.locator('.cron-job').evaluateAll(rows => rows.map(row => (row as HTMLElement).dataset.jobId))).toEqual(['3', '1', '4']);
  expect(await page.evaluate(() => {
    const state = window as any;
    return {
      sameOne: document.querySelector('.cron-job[data-job-id="1"]') === state.cronRows[0],
      sameThree: document.querySelector('.cron-job[data-job-id="3"]') === state.cronRows[2],
      removed: state.cronRemoved.deref()?.isConnected === false,
    };
  })).toEqual({ sameOne: true, sameThree: true, removed: true });
  await expect(page.locator('.cron-job')).toHaveCount(3);
});

test('언어 변경은 예약 행 라벨을 갱신하고 같은 언어의 다음 조회는 재생성하지 않는다', async ({ page }) => {
  await fixture(page, { jobs: 1 });
  await page.evaluate(async () => { const { setLanguage } = await import('/src/i18n.ts'); setLanguage('en'); });
  await expect(page.locator('.cron-job strong')).toContainText('Running');
  await expect(page.locator('.cron-job small')).toContainText('Every 120 seconds');
  await expect(page.locator('.cron-job button').first()).toHaveText('Pause');
  await page.evaluate(() => { (window as any).cronEnglishRow = document.querySelector('.cron-job'); });
  await page.locator('.cron-refresh').click();
  await expect(page.locator('#cron-dialog')).toHaveAttribute('aria-busy', 'false');
  expect(await page.evaluate(() => document.querySelector('.cron-job') === (window as any).cronEnglishRow)).toBe(true);
  await page.evaluate(async () => { const { setLanguage } = await import('/src/i18n.ts'); setLanguage('ko'); });
  await expect(page.locator('.cron-job strong')).toContainText('실행 중');
  await expect(page.locator('.cron-job button').first()).toHaveText('일시정지');
});

test('조회 중 포인터·키보드 조작이 있으면 이전 예약 버튼으로 초점을 되돌리지 않는다', async ({ page }) => {
  for (const interaction of ['pointer', 'keyboard']) {
    await fixture(page, { jobs: 1 });
    await page.evaluate(() => {
      const state = window as any;
      state.cronFixture.holdRead = true;
      state.cronFocused = document.querySelector('.cron-job button');
      state.cronFocused.focus();
    });
    await page.clock.fastForward(5001);
    await expect.poll(() => page.evaluate(() => (window as any).cronFixture.calls.length)).toBe(2);
    if (interaction === 'pointer') await page.locator('#cron-title').click();
    else await page.keyboard.press('Tab');
    await page.evaluate(() => (window as any).cronFixture.releaseRead());
    await expect(page.locator('#cron-dialog')).toHaveAttribute('aria-busy', 'false');
    expect(await page.evaluate(() => document.activeElement === (window as any).cronFocused)).toBe(false);
  }
});

test('닫힌 팝업은 예약 행 DOM과 캐시 참조를 해제해 행이 수집될 수 있다', async ({ page }) => {
  await fixture(page, { jobs: 100 });
  await page.evaluate(() => { (window as any).cronClosedRow = new WeakRef(document.querySelector('.cron-job[data-job-id="50"]')!); });
  await page.locator('.cron-close').click();
  await expect(page.locator('.cron-job')).toHaveCount(0);
  const gc = await page.context().newCDPSession(page);
  try {
    await expect.poll(async () => {
      await gc.send('HeapProfiler.collectGarbage');
      return page.evaluate(() => (window as any).cronClosedRow.deref() === undefined);
    }).toBe(true);
  } finally { await gc.detach(); }
  await page.evaluate(async () => { const state = window as any; await state.cronManager.open(state.cronFixture.session); });
  await expect(page.locator('.cron-job')).toHaveCount(100);
});


test('자동 목록 조회 중에도 예약 초안의 한글 입력과 초점을 유지한다', async ({ page }) => {
  await fixture(page);
  const prompt = page.locator('.cron-prompt');
  await prompt.fill('입력 중인 예약 초안');
  await prompt.focus();
  await page.evaluate(() => { (window as any).cronFixture.holdRead = true; });
  await page.clock.fastForward(5001);
  await expect.poll(() => page.evaluate(() => (window as any).cronFixture.calls.length)).toBe(2);
  await expect(prompt).toBeEnabled();
  await expect(prompt).toBeFocused();
  await prompt.press('End');
  await prompt.pressSequentially(' 계속 작성');
  await expect(prompt).toHaveValue('입력 중인 예약 초안 계속 작성');
  await expect(page.locator('.cron-add')).toBeDisabled();
  await page.evaluate(() => (window as any).cronFixture.releaseRead());
  await expect(page.locator('#cron-dialog')).toHaveAttribute('aria-busy', 'false');
  await expect(prompt).toBeFocused();
  await expect(prompt).toHaveValue('입력 중인 예약 초안 계속 작성');
  await expect(page.locator('.cron-add')).toBeEnabled();
  expect(await mutations(page)).toEqual([]);
});
