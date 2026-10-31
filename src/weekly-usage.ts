import { locale, onLanguageChange, t } from './i18n';

interface WeeklyUsageSnapshot {
  executable: string;
  observedAt: number;
  limitName: string | null;
  weekly: { usedPercent: number; resetsAt: number | null } | null;
}
interface WeeklyUsageHost {
  call<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  initiallyVisible?: boolean;
}
const pollInterval = 60 * 1000;
const manualInterval = 5 * 1000;

export function createWeeklyUsage(host: WeeklyUsageHost) {
  const element = document.createElement('section'); element.id = 'weekly-usage';
  element.innerHTML = '<div class="weekly-usage-heading"><span id="weekly-usage-label"></span><strong id="weekly-usage-value"></strong><button id="weekly-usage-refresh" type="button"></button></div><progress id="weekly-usage-meter" max="100" hidden></progress><small id="weekly-usage-reset"></small><small id="weekly-usage-observed"></small><small id="weekly-usage-status" role="status" aria-live="polite"></small>';
  const $ = <T extends HTMLElement = HTMLElement>(id: string) => element.querySelector<T>(`#weekly-usage-${id}`)!;
  const meter = $<HTMLProgressElement>('meter');
  let snapshot: WeeklyUsageSnapshot | undefined;
  let error = '';
  let generation = 0;
  let exiting = false;
  let nativeVisible = host.initiallyVisible ?? true;
  let lastStarted = -Infinity;
  let lastFinished = -Infinity;
  let lastManualStarted = -Infinity;
  let pendingRefresh = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let running: Promise<void> | undefined;

  function render() {
    const weekly = snapshot?.weekly;
    const expired = weekly?.resetsAt != null && weekly.resetsAt * 1000 <= Date.now();
    $('label').textContent = t('주간 사용량', 'Weekly usage');
    element.setAttribute('aria-label', t('계정 대표 주간 사용량', 'Representative account weekly usage'));
    element.setAttribute('aria-busy', String(!!running && !exiting));
    const refresh = $<HTMLButtonElement>('refresh');
    refresh.textContent = t('새로고침', 'Refresh');
    refresh.disabled = exiting || !!running || performance.now() - lastManualStarted < manualInterval;
    refresh.title = t('지금 다시 조회 · 수동 조회는 5초 간격', 'Read now · manual reads are limited to every 5 seconds');
    $('value').textContent = weekly ? `${weekly.usedPercent.toLocaleString(locale(), { maximumFractionDigits: 1 })}%` : '—';
    meter.hidden = !weekly;
    if (weekly) { meter.value = weekly.usedPercent; meter.setAttribute('aria-label', t('주간 사용 비율', 'Weekly percentage used')); }
    $('reset').textContent = weekly?.resetsAt != null
      ? t('초기화 ', 'Resets ') + new Date(weekly.resetsAt * 1000).toLocaleString(locale(), { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', timeZoneName: 'short' })
      : t('초기화 시각 확인 불가', 'Reset time unavailable');
    const age = snapshot ? Math.max(0, Math.floor((Date.now() - snapshot.observedAt) / 60_000)) : 0;
    $('observed').textContent = snapshot ? t('마지막 조회 ', 'Last checked ') + new Date(snapshot.observedAt).toLocaleString(locale(), { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' })
      + (age ? t(` · ${age}분 전`, ` · ${age} min ago`) : t(' · 방금', ' · just now')) : '';
    $('status').textContent = exiting ? t('종료 준비 중', 'Preparing to exit')
      : error ? t('사용량을 불러오지 못했습니다. 잠시 후 새로고침해 주세요.', 'Could not load usage. Please refresh in a moment.')
        + (weekly ? t(' 마지막 정상값을 표시합니다.', ' Showing the last known value.') : '')
      : expired ? t('초기화 시각 경과 · 갱신 대기', 'Reset time passed · awaiting refresh')
      : snapshot ? (weekly ? t('조회 ', 'Checked ') + new Date(snapshot.observedAt).toLocaleTimeString(locale(), { hour: '2-digit', minute: '2-digit' }) : t('주간 사용량 제공 안 됨', 'Weekly usage not provided'))
      : running ? t('사용량 조회 중…', 'Reading usage…') : t('다음 갱신 대기', 'Awaiting next refresh');
    element.classList.toggle('usage-stale', expired || !!error);
    // Account names are text. Technical errors and local executable paths stay
    // out of the user-facing DOM and tooltips.
    element.title = snapshot?.limitName ?? '';
  }

  function clearTimer() { if (timer !== undefined) clearTimeout(timer); timer = undefined; }
  function schedule() {
    clearTimer();
    if (exiting || !nativeVisible || document.hidden || running) return;
    const now = performance.now();
    let delay = Math.max(1, (pendingRefresh ? lastManualStarted + manualInterval : Math.max(lastStarted, lastFinished) + pollInterval) - now);
    const manualDelay = lastManualStarted + manualInterval - now;
    if (manualDelay > 0) delay = Math.min(delay, manualDelay);
    const resetDelay = (snapshot?.weekly?.resetsAt ?? 0) * 1000 - Date.now();
    if (resetDelay > 0) delay = Math.min(delay, resetDelay);
    timer = setTimeout(() => { timer = undefined; render(); poll(pendingRefresh); }, delay);
  }
  function poll(manual = false) {
    if (exiting || !nativeVisible || document.hidden || running) return;
    if (performance.now() - (manual ? lastManualStarted : Math.max(lastStarted, lastFinished)) < (manual ? manualInterval : pollInterval)) { schedule(); return; }
    lastStarted = performance.now();
    if (manual) lastManualStarted = lastStarted;
    pendingRefresh = false;
    const token = generation;
    // Promise.resolve also turns a synchronous host failure into the same
    // settlement path. One request retains ownership until fully settled.
    running = Promise.resolve().then(() => host.call<WeeklyUsageSnapshot>('weekly_usage', { manual })).then(result => {
      if (token !== generation || exiting || !nativeVisible || document.hidden) return;
      if (!result || typeof result.executable !== 'string' || !Number.isSafeInteger(result.observedAt) || result.observedAt < 0 || result.observedAt > 8_640_000_000_000_000
        || (result.weekly !== null && (!result.weekly || !Number.isFinite(result.weekly.usedPercent)
          || result.weekly.usedPercent < 0 || result.weekly.usedPercent > 100
          || (result.weekly.resetsAt !== null && (!Number.isSafeInteger(result.weekly.resetsAt) || result.weekly.resetsAt <= 0 || result.weekly.resetsAt > 8_640_000_000_000))))) {
        throw new Error(t('사용량 응답 형식이 올바르지 않습니다.', 'Invalid usage response.'));
      }
      snapshot = result; error = '';
    }).catch(reason => {
      if (token !== generation || exiting || !nativeVisible || document.hidden) return;
      error = String(reason instanceof Error ? reason.message : reason).slice(0, 1024);
    }).finally(() => {
      // Starting 60 seconds after renderer completion also clears the native
      // cooldown when IPC or worker-start delays differ between requests.
      lastFinished = performance.now(); running = undefined; render(); schedule();
    });
    render();
  }
  document.addEventListener('visibilitychange', () => {
    generation++; clearTimer();
    if (!document.hidden) { render(); poll(pendingRefresh); }
  });
  $('refresh').addEventListener('click', () => poll(true));
  onLanguageChange(render);
  render(); poll();
  return {
    element,
    setVisible(visible: boolean) {
      if (nativeVisible === visible) return;
      nativeVisible = visible; generation++; clearTimer();
      if (visible) { render(); poll(pendingRefresh); }
    },
    refresh() { if (exiting) return; generation++; snapshot = undefined; error = ''; pendingRefresh = true; clearTimer(); render(); poll(true); },
    beginExit(): Promise<void> { exiting = true; generation++; pendingRefresh = false; clearTimer(); render(); return running ?? Promise.resolve(); },
    cancelExit() { exiting = false; render(); poll(); },
  };
}
