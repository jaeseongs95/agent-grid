import { locale, onLanguageChange, t } from './i18n';

interface WeeklyUsageSnapshot {
  executable: string;
  observedAt: number;
  limitName: string | null;
  weekly: { usedPercent: number; resetsAt: number | null } | null;
}
interface WeeklyUsageHost {
  call<T>(command: string, args?: Record<string, unknown>): Promise<T>;
}
const pollInterval = 5 * 60 * 1000;

export function createWeeklyUsage(host: WeeklyUsageHost) {
  const element = document.createElement('section'); element.id = 'weekly-usage';
  element.innerHTML = '<div class="weekly-usage-heading"><span id="weekly-usage-label"></span><strong id="weekly-usage-value"></strong></div><progress id="weekly-usage-meter" max="100" hidden></progress><small id="weekly-usage-reset"></small><small id="weekly-usage-status" role="status" aria-live="polite"></small>';
  const $ = <T extends HTMLElement = HTMLElement>(id: string) => element.querySelector<T>(`#weekly-usage-${id}`)!;
  const meter = $<HTMLProgressElement>('meter');
  let snapshot: WeeklyUsageSnapshot | undefined;
  let error = '';
  let generation = 0;
  let exiting = false;
  let nativeVisible = true;
  let lastStarted = -Infinity;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let running: Promise<void> | undefined;

  function render() {
    const weekly = snapshot?.weekly;
    const expired = weekly?.resetsAt != null && weekly.resetsAt * 1000 <= Date.now();
    $('label').textContent = t('주간 사용량', 'Weekly usage');
    element.setAttribute('aria-label', t('계정 대표 주간 사용량', 'Representative account weekly usage'));
    element.setAttribute('aria-busy', String(!!running && !exiting));
    $('value').textContent = weekly ? `${weekly.usedPercent.toLocaleString(locale(), { maximumFractionDigits: 1 })}%` : '—';
    meter.hidden = !weekly;
    if (weekly) { meter.value = weekly.usedPercent; meter.setAttribute('aria-label', t('주간 사용 비율', 'Weekly percentage used')); }
    $('reset').textContent = weekly?.resetsAt != null
      ? t('초기화 ', 'Resets ') + new Date(weekly.resetsAt * 1000).toLocaleString(locale(), { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', timeZoneName: 'short' })
      : t('초기화 시각 확인 불가', 'Reset time unavailable');
    $('status').textContent = exiting ? t('종료 준비 중', 'Preparing to exit')
      : error ? t('사용량 조회 실패', 'Usage read failed')
      : expired ? t('초기화 시각 경과 · 갱신 대기', 'Reset time passed · awaiting refresh')
      : snapshot ? (weekly ? t('조회 ', 'Checked ') + new Date(snapshot.observedAt).toLocaleTimeString(locale(), { hour: '2-digit', minute: '2-digit' }) : t('주간 사용량 제공 안 됨', 'Weekly usage not provided'))
      : running ? t('사용량 조회 중…', 'Reading usage…') : t('다음 갱신 대기', 'Awaiting next refresh');
    element.classList.toggle('usage-stale', expired);
    // CLI data is text, including errors and names; never insert it as markup.
    element.title = [snapshot?.limitName, snapshot?.executable, error].filter(Boolean).join('\n');
  }

  function clearTimer() { if (timer !== undefined) clearTimeout(timer); timer = undefined; }
  function schedule() {
    clearTimer();
    if (exiting || !nativeVisible || document.hidden || running) return;
    let delay = Math.max(0, lastStarted + pollInterval - performance.now());
    const resetDelay = (snapshot?.weekly?.resetsAt ?? 0) * 1000 - Date.now();
    if (resetDelay > 0) delay = Math.min(delay, resetDelay);
    timer = setTimeout(() => { timer = undefined; render(); poll(); }, delay);
  }
  function poll() {
    if (exiting || !nativeVisible || document.hidden || running) return;
    if (performance.now() - lastStarted < pollInterval) { schedule(); return; }
    lastStarted = performance.now();
    const token = generation;
    // Promise.resolve also turns a synchronous host failure into the same
    // settlement path. One request retains ownership until fully settled.
    running = Promise.resolve().then(() => host.call<WeeklyUsageSnapshot>('weekly_usage')).then(result => {
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
      snapshot = undefined;
      error = String(reason instanceof Error ? reason.message : reason).slice(0, 1024);
    }).finally(() => { running = undefined; render(); schedule(); });
    render();
  }
  document.addEventListener('visibilitychange', () => {
    generation++; clearTimer();
    if (!document.hidden) { render(); poll(); }
  });
  onLanguageChange(render);
  render(); poll();
  return {
    element,
    setVisible(visible: boolean) {
      if (nativeVisible === visible) return;
      nativeVisible = visible; generation++; clearTimer();
      if (visible) { render(); poll(); }
    },
    refresh() { generation++; snapshot = undefined; error = ''; clearTimer(); render(); poll(); },
    beginExit(): Promise<void> { exiting = true; generation++; clearTimer(); render(); return running ?? Promise.resolve(); },
    cancelExit() { exiting = false; render(); poll(); },
  };
}
