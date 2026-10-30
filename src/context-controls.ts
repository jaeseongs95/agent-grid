import { bindAttribute, onLanguageChange, t } from './i18n';
import './context-controls.css';

interface ContextStatus {
  connected: boolean;
  guiConnected: boolean;
  tokenUsage?: { last?: { totalTokens?: number }; modelContextWindow?: number | null };
  tokenUsageFresh: boolean;
  compacting: boolean;
  active: boolean | null;
  canCompact: boolean;
  reason?: string;
  revision?: number;
}
interface Options {
  host: HTMLElement;
  statusHost: HTMLElement;
  sessionId: string;
  generation: number;
  call<T>(command: string, args: Record<string, unknown>): Promise<T>;
  prepare(): Promise<void>;
  compact(): Promise<{ status?: string }>;
  notify(ko: string, error: boolean, en: string): void;
}

/** One small snapshot per visible card; observation never claims the GUI input lease. */
export function createContextControls(options: Options) {
  const { host } = options;
  host.innerHTML = '<button class="context-usage icon-button" type="button"><svg viewBox="0 0 20 20" aria-hidden="true"><circle class="context-track" cx="10" cy="10" r="7"/><circle class="context-fill" cx="10" cy="10" r="7" pathLength="100"/></svg><span>—</span></button><button class="context-compact icon-button" type="button"><svg viewBox="0 0 20 20" aria-hidden="true"><path d="M2 3l5 5M3 8h4V4M18 3l-5 5M13 4v4h4M2 17l5-5M3 12h4v4M18 17l-5-5M13 16v-4h4"/></svg></button>';
  const usage = host.querySelector<HTMLButtonElement>('.context-usage')!;
  const compact = host.querySelector<HTMLButtonElement>('.context-compact')!;
  const amount = usage.querySelector('span')!;
  const fill = usage.querySelector<SVGCircleElement>('.context-fill')!;
  let snapshot: ContextStatus | undefined;
  let visible = false; let disposed = false; let epoch = 0;
  let reading = false; let busy = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let revision = -1;
  const abort = new AbortController();
  const read = () => options.call<ContextStatus>('conversation_request', { id: options.sessionId, generation: options.generation, request: { type: 'contextStatus' } });
  const alive = (version: number) => !disposed && visible && epoch === version;
  const canAttempt = () => !!snapshot && (snapshot.canCompact === true || (snapshot.reason === 'guiDisconnected' && snapshot.connected === true && snapshot.active === false && !snapshot.compacting));
  function accept(value: ContextStatus) {
    if (!value || typeof value !== 'object') return;
    if (typeof value.revision === 'number' && Number.isSafeInteger(value.revision) && value.revision >= 0) {
      if (value.revision < revision) return;
      revision = value.revision;
    }
    snapshot = value;
  }
  function reason() {
    if (busy || snapshot?.compacting) return t('컨텍스트 압축 중 · 완료 확인 대기', 'Compacting context · waiting for confirmation');
    switch (snapshot?.reason) {
      case 'active': return t('작업 완료 후 컨텍스트를 압축할 수 있습니다.', 'Context can be compacted after the current task finishes.');
      case 'pendingRequest': return t('대기 중인 질문이나 승인을 먼저 처리하세요.', 'Resolve pending questions or approvals first.');
      case 'unsupported': return t('이 실행 환경은 안전한 수동 압축을 지원하지 않습니다.', 'This runtime does not support safe manual compaction.');
      case 'unknownResult': return t('이전 요청 결과를 확인할 때까지 압축하지 않습니다.', 'Compaction is blocked until the previous request is confirmed.');
      case 'deleted': return t('삭제된 대화는 압축할 수 없습니다.', 'A deleted conversation cannot be compacted.');
      case 'closing': return t('연결을 전환하거나 종료하는 중입니다.', 'The connection is switching or closing.');
      case 'guiDisconnected': return t('컨텍스트 압축 · 같은 세션의 대화 화면을 연결합니다.', 'Compact context · connects the conversation view of this session.');
      default: return snapshot?.canCompact ? t('컨텍스트 압축', 'Compact context') : t('연결과 작업 상태를 확인한 후 압축할 수 있습니다.', 'Compaction is available after connection and activity are confirmed.');
    }
  }
  function render() {
    options.statusHost.hidden = snapshot?.connected !== true || snapshot.compacting !== true;
    const status = t('압축 중', 'Compacting');
    if (options.statusHost.textContent !== status) options.statusHost.textContent = status;
    const used = snapshot?.tokenUsage?.last?.totalTokens;
    const capacity = snapshot?.tokenUsage?.modelContextWindow;
    const valid = typeof used === 'number' && Number.isSafeInteger(used) && used >= 0 && typeof capacity === 'number' && Number.isSafeInteger(capacity) && capacity > 0;
    const percent = valid ? Math.min(100, Math.max(0, Math.round(used / capacity * 100))) : undefined;
    const fresh = snapshot?.connected === true && snapshot.tokenUsageFresh === true;
    amount.textContent = percent === undefined || !fresh ? '—' : `${percent}%`;
    fill.style.strokeDasharray = `${percent ?? 0} 100`;
    host.classList.toggle('context-stale', !fresh);
    host.classList.toggle('context-compacting', busy || snapshot?.compacting === true);
    const ko = valid ? `컨텍스트 ${percent}% · 최근 모델 응답 사용량 ${used.toLocaleString('ko-KR')} / ${capacity.toLocaleString('ko-KR')} 토큰${fresh ? '' : ' · 마지막 확인값, 현재 사용량 미확인'} · 클릭하여 새로고침` : '컨텍스트 사용량 미확인 · 클릭하여 새로고침';
    const en = valid ? `Context ${percent}% · latest model response usage ${used.toLocaleString('en-US')} / ${capacity.toLocaleString('en-US')} tokens${fresh ? '' : ' · last known value, current usage unknown'} · click to refresh` : 'Context usage unknown · click to refresh';
    bindAttribute(usage, 'title', ko, en); bindAttribute(usage, 'aria-label', ko, en);
    compact.title = reason(); compact.setAttribute('aria-label', reason());
    usage.disabled = !visible || reading;
    compact.disabled = !visible || busy || !canAttempt();
    compact.setAttribute('aria-busy', String(busy || snapshot?.compacting === true));
  }
  function schedule(delay = 2000) {
    clearTimeout(timer);
    if (!disposed && visible) timer = setTimeout(() => void refresh(), delay);
  }
  async function refresh() {
    if (disposed || !visible || reading) return;
    reading = true; const version = epoch; render();
    try {
      const value = await read();
      if (alive(version)) accept(value);
    } catch {
      if (alive(version)) snapshot = { ...snapshot, connected: false, guiConnected: false, tokenUsageFresh: false, compacting: false, active: null, canCompact: false, reason: 'disconnected' };
    } finally {
      reading = false;
      if (!disposed) { render(); schedule(version === epoch ? 2000 : 0); }
    }
  }
  async function startCompaction() {
    if (disposed || !visible || busy || !canAttempt()) return;
    busy = true; const version = epoch; render();
    try {
      await options.prepare();
      if (!alive(version)) return;
      const current = await read();
      if (!alive(version)) return;
      accept(current);
      if (snapshot?.canCompact !== true || snapshot.guiConnected !== true) {
        options.notify('현재 연결·작업 상태에서는 압축할 수 없습니다. 상태를 확인한 뒤 다시 시도하세요.', false, 'Compaction is unavailable in the current connection or activity state. Check the status before trying again.'); return;
      }
      const result = await options.compact();
      if (!alive(version)) return;
      if (result.status === 'started') options.notify('압축을 요청했습니다. 서버의 완료 확인을 기다립니다.', false, 'Compaction requested. Waiting for server confirmation.');
      else if (result.status === 'skippedBusy') options.notify('다른 작업이 시작되어 압축하지 않았습니다.', false, 'Another task started, so context was not compacted.');
      else options.notify('현재 상태에서는 압축을 시작하지 못했습니다. 연결과 대기 중인 요청을 확인하세요.', false, 'Compaction could not start. Check the connection and pending requests.');
    } catch {
      if (alive(version)) options.notify('압축 결과를 확인하지 못했습니다. 자동으로 다시 요청하지 않습니다.', true, 'The compaction result could not be confirmed. It will not be retried automatically.');
    } finally {
      busy = false;
      if (!disposed) { render(); schedule(0); }
    }
  }
  usage.addEventListener('click', () => void refresh(), { signal: abort.signal });
  compact.addEventListener('click', () => void startCompaction(), { signal: abort.signal });
  const unlanguage = onLanguageChange(render);
  render();
  return {
    setVisible(value: boolean) {
      if (disposed || visible === value) return;
      visible = value; epoch++; clearTimeout(timer); render();
      if (visible) void refresh();
    },
    dispose() { if (disposed) return; disposed = true; epoch++; visible = false; clearTimeout(timer); abort.abort(); unlanguage(); snapshot = undefined; options.statusHost.hidden = true; options.statusHost.textContent = ''; host.replaceChildren(); },
  };
}
