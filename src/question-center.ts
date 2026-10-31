import { locale, onLanguageChange, t } from './i18n';
import './question-center.css';

export interface PendingQuestion {
  id: string; sessionId: string; threadId?: string | null; generation: number; requestId?: string;
  kind: 'question' | 'approval' | 'unsupported'; title?: string; message?: string; sessionTitle?: string;
  canRespond: boolean; responded?: boolean; metadataAvailable?: boolean;
  questions?: { id: string; header?: string; question: string; options?: { label: string; description?: string }[] | null; isOther?: boolean; isSecret?: boolean }[];
  approval?: { options: { id: string; label: string }[] };
}
interface QuestionCenterHost {
  call<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  openSession(sessionId: string): void | Promise<void>;
  button?: HTMLButtonElement;
}
interface Draft { values: Record<string, { selected?: string; text?: string }>; phase?: 'sending' | 'sent' | 'uncertain'; openFailed?: boolean }
const interval = 3_000;
const key = (item: PendingQuestion) => JSON.stringify([item.sessionId, item.threadId ?? '', item.generation, item.requestId ?? item.id]);

function readItems(value: unknown): PendingQuestion[] {
  const items = (value as { items?: unknown })?.items;
  if (!Array.isArray(items) || items.length > 1024) throw new Error(t('질문 목록 형식이 올바르지 않습니다.', 'Invalid question list.'));
  const result = new Map<string, PendingQuestion>();
  for (const item of items as PendingQuestion[]) {
    if (!item || typeof item.id !== 'string' || typeof item.sessionId !== 'string' || !Number.isSafeInteger(item.generation)
      || !['question', 'approval', 'unsupported'].includes(item.kind) || typeof item.canRespond !== 'boolean'
      || (item.threadId != null && typeof item.threadId !== 'string') || (item.requestId !== undefined && typeof item.requestId !== 'string')
      || (item.questions !== undefined && (!Array.isArray(item.questions) || item.questions.some(question => !question || typeof question.id !== 'string' || typeof question.question !== 'string'
        || (question.options != null && (!Array.isArray(question.options) || question.options.some(option => !option || typeof option.label !== 'string'))))))
      || (item.approval !== undefined && (!Array.isArray(item.approval?.options) || item.approval.options.some(option => !option || typeof option.id !== 'string' || typeof option.label !== 'string')))) {
      throw new Error(t('질문 목록 형식이 올바르지 않습니다.', 'Invalid question list.'));
    }
    const known = result.get(key(item));
    // A duplicate may already have been answered by the session view.
    result.set(key(item), known?.responded ? known : item);
  }
  return [...result.values()];
}

