import { bindAttribute, bindText, getLanguage, onLanguageChange, t } from './i18n';
import './cron-manager.css';
import { MAX_WORKSPACE_SESSIONS } from './model';

interface Session { id: string; generation: number; title: string; active: boolean; threadId?: string | null }
type Schedule = { type: 'every'; seconds: number } | { type: 'at'; atUnixMs: number };
interface Job { id: string; promptPreview: string; schedule: Schedule; state: string; paused: boolean; removed: boolean; nextRunAt?: number | null; skipped?: number }
interface List { supported: boolean; connected: boolean; scope: 'cliLifetime'; jobs: Job[]; hasMore?: boolean; mutationState?: 'clear' | 'unknown'; reconciledRequestId?: string; supportsExpectedTurnInterrupt?: boolean }
interface Reply { status: 'applied' | 'notApplied' | 'unknown' | 'unsupported' }
interface Host { call<T>(command: string, args: Record<string, unknown>): Promise<T>; notify(ko: string, error?: boolean, en?: string): void }
const LIMIT = 100;
const states: Record<string, [string, string]> = { ready: ['예약됨', 'Scheduled'], pending: ['실행 접수 확인 중', 'Checking admission'], running: ['실행 중', 'Running'], finished: ['종료됨', 'Finished'], unknown: ['실행 상태 미확인', 'Execution unknown'], blockedUnsupported: ['실행 환경 미지원', 'Runtime unsupported'], blockedConnection: ['연결 확인 필요', 'Connection required'] };
function preview(value: string) {
  let text = ''; let count = 0;
  for (const character of value) { if (count++ === 512) break; text += character; }
  return text;
}

export function createCronManager(host: Host) {
  const dialog = document.createElement('dialog'); dialog.id = 'cron-dialog'; dialog.className = 'cron-dialog'; dialog.setAttribute('aria-labelledby', 'cron-title');
  dialog.innerHTML = `<div class="dialog-heading"><h2 id="cron-title"></h2><button class="cron-close icon-button" type="button">×</button></div>
    <p class="cron-session"></p><p class="cron-lifetime"></p>
    <div class="cron-toolbar"><button class="cron-refresh quiet-button" type="button"></button><span class="cron-status" role="status" aria-live="polite"></span></div>
    <p class="cron-error inline-error" role="alert" hidden></p><p class="cron-unknown" role="status" hidden></p>
    <section class="cron-jobs" aria-label="예약 작업"></section><p class="cron-limit" hidden></p>
    <form class="cron-form"><h3 class="cron-new-label"></h3><label><span class="cron-prompt-label"></span><textarea class="cron-prompt" rows="3" maxlength="65536" required></textarea></label>
    <div class="cron-schedule"><label><span class="cron-schedule-label"></span><select class="cron-kind"><option value="every"></option><option value="at"></option></select></label>
    <label class="cron-every"><span class="cron-interval-label"></span><span class="cron-interval-input"><input class="cron-amount" type="number" min="1" step="1" value="5" required><select class="cron-unit"><option value="1"></option><option value="60" selected></option><option value="3600"></option></select></span></label>
    <label class="cron-at" hidden><span class="cron-at-label"></span><input class="cron-time" type="datetime-local" step="1"></label></div>
    <p class="cron-timing-help"></p><button class="cron-add primary-button" type="submit"></button></form>`;
  document.body.append(dialog);
  const $ = <T extends HTMLElement = HTMLElement>(selector: string) => dialog.querySelector<T>(selector)!;
  const prompt = $<HTMLTextAreaElement>('.cron-prompt');
  const kind = $<HTMLSelectElement>('.cron-kind');
  const amount = $<HTMLInputElement>('.cron-amount');
  const unit = $<HTMLSelectElement>('.cron-unit');
  const time = $<HTMLInputElement>('.cron-time');
  const form = $<HTMLFormElement>('.cron-form');
  let selected: Session | undefined; let available = false; let visible = true; let exiting = false;
  let epoch = 0; let request: Promise<void> | undefined; let timer: ReturnType<typeof setTimeout> | undefined;
  let jobs: Job[] = []; let supported = false; let connected = false; let fresh = false; let hasMore = false;
  let canCancel = false;
  let knownList = false; let readingRequest = false;
  const rows = new Map<string, { signature: string; row: HTMLElement }>();
  const empty = document.createElement('p');
  const unresolved = new Map<string, string>();
  const key = (session: Session) => `${session.id}:${session.generation}:${session.threadId ?? ''}`;
  const uncertain = () => !!selected && unresolved.has(key(selected));
  const live = (version: number) => dialog.open && selected && version === epoch && available && !exiting && visible && !document.hidden;
  const rpc = <T>(session: Session, value: Record<string, unknown>) => host.call<T>('cron_request', { id: session.id, generation: session.generation, request: value });
  function error(ko: string, en: string) { bindText($('.cron-error'), ko, en); $('.cron-error').hidden = false; }
  function status(ko: string, en: string) { if ($('.cron-status').textContent !== t(ko, en)) bindText($('.cron-status'), ko, en); }
  function controls() {
    const eligible = !!selected && available && supported && connected && fresh && !exiting && !uncertain() && unresolved.size < MAX_WORKSPACE_SESSIONS;
    const ready = eligible && !request;
    // 목록 읽기는 초안 편집을 중단하지 않는다. 실제 변경 전송만 단일 요청으로 제한한다.
    for (const element of form.querySelectorAll<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement>('input,select,textarea')) element.disabled = !eligible || (!!request && !readingRequest);
    $<HTMLButtonElement>('.cron-add').disabled = !ready;
    $<HTMLButtonElement>('.cron-refresh').disabled = !selected || !available || !!request || exiting;
    for (const button of dialog.querySelectorAll<HTMLButtonElement>('.cron-job button')) button.disabled = !ready || button.dataset.allowed !== 'true';
    $('.cron-unknown').hidden = !uncertain();
    dialog.setAttribute('aria-busy', String(!!request));
  }
  function formatTime(value: number | null | undefined) { return typeof value === 'number' && Number.isSafeInteger(value) && Number.isFinite(new Date(value).getTime()) ? new Date(value).toLocaleString(document.documentElement.lang === 'en' ? 'en-US' : 'ko-KR') : t('미확인', 'Unknown'); }
  function createRow(job: Job) {
    const row = document.createElement('article'); row.className = 'cron-job'; row.dataset.jobId = job.id;
    const title = document.createElement('strong'); title.textContent = `#${job.id} · ${job.paused ? t('일시정지 · ', 'Paused · ') : ''}${t(...states[job.state])}${job.removed ? t(' · 삭제됨, 실행 종료 확인 중', ' · Removed, waiting for execution to end') : ''}`;
    const text = document.createElement('p'); text.className = 'cron-preview'; text.textContent = preview(job.promptPreview);
    const detail = document.createElement('small');
    const schedule = job.schedule.type === 'every' ? t(`${job.schedule.seconds}초 간격`, `Every ${job.schedule.seconds} seconds`) : formatTime(job.schedule.atUnixMs);
    detail.textContent = `${schedule} · ${t('다음 실행', 'Next run')}: ${formatTime(job.nextRunAt)}${Number.isSafeInteger(job.skipped) && job.skipped! >= 0 ? ` · ${t('건너뜀', 'Skipped')}: ${job.skipped}` : ''}`;
    const actions = document.createElement('div'); actions.className = 'cron-job-actions';
    function action(type: string, ko: string, en: string, allowed: boolean) { const button = document.createElement('button'); button.type = 'button'; button.className = 'quiet-button'; bindText(button, ko, en); button.dataset.allowed = String(allowed); button.onclick = () => void mutate({ type, jobId: job.id }); actions.append(button); }
    const settled = !['unknown', 'blockedUnsupported', 'blockedConnection', 'pending'].includes(job.state);
    action(job.paused ? 'resume' : 'pause', job.paused ? '재개' : '일시정지', job.paused ? 'Resume' : 'Pause', !job.removed && settled && (job.paused ? job.state !== 'finished' || job.schedule.type === 'every' : job.state !== 'finished'));
    action('remove', '예약 삭제', 'Remove schedule', !job.removed);
    action('cancel', '현재 회차 중단', 'Cancel current run', canCancel && job.state === 'running');
    row.append(title, text, detail, actions); return row;
  }
  function render() {
    const ko = selected ? `세션: ${selected.title}` : ''; const en = selected ? `Session: ${selected.title}` : '';
    if ($('.cron-session').textContent !== t(ko, en)) bindText($('.cron-session'), ko, en);
    const list = $('.cron-jobs');
    const ids = new Set(jobs.map(job => job.id));
    for (const id of rows.keys()) if (!ids.has(id)) rows.delete(id);
    const desired = jobs.map(job => {
      const signature = JSON.stringify([getLanguage(), canCancel, job]);
      let entry = rows.get(job.id);
      if (!entry || entry.signature !== signature) { entry = { signature, row: createRow(job) }; rows.set(job.id, entry); }
      return entry.row;
    });
    if (!desired.length) {
      const ko = knownList && fresh ? '이 세션에 등록된 예약이 없습니다.' : '예약 목록을 아직 확인하지 못했습니다.';
      const en = knownList && fresh ? 'This session has no scheduled jobs.' : 'The job list has not been confirmed.';
      if (empty.textContent !== t(ko, en)) bindText(empty, ko, en);
      desired.push(empty);
    }
    const retained = new Set<Node>(desired);
    for (const child of Array.from(list.childNodes)) if (!retained.has(child)) child.remove();
    let position = list.firstChild;
    for (const row of desired) { if (row !== position) list.insertBefore(row, position); position = row.nextSibling; }
    if (!available) rows.clear();
    $('.cron-limit').hidden = !hasMore;
    controls();
  }
  function validate(value: List) {
    if (!value || typeof value.supported !== 'boolean' || typeof value.connected !== 'boolean' || value.scope !== 'cliLifetime' || !Array.isArray(value.jobs)) throw Error('예약 응답 형식 불일치');
    if (Object.hasOwn(value, 'supportsExpectedTurnInterrupt') && typeof value.supportsExpectedTurnInterrupt !== 'boolean') throw Error('예약 중단 지원 정보 형식 불일치');
    const ids = new Set<string>();
    for (const job of value.jobs.slice(0, LIMIT)) { if (ids.has(job?.id)) throw Error('중복 예약 식별자'); ids.add(job?.id); }
    if (value.jobs.slice(0, LIMIT).some(job => !job || typeof job.id !== 'string' || !/^[1-9][0-9]{0,19}$/.test(job.id) || BigInt(job.id) > 18446744073709551615n || typeof job.promptPreview !== 'string' || typeof job.state !== 'string' || !Object.hasOwn(states, job.state) || typeof job.paused !== 'boolean' || typeof job.removed !== 'boolean' || (job.nextRunAt != null && (!Number.isSafeInteger(job.nextRunAt) || job.nextRunAt <= 0)) || (job.skipped != null && (!Number.isSafeInteger(job.skipped) || job.skipped < 0)) || !job.schedule || (job.schedule.type === 'every' ? !Number.isSafeInteger(job.schedule.seconds) || job.schedule.seconds <= 0 : job.schedule.type !== 'at' || !Number.isSafeInteger(job.schedule.atUnixMs) || job.schedule.atUnixMs <= 0))) throw Error('예약 목록 형식 불일치');
  }
  function scheduleRefresh() { clearTimeout(timer); if (dialog.open && visible && !document.hidden && !exiting && available) timer = setTimeout(() => void refresh(), 5000); }
  function track(work: () => Promise<void>, preserveRowFocus = false) {
    if (request || exiting) return Promise.resolve();
    clearTimeout(timer);
    const focused = preserveRowFocus && document.activeElement instanceof HTMLButtonElement && dialog.contains(document.activeElement) && document.activeElement.closest('.cron-job') ? new WeakRef(document.activeElement as HTMLButtonElement) : undefined;
    let interacted = false;
    const interaction = () => { interacted = true; };
    if (focused) { document.addEventListener('pointerdown', interaction, true); document.addEventListener('keydown', interaction, true); }
    readingRequest = preserveRowFocus;
    request = Promise.resolve().then(work).finally(() => {
      if (focused) { document.removeEventListener('pointerdown', interaction, true); document.removeEventListener('keydown', interaction, true); }
      request = undefined; readingRequest = false; controls();
      const button = focused?.deref();
      if (button && !interacted && button.isConnected && !button.disabled && dialog.open && visible && !document.hidden && !exiting && (document.activeElement === document.body || document.activeElement === dialog)) button.focus({ preventScroll: true });
      scheduleRefresh();
    }); controls(); return request;
  }
  function refresh() {
    const session = selected; const version = epoch; if (!session || !available || !dialog.open || !visible || document.hidden) return Promise.resolve();
    return track(async () => {
      if (!live(version) || !visible || document.hidden) return;
      try {
        const value = await rpc<List>(session, { type: 'list' });
        if (!live(version)) return;
        validate(value); supported = value.supported; connected = value.connected; fresh = supported && connected;
        canCancel = fresh && value.supportsExpectedTurnInterrupt === true;
        if (fresh) { jobs = value.jobs.slice(0, LIMIT).map(job => ({ id: job.id, promptPreview: preview(job.promptPreview), state: job.state, paused: job.paused, removed: job.removed, nextRunAt: job.nextRunAt, skipped: job.skipped, schedule: job.schedule.type === 'every' ? { type: 'every', seconds: job.schedule.seconds } : { type: 'at', atUnixMs: job.schedule.atUnixMs } })); hasMore = value.hasMore === true || value.jobs.length > LIMIT; knownList = true; }
        if (fresh && value.mutationState === 'clear' && value.reconciledRequestId === unresolved.get(key(session))) unresolved.delete(key(session));
        $('.cron-error').hidden = true;
        if (!supported) status('이 실행 환경은 예약 관리 연결을 지원하지 않습니다.', 'This runtime does not support scheduled job management.');
        else if (!connected) status('예약 실행기 연결을 확인하지 못했습니다. 마지막 목록은 현재 상태가 아닐 수 있습니다.', 'The scheduler connection is unavailable. The last list may be out of date.');
        else status('예약 목록을 확인했습니다.', 'Job list confirmed.');
        render();
      } catch {
        if (live(version)) { fresh = false; canCancel = false; error('예약 목록을 불러오지 못했습니다. 연결을 확인하고 새로고침하세요. 마지막 목록은 보존합니다.', 'Could not load scheduled jobs. Check the connection and refresh. The last list is preserved.'); controls(); }
      }
    }, true);
  }
  function mutate(value: Record<string, unknown>) {
    const session = selected; const version = epoch;
    if (!session || !live(version) || !supported || !connected || !fresh || request || uncertain() || unresolved.size >= MAX_WORKSPACE_SESSIONS) return Promise.resolve();
    if (value.type === 'cancel' && !canCancel) return Promise.resolve();
    const requestId = crypto.randomUUID();
    return track(async () => {
      if (!live(version)) return;
      unresolved.set(key(session), requestId);
      try {
        const result = await rpc<Reply>(session, { ...value, requestId });
        if (result.status !== 'unknown' && ['applied', 'notApplied', 'unsupported'].includes(result.status)) unresolved.delete(key(session));
        else throw Error('예약 결과 미확인');
        if (!live(version)) {
          if (result.status === 'applied') host.notify('예약 요청이 처리되었습니다. 해당 세션에서 목록을 확인하세요.', false, 'The schedule request was applied. Check that session’s job list.');
          return;
        }
        fresh = false;
        if (result.status === 'applied') {
          if (value.type === 'add') prompt.value = '';
          status(value.type === 'remove' ? '예약을 삭제했습니다. 이미 실행 중인 회차는 계속됩니다.' : value.type === 'cancel' ? '현재 회차 중단을 요청했습니다. 종료 확인을 기다립니다. 반복 예약은 유지됩니다.' : '예약 요청이 처리되었습니다. 새로고침하여 상태를 확인하세요.', value.type === 'remove' ? 'Schedule removed. An accepted run continues.' : value.type === 'cancel' ? 'Cancellation requested. Waiting for the current run to end. Recurring schedules are preserved.' : 'The schedule request was applied. Refresh to confirm its state.');
        } else if (result.status === 'unsupported') {
          if (value.type === 'cancel') { canCancel = false; status('현재 연결에서 안전한 작업 중단을 지원하는지 확인되지 않아 요청을 보내지 않았습니다.', 'Safe cancellation is not confirmed for this connection. No request was sent.'); }
          else { supported = false; status('이 실행 환경은 예약 관리를 지원하지 않습니다.', 'This runtime does not support job management.'); }
        }
        else error('예약을 변경하지 못했습니다. 새로고침하여 상태를 확인하세요. 작성 중인 내용은 보존합니다.', 'The schedule was not changed. Refresh to check its state. Your draft is preserved.');
      } catch {
        const ko = '예약 요청 결과를 확인하지 못했습니다. 다시 등록하지 말고 목록을 새로고침하여 확인하세요. 자동으로 재전송하지 않습니다.';
        const en = 'The schedule request result is unknown. Refresh the list before registering again. It will not be retried automatically.';
        if (live(version)) { fresh = false; error(ko, en); }
        else host.notify(ko, true, en);
      }
    });
  }
  function mode() { $('.cron-every').hidden = kind.value !== 'every'; $('.cron-at').hidden = kind.value !== 'at'; amount.required = kind.value === 'every'; time.required = kind.value === 'at'; }
  form.addEventListener('submit', event => {
    event.preventDefault();
    if (!prompt.value.trim() || prompt.value.includes('\0') || new TextEncoder().encode(prompt.value).byteLength > 64 * 1024) { error('작업 내용을 입력하세요. UTF-8 기준 64 KiB까지 등록할 수 있습니다.', 'Enter the task prompt, up to 64 KiB in UTF-8.'); return; }
    let schedule: Schedule;
    if (kind.value === 'every') { const seconds = Number(amount.value) * Number(unit.value); if (!Number.isSafeInteger(Number(amount.value)) || !Number.isSafeInteger(seconds) || seconds <= 0) { error('반복 간격은 0보다 큰 정수로 입력하세요.', 'Enter a positive whole-number interval.'); return; } schedule = { type: 'every', seconds }; }
    else if (kind.value === 'at') { const atUnixMs = new Date(time.value).getTime(); if (!Number.isSafeInteger(atUnixMs) || atUnixMs <= Date.now()) { error('미래의 실행 시각을 선택하세요.', 'Choose a future execution time.'); return; } schedule = { type: 'at', atUnixMs }; }
    else return;
    void mutate({ type: 'add', schedule, prompt: prompt.value });
  });
  function clear() { epoch++; selected = undefined; available = false; jobs = []; rows.clear(); supported = false; connected = false; fresh = false; canCancel = false; hasMore = false; knownList = false; clearTimeout(timer); prompt.value = ''; time.value = ''; $('.cron-error').textContent = ''; $('.cron-error').hidden = true; status('', ''); render(); }
  $('.cron-close').onclick = () => dialog.close(); dialog.addEventListener('close', clear);
  $('.cron-refresh').onclick = () => void refresh(); kind.onchange = mode;
  document.addEventListener('visibilitychange', () => { clearTimeout(timer); if (!document.hidden && dialog.open && visible) void refresh(); });
  const labels: [string, string, string][] = [
    ['#cron-title', '예약 작업', 'Scheduled jobs'], ['.cron-refresh', '새로고침', 'Refresh'],
    ['.cron-lifetime', '이 세션의 CLI 예약을 관리합니다. CLI가 종료되면 예약도 끝납니다. 앱이나 운영체제에 별도 예약을 만들지 않습니다.', 'Manage schedules owned by this session’s CLI. Schedules end when the CLI exits. No separate app or operating-system schedules are created.'],
    ['.cron-unknown', '이 세션에 결과 미확인 요청이 있습니다. 원래 요청의 처리 여부가 확인될 때까지 변경을 제한합니다.', 'This session has an unresolved request. Changes remain blocked until the original request is reconciled.'],
    ['.cron-new-label', '예약 등록', 'Add schedule'], ['.cron-prompt-label', '실행할 작업', 'Task prompt'], ['.cron-schedule-label', '실행 방식', 'Schedule type'], ['.cron-kind option[value="every"]', '반복', 'Recurring'], ['.cron-kind option[value="at"]', '한 번 실행', 'One time'], ['.cron-interval-label', '반복 간격', 'Interval'], ['.cron-unit option[value="1"]', '초', 'seconds'], ['.cron-unit option[value="60"]', '분', 'minutes'], ['.cron-unit option[value="3600"]', '시간', 'hours'], ['.cron-at-label', '실행 시각', 'Execution time'], ['.cron-timing-help', '실행 시각은 이 컴퓨터의 현지 시각입니다. 실행 중 작업이 있으면 해당 회차를 건너뛰며, 밀린 회차를 쌓아두지 않습니다.', 'Times use this computer’s local time. Busy sessions skip the occurrence; missed occurrences do not queue up.'], ['.cron-add', '예약 등록', 'Add schedule'], ['.cron-limit', '일부 예약만 표시합니다. 실행 환경의 목록 범위 제한을 확인하세요.', 'Only part of the job list is displayed. Check the runtime’s list limits.'],
  ];
  for (const [selector, ko, en] of labels) bindText($(selector), ko, en);
  bindAttribute($('.cron-close'), 'aria-label', '예약 작업 창 닫기', 'Close scheduled jobs'); bindAttribute($('.cron-jobs'), 'aria-label', '예약 작업 목록', 'Scheduled job list');
  onLanguageChange(render); mode(); controls();
  return {
    async open(session: Session) { if (request || exiting) return; if (dialog.open) dialog.close(); clear(); selected = { ...session }; available = session.active; render(); dialog.showModal(); if (available) await refresh(); else status('실행 중인 세션에서 예약을 관리할 수 있습니다.', 'Start the session to manage its schedules.'); },
    updateSessions(sessions: Session[]) { const keys = new Set(sessions.filter(s => s.active).map(key)); for (const item of unresolved.keys()) if (!keys.has(item)) unresolved.delete(item); if (selected && !keys.has(key(selected))) { available = false; fresh = false; rows.clear(); clearTimeout(timer); status('원래 세션이 종료되거나 바뀌었습니다. 예약 상태는 미확인입니다.', 'The original session ended or changed. Schedule state is unknown.'); controls(); } },
    setVisible(value: boolean) { visible = value; clearTimeout(timer); if (visible && dialog.open && !document.hidden) void refresh(); },
    async beginExit() { exiting = true; if (dialog.open) dialog.close(); clearTimeout(timer); await request; controls(); },
    cancelExit() { exiting = false; controls(); },
  };
}