export function createQuestionCenter(host: QuestionCenterHost) {
  const button = host.button ?? document.createElement('button');
  button.type = 'button'; button.classList.add('icon-button', 'question-center-button');
  button.innerHTML = '<span aria-hidden="true">?</span><span id="question-count" class="question-count" hidden></span>';
  const badge = button.querySelector<HTMLElement>('#question-count')!;
  const element = document.createElement('dialog'); element.id = 'question-center'; element.setAttribute('aria-labelledby', 'question-center-title');
  element.innerHTML = '<div class="dialog-heading"><h2 id="question-center-title"></h2><button type="button" class="icon-button question-close">×</button></div><p class="question-summary"></p><p class="question-list-status" role="status" aria-live="polite"></p><div class="question-items"></div>';
  const list = element.querySelector<HTMLElement>('.question-items')!;
  const drafts = new Map<string, Draft>();
  const rows = new Map<string, { node: HTMLElement; signature: string }>();
  const abort = new AbortController();
  let items: PendingQuestion[] = []; let readFailed = false; let lastCheckedAt: number | undefined; let disposed = false; let exiting = false; let nativeVisible = true; let epoch = 0;
  let timer: ReturnType<typeof setTimeout> | undefined; let running: Promise<void> | undefined; let lastStarted = -Infinity;
  const responses = new Set<Promise<void>>();
  const text = (tag: string, content: string) => { const node = document.createElement(tag); node.textContent = content; return node; };
  const clearTimer = () => { clearTimeout(timer); timer = undefined; };
  const active = () => !disposed && !exiting && nativeVisible && !document.hidden;

  function render(force = false) {
    if (disposed) return;
    const count = items.filter(item => item.kind !== 'unsupported').length;
    const unavailable = items.filter(item => item.kind === 'unsupported').length;
    const stale = items.some(item => item.metadataAvailable === false);
    const unknown = readFailed && lastCheckedAt === undefined;
    const label = unknown ? t('질문함 · 상태 확인 불가', 'Questions · Status unavailable') : t(`질문함 · 미응답 ${count}건`, `Questions · ${count} unanswered`);
    button.setAttribute('aria-label', label); button.title = label + (stale || readFailed && !unknown ? t(' · 마지막 확인 목록 포함', ' · Includes last confirmed requests') : '');
    button.setAttribute('aria-haspopup', 'dialog'); button.setAttribute('aria-controls', element.id);
    badge.textContent = String(count); badge.hidden = !count;
    element.querySelector('h2')!.textContent = t('질문함', 'Questions');
    element.querySelector('.question-close')!.setAttribute('aria-label', t('질문함 닫기', 'Close questions'));
    element.querySelector('.question-summary')!.textContent = unknown ? t('미응답 상태를 확인할 수 없습니다.', 'Unanswered status is unavailable.')
      : t(`미응답 ${count}건`, `${count} unanswered`) + (unavailable ? t(` · 확인 필요 세션 ${unavailable}개`, ` · ${unavailable} sessions need checking`) : '') + (stale || readFailed ? t(' · 마지막 확인 목록 포함', ' · Includes last confirmed requests') : '');
    const checked = lastCheckedAt === undefined ? '' : t(' 마지막 확인: ', ' Last checked: ') + new Date(lastCheckedAt).toLocaleTimeString(locale(), { hour: '2-digit', minute: '2-digit', second: '2-digit' });
    element.querySelector('.question-list-status')!.textContent = readFailed ? (unknown
      ? t('질문 목록에 연결할 수 없습니다. 잠시 후 다시 확인하거나 원래 세션에서 상태를 확인하세요.', 'Cannot connect to the question list. Try again shortly or check the original session.')
      : t('조회 실패 · 이전 목록을 유지합니다. 잠시 후 다시 확인하거나 원래 세션에서 상태를 확인하세요.', 'Could not refresh · Previous list preserved. Try again shortly or check the original session.') + checked)
      : !items.length ? t('미응답 질문이 없습니다.', 'No unanswered questions.') : '';
    const remaining = new Set(items.map(key));
    for (const [id, row] of rows) if (!remaining.has(id)) { row.node.remove(); rows.delete(id); drafts.delete(id); }
    items.forEach((item, index) => {
      const id = key(item); const draft = drafts.get(id) ?? { values: {} }; drafts.set(id, draft);
      const signature = JSON.stringify([item, draft.phase, draft.openFailed]);
      let row = rows.get(id);
      if (!row || force || row.signature !== signature) {
        const node = buildRow(item, draft, index); row?.node.replaceWith(node); row = { node, signature }; rows.set(id, row);
      }
      // Keep stable rows (and focused inputs) untouched during ordinary polling.
      if (list.children[index] !== row.node) list.insertBefore(row.node, list.children[index] ?? null);
    });
  }

  function buildRow(item: PendingQuestion, draft: Draft, index: number) {
    const row = document.createElement('article'); row.className = 'question-item'; row.dataset.questionKey = key(item);
    const generatedTitle = !item.title || ['사용자 답변 대기', '사용자 승인 대기', '터미널에서 확인할 요청', '터미널에서 답변할 요청'].includes(item.title);
    const heading = text('h3', !generatedTitle ? item.title! : t(item.kind === 'approval' ? '승인 요청' : item.kind === 'unsupported' ? '원문 확인 필요' : '질문', item.kind === 'approval' ? 'Approval request' : item.kind === 'unsupported' ? 'Details unavailable' : 'Question'));
    const session = text('small', item.sessionTitle || item.sessionId); session.className = 'question-session';
    const kind = text('span', item.kind === 'approval' ? t('승인 요청', 'Approval request') : item.kind === 'unsupported' ? t('세션 확인 필요', 'Check session') : t('질문', 'Question')); kind.className = 'question-kind';
    row.append(kind, heading, session);
    if (item.message && item.kind !== 'unsupported') row.append(text('p', item.message));
    if (item.metadataAvailable === false) {
      const notice = text('p', t('연결이 끊겨 마지막 확인 내용을 표시합니다. 원래 세션에서 상태를 확인하세요.', 'Connection lost. Showing the last confirmed details. Check the status in the original session.'));
      notice.className = 'question-metadata-status'; notice.setAttribute('role', 'status'); row.append(notice);
    }
    const locked = !item.canRespond || !item.requestId || !!item.responded || !!draft.phase || exiting;
    const form = document.createElement('form');
    if (item.kind === 'question') for (const [questionIndex, question] of (item.questions ?? []).entries()) {
      const field = document.createElement('fieldset'); field.disabled = locked;
      field.append(text('legend', question.header ? `${question.header} · ${question.question}` : question.question));
      const value = draft.values[question.id] ?? {}; draft.values[question.id] = value;
      const options = question.options ?? [];
      for (const option of options) {
        const label = document.createElement('label'); label.className = 'question-option';
        const input = document.createElement('input'); input.type = 'radio'; input.name = `question-${index}-${questionIndex}`; input.value = option.label; input.checked = value.selected === option.label; input.required = true;
        input.onchange = () => { value.selected = input.value; };
        label.append(input, text('span', option.label)); if (option.description) label.append(text('small', option.description)); field.append(label);
      }
      if (!options.length || question.isOther) {
        if (options.length) {
          const label = document.createElement('label'); label.className = 'question-option';
          const other = document.createElement('input'); other.type = 'radio'; other.name = `question-${index}-${questionIndex}`; other.value = '__other'; other.checked = value.selected === '__other'; other.required = true;
          other.onchange = () => { value.selected = '__other'; }; label.append(other, text('span', t('직접 입력', 'Write an answer'))); field.append(label);
        }
        const input = document.createElement('input'); input.type = question.isSecret ? 'password' : 'text'; input.maxLength = 16_384; input.value = value.text ?? '';
        input.setAttribute('aria-label', question.question); input.autocomplete = 'off'; input.required = !options.length;
        input.oninput = () => { value.text = input.value; if (options.length) { value.selected = '__other'; const radio = field.querySelector<HTMLInputElement>('input[value="__other"]'); if (radio) radio.checked = true; } };
        field.append(input);
      }
      form.append(field);
    }
    const rowStatus = text('p', draft.openFailed ? t('원래 세션을 열 수 없습니다. 세션 목록에서 해당 세션을 선택해 상태를 확인하세요.', 'Cannot open the original session. Select it in the session list to check its status.')
      : draft.phase === 'uncertain' ? t('응답 결과를 확인하지 못했습니다. 자동으로 다시 보내지 않습니다. 원래 세션에서 상태를 확인하세요.', 'Response result is unknown. It will not be resent automatically. Check the original session.')
      : draft.phase === 'sending' ? t('응답 전송 중…', 'Sending response…')
      : item.responded || draft.phase === 'sent' ? t('응답을 보냈습니다. 서버의 해결 확인을 기다립니다.', 'Response sent. Waiting for server confirmation.')
      : item.kind === 'unsupported' ? t('질문 원문을 제공하지 않는 세션입니다. 원래 세션을 열어 확인하고 응답하세요.', 'Question details are unavailable. Open the original session to review and respond.')
      : !item.canRespond ? t('여기서 응답할 수 없습니다. 원래 세션에서 확인하세요.', 'Cannot respond here. Check the original session.') : '');
    rowStatus.className = 'question-response-status'; rowStatus.setAttribute('role', 'status');
    if (item.kind === 'question' && item.questions?.length) {
      const submit = text('button', t('응답 보내기', 'Send answer')) as HTMLButtonElement; submit.type = 'submit'; submit.className = 'primary-button'; submit.disabled = locked;
      form.append(submit); form.onsubmit = event => {
        event.preventDefault(); if (locked || draft.phase) return;
        const answers: Record<string, { answers: string[] }> = {};
        for (const question of item.questions!) {
          const value = draft.values[question.id]; const answer = value?.selected && value.selected !== '__other' ? value.selected : value?.text ?? '';
          if (!answer.trim()) { rowStatus.textContent = t('모든 질문에 응답하세요.', 'Answer every question.'); return; }
          answers[question.id] = { answers: [answer] };
        }
        void respond(item, draft, { answers });
      };
    }
    if (item.kind === 'approval') for (const option of item.approval?.options ?? []) {
      const labels: Record<string, [string, string]> = { accept: ['승인', 'Approve'], decline: ['거절', 'Decline'], cancel: ['취소', 'Cancel'], acceptForSession: ['이 세션에서 승인', 'Approve for this session'] };
      const translated = labels[option.id];
      const label = translated && ['승인', '거절', '취소', '이 세션에서 승인'].includes(option.label) ? t(...translated) : option.label;
      const control = text('button', label) as HTMLButtonElement; control.type = 'button'; control.className = 'quiet-button'; control.disabled = locked;
      control.onclick = () => { if (!draft.phase) void respond(item, draft, { decision: option.id }); }; form.append(control);
    }
    const open = text('button', t('원래 세션 열기 →', 'Open original session →')) as HTMLButtonElement; open.type = 'button'; open.className = 'quiet-button question-open';
    open.onclick = () => { void Promise.resolve().then(() => host.openSession(item.sessionId)).then(() => { if (!disposed) { draft.openFailed = false; element.close(); render(); } }).catch(() => { if (!disposed) { draft.openFailed = true; render(); } }); };
    row.append(form, rowStatus, open); return row;
  }

  function respond(item: PendingQuestion, draft: Draft, result: Record<string, unknown>) {
    if (disposed || exiting || draft.phase || item.responded || !item.canRespond || !item.requestId) return Promise.resolve();
    draft.phase = 'sending'; draft.openFailed = false; render();
    const operation = Promise.resolve().then(() => host.call('respond_pending_question', { sessionId: item.sessionId, generation: item.generation, requestId: item.requestId, ...result })).then(() => {
      draft.phase = 'sent';
    }).catch(() => { draft.phase = 'uncertain'; }).finally(() => {
      responses.delete(operation); if (!disposed) { render(); poll(); }
    });
    responses.add(operation); return operation;
  }
  function schedule() {
    clearTimer(); if (!active() || running) return;
    timer = setTimeout(poll, Math.max(0, lastStarted + interval - performance.now()));
  }
  function poll() {
    clearTimer(); if (!active() || running) return;
    if (performance.now() - lastStarted < interval) { schedule(); return; }
    lastStarted = performance.now(); const token = epoch;
    running = Promise.resolve().then(() => host.call('list_pending_questions')).then(value => {
      if (!active() || token !== epoch) return;
      items = readItems(value); readFailed = false; lastCheckedAt = Date.now(); render();
    }).catch(() => { if (active() && token === epoch) { readFailed = true; render(); } }).finally(() => { running = undefined; schedule(); });
  }
  function visibility() { epoch++; clearTimer(); if (active()) poll(); }
  document.addEventListener('visibilitychange', visibility, { signal: abort.signal });
  button.addEventListener('click', () => { if (!element.open && !disposed) element.showModal(); poll(); }, { signal: abort.signal });
  element.querySelector('.question-close')!.addEventListener('click', () => element.close(), { signal: abort.signal });
  const unlanguage = onLanguageChange(() => render(true));
  render(); poll();
  return {
    button, element,
    refresh: poll,
    setVisible(visible: boolean) { if (nativeVisible !== visible) { nativeVisible = visible; visibility(); } },
    beginExit(): Promise<void> { exiting = true; epoch++; clearTimer(); render(true); return Promise.all([running, ...responses]).then(() => {}); },
    cancelExit() { exiting = false; render(true); poll(); },
    dispose() { if (disposed) return; disposed = true; epoch++; clearTimer(); abort.abort(); unlanguage(); element.close(); element.remove(); element.replaceChildren(); rows.clear(); drafts.clear(); items = []; },
  };
}
