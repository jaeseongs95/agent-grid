import { bindAttribute, bindText, getLanguage, onLanguageChange, t } from './i18n';
import { MAX_WORKSPACE_SESSIONS } from './model';
import './subagent-manager.css';

interface Session { id: string; generation: number; title: string; active: boolean; threadId?: string | null }
interface Agent {
  id: string; parentThreadId: string; runtimeId: string | null; nickname?: string | null; role?: string | null;
  status: 'pending' | 'running' | 'idle' | 'completed' | 'errored' | 'closed' | 'unknown';
  model?: string | null; effort?: string | null; activeTurnId?: string | null;
  capabilities: { send: boolean; interrupt: boolean; close: boolean; resume: boolean };
}
interface List { supported: boolean; connected: boolean; parentRuntimeId?: string | null; agents: Agent[]; nextCursor?: string | null; mutationState?: 'clear' | 'unknown'; reconciledRequestId?: string | null }
interface Reply { status: 'applied' | 'notApplied' | 'unknown' | 'unsupported' }
interface Host { call<T>(command: string, args: Record<string, unknown>): Promise<T>; notify(ko: string, error?: boolean, en?: string): void }
type Action = 'send' | 'interrupt' | 'close' | 'resume';
const LIMIT = 100;
const states: Record<Agent['status'], [string, string]> = {
  pending: ['대기 중', 'Pending'], running: ['실행 중', 'Running'], idle: ['입력 대기', 'Idle'],
  completed: ['완료됨', 'Completed'], errored: ['오류', 'Errored'], closed: ['종료됨', 'Closed'], unknown: ['미확인', 'Unknown'],
};
const validText = (value: unknown, limit = 1024): value is string => typeof value === 'string' && value.length > 0 && value.length <= limit && !value.includes('\0');
const validUuid = (value: unknown): value is string => typeof value === 'string' && /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(value);

export function createSubagentManager(host: Host) {
  const dialog = document.createElement('dialog'); dialog.id = 'subagents-dialog'; dialog.className = 'subagent-dialog'; dialog.setAttribute('aria-labelledby', 'subagent-title');
  dialog.innerHTML = `<div class="dialog-heading"><h2 id="subagent-title"></h2><button class="subagent-close icon-button" type="button">×</button></div>
    <p class="subagent-session"></p><p class="subagent-help"></p>
    <div class="subagent-toolbar"><button class="subagent-refresh quiet-button" type="button"></button><span class="subagent-status" role="status" aria-live="polite"></span></div>
    <p class="subagent-error inline-error" role="alert" hidden></p><p class="subagent-unknown" role="status" hidden></p><p class="subagent-remote-unknown" role="status" hidden></p>
    <div class="subagent-columns"><section class="subagent-browser"><label class="subagent-search-label"><span></span><input class="subagent-search" type="search" maxlength="512"></label>
    <div class="subagent-list"></div><div class="subagent-pages"><button class="subagent-first quiet-button" type="button"></button><button class="subagent-next quiet-button" type="button"></button><span class="subagent-page"></span></div><p class="subagent-limit" hidden></p></section>
    <section class="subagent-detail"><p class="subagent-empty-detail"></p><div class="subagent-fields" hidden><h3 class="subagent-name"></h3><p class="subagent-id"></p><dl></dl>
    <form class="subagent-form"><label><span class="subagent-prompt-label"></span><textarea class="subagent-prompt" rows="4" maxlength="65536"></textarea></label><button class="subagent-send primary-button" type="submit"></button></form>
    <div class="subagent-actions"><button class="subagent-interrupt quiet-button" type="button"></button><button class="subagent-resume quiet-button" type="button"></button><button class="subagent-end quiet-button" type="button"></button></div>
    <div class="subagent-confirm" hidden><p></p><button class="subagent-confirm-end quiet-button" type="button"></button><button class="subagent-confirm-cancel quiet-button" type="button"></button></div></div></section></div>`;
  document.body.append(dialog);
  const $ = <T extends HTMLElement = HTMLElement>(selector: string) => dialog.querySelector<T>(selector)!;
  const prompt = $<HTMLTextAreaElement>('.subagent-prompt');
  const search = $<HTMLInputElement>('.subagent-search');
  const rows = new Map<string, { signature: string; node: HTMLButtonElement }>();
  // ponytail: 동시 요청은 팝업 전체에서 하나. 세션별 미확인 표식은 제한 수 이후 새 변경을 막아 상한을 유지한다.
  const unresolved = new Map<string, { requestId: string; expectedParentRuntimeId: string; expectedRuntimeId: string | null }>();
  let selected: Session | undefined; let agentId: string | undefined; let agents: Agent[] = [];
  let parentRuntimeId: string | undefined;
  let available = false; let supported = false; let connected = false; let fresh = false; let knownList = false; let remoteUnknown = false;
  let visible = true; let exiting = false; let disposed = false; let epoch = 0;
  let request: Promise<void> | undefined; let reading = false; let timer: ReturnType<typeof setTimeout> | undefined;
  let cursor: string | null = null; let nextCursor: string | null = null; let page = 1; let overflow = false; let confirming = false;
  const key = (session: Session) => JSON.stringify([session.id, session.generation, session.threadId ?? null]);
  const uncertain = () => !!selected && unresolved.has(key(selected));
  const live = (version: number) => !disposed && dialog.open && version === epoch && !!selected && available && visible && !document.hidden && !exiting;
  const currentAgent = () => agents.find(agent => agent.id === agentId);
  const rpc = <T>(session: Session, value: Record<string, unknown>) => host.call<T>('subagents_request', { id: session.id, generation: session.generation, request: value });
  function status(ko: string, en: string) { if ($('.subagent-status').textContent !== t(ko, en)) bindText($('.subagent-status'), ko, en); }
  function error(ko: string, en: string) { bindText($('.subagent-error'), ko, en); $('.subagent-error').hidden = false; }
  function controls() {
    const agent = currentAgent();
    const eligible = !!selected && live(epoch) && supported && connected && fresh && validUuid(parentRuntimeId) && !uncertain() && !remoteUnknown && unresolved.size < MAX_WORKSPACE_SESSIONS;
    const ready = eligible && !request;
    prompt.disabled = !eligible || (!!request && !reading) || !agent?.capabilities.send;
    $<HTMLButtonElement>('.subagent-send').disabled = !ready || !agent?.capabilities.send;
    $<HTMLButtonElement>('.subagent-interrupt').disabled = !ready || !agent?.capabilities.interrupt || agent.status !== 'running' || !validText(agent.activeTurnId);
    $<HTMLButtonElement>('.subagent-resume').disabled = !ready || !agent?.capabilities.resume;
    $<HTMLButtonElement>('.subagent-end').disabled = !ready || !agent?.capabilities.close;
    $<HTMLButtonElement>('.subagent-confirm-end').disabled = !ready || !agent?.capabilities.close;
    $<HTMLButtonElement>('.subagent-refresh').disabled = !selected || !live(epoch) || !!request;
    $<HTMLButtonElement>('.subagent-first').disabled = !readyToRead() || cursor === null;
    $<HTMLButtonElement>('.subagent-next').disabled = !readyToRead() || !fresh || !nextCursor;
    $('.subagent-unknown').hidden = !uncertain();
    $('.subagent-remote-unknown').hidden = !remoteUnknown;
    $('.subagent-confirm').hidden = !confirming || !agent;
    dialog.setAttribute('aria-busy', String(!!request));
  }
  function readyToRead() { return live(epoch) && !request; }
  function render() {
    bindText($('.subagent-session'), selected ? `세션: ${selected.title}` : '', selected ? `Session: ${selected.title}` : '');
    const list = $('.subagent-list'); const query = search.value.trim().toLocaleLowerCase();
    const desired: HTMLElement[] = [];
    const ids = new Set(agents.map(agent => agent.id));
    for (const id of rows.keys()) if (!ids.has(id)) rows.delete(id);
    for (const agent of agents) {
      if (query && ![agent.id, agent.nickname, agent.role, agent.status, ...states[agent.status]].some(value => value?.toLocaleLowerCase().includes(query))) continue;
      const signature = JSON.stringify([getLanguage(), agent]); let entry = rows.get(agent.id);
      if (!entry || entry.signature !== signature) {
        const node = document.createElement('button'); node.type = 'button'; node.className = 'subagent-row'; node.dataset.agentId = agent.id;
        const name = document.createElement('strong'); name.textContent = agent.nickname || agent.id;
        const detail = document.createElement('small'); detail.textContent = `${t(...states[agent.status])}${agent.role ? ` · ${agent.role}` : ''}`;
        node.append(name, detail); node.onclick = () => { agentId = agent.id; confirming = false; render(); };
        entry = { signature, node }; rows.set(agent.id, entry);
      }
      entry.node.setAttribute('aria-pressed', String(agent.id === agentId)); desired.push(entry.node);
    }
    if (!desired.length) {
      const empty = document.createElement('p');
      bindText(empty, query ? '현재 페이지에 검색 결과가 없습니다.' : knownList && fresh ? '이 세션의 직접 하위 에이전트가 없습니다.' : '하위 에이전트 목록을 아직 확인하지 못했습니다.', query ? 'No matches on this page.' : knownList && fresh ? 'This session has no direct subagents.' : 'The subagent list has not been confirmed.'); desired.push(empty);
    }
    const retained = new Set<Node>(desired); for (const child of Array.from(list.childNodes)) if (!retained.has(child)) child.remove();
    let position = list.firstChild; for (const node of desired) { if (node !== position) list.insertBefore(node, position); position = node.nextSibling; }
    bindText($('.subagent-page'), `${page}쪽`, `Page ${page}`); $('.subagent-limit').hidden = !overflow;
    const agent = currentAgent(); $('.subagent-empty-detail').hidden = !!agent; $('.subagent-fields').hidden = !agent;
    if (agent) {
      $('.subagent-name').textContent = agent.nickname || agent.id; $('.subagent-id').textContent = agent.id;
      const fields: [string, string, string][] = [['상태', 'Status', t(...states[agent.status])], ['역할', 'Role', agent.role || t('미확인', 'Unknown')], ['모델', 'Model', agent.model || t('미확인', 'Unknown')], ['추론 수준', 'Reasoning effort', agent.effort || t('미확인', 'Unknown')]];
      const detail = $('.subagent-fields dl'); const signature = JSON.stringify([getLanguage(), fields]);
      if (detail.dataset.signature !== signature) { detail.replaceChildren(); for (const [ko, en, value] of fields) { const label = document.createElement('dt'); label.textContent = t(ko, en); const text = document.createElement('dd'); text.textContent = value; detail.append(label, text); } detail.dataset.signature = signature; }
    }
    controls();
  }
  function validate(value: List) {
    if (!value || typeof value.supported !== 'boolean' || typeof value.connected !== 'boolean' || !Array.isArray(value.agents) || (value.nextCursor != null && !validText(value.nextCursor, 8192)) || (value.mutationState !== undefined && !['clear', 'unknown'].includes(value.mutationState)) || (value.reconciledRequestId != null && !validText(value.reconciledRequestId))) throw Error('하위 에이전트 응답 형식 불일치');
    if ((value.supported || value.parentRuntimeId != null) && !validUuid(value.parentRuntimeId)) throw Error('부모 실행 세대 형식 불일치');
    const ids = new Set<string>();
    for (const agent of value.agents.slice(0, LIMIT)) {
      if (!agent || !validText(agent.id) || ids.has(agent.id) || !validText(agent.parentThreadId) || !Object.hasOwn(states, agent.status) || !agent.capabilities || ['send', 'interrupt', 'close', 'resume'].some(action => typeof agent.capabilities[action as Action] !== 'boolean') || [agent.nickname, agent.role, agent.model, agent.effort, agent.activeTurnId].some(text => text != null && (typeof text !== 'string' || text.length > 1024 || text.includes('\0')))) throw Error('하위 에이전트 목록 형식 불일치');
      if (value.supported && (agent.runtimeId !== null && !validUuid(agent.runtimeId))) throw Error('하위 실행 세대 형식 불일치');
      ids.add(agent.id);
    }
  }
  function scheduleRefresh() { clearTimeout(timer); if (live(epoch)) timer = setTimeout(() => void refresh(), 5000); }
  function track(version: number, work: () => Promise<void>, read = false) {
    if (request || exiting || disposed) return Promise.resolve();
    clearTimeout(timer); reading = read;
    request = Promise.resolve().then(work).finally(() => {
      request = undefined; reading = false; controls();
      if (version !== epoch && live(epoch)) void refresh(); else scheduleRefresh();
    }); controls(); return request;
  }
  function refresh(targetCursor = cursor, targetPage = page) {
    const session = selected; const version = epoch;
    if (!session || !live(version)) return Promise.resolve();
    return track(version, async () => {
      if (!live(version)) return;
      try {
        const value = await rpc<List>(session, { type: 'list', cursor: targetCursor }); if (!live(version)) return;
        validate(value); supported = value.supported; connected = value.connected; fresh = supported && connected;
        if (!fresh) remoteUnknown = false;
        if (fresh) {
          if (value.mutationState === 'unknown') remoteUnknown = true;
          else if (value.mutationState === 'clear') remoteUnknown = false;
          parentRuntimeId = value.parentRuntimeId ?? undefined;
          agents = value.agents.slice(0, LIMIT).filter(agent => agent.parentThreadId === session.threadId).map(agent => ({ id: agent.id, parentThreadId: agent.parentThreadId, runtimeId: agent.runtimeId, nickname: agent.nickname, role: agent.role, status: agent.status, model: agent.model, effort: agent.effort, activeTurnId: agent.activeTurnId, capabilities: { send: agent.capabilities.send, interrupt: agent.capabilities.interrupt, close: agent.capabilities.close, resume: agent.capabilities.resume } }));
          cursor = targetCursor; page = targetPage; nextCursor = value.nextCursor || null; overflow = value.agents.length > LIMIT; knownList = true; confirming = false;
          if (!agents.some(agent => agent.id === agentId)) agentId = agents[0]?.id;
          const pending = unresolved.get(key(session));
          if (pending && value.parentRuntimeId === pending.expectedParentRuntimeId && value.mutationState === 'clear' && value.reconciledRequestId === pending.requestId) unresolved.delete(key(session));
        }
        $('.subagent-error').hidden = true;
        if (!supported) status('연결된 서버에서 이 기능을 지원하지 않습니다. 호환 CLI·서버로 업데이트한 뒤 세션을 다시 연결하세요.', 'The connected server does not support this feature. Update to a compatible CLI and server, then reconnect the session.');
        else if (!connected) status('실행기 연결을 확인하지 못했습니다. 마지막 목록은 현재 상태가 아닐 수 있습니다.', 'The runtime connection is unavailable. The last list may be out of date.');
        else status('하위 에이전트 목록을 확인했습니다.', 'Subagent list confirmed.');
        render();
      } catch {
        if (live(version)) { fresh = false; remoteUnknown = false; error('목록을 불러오지 못했습니다. 연결을 확인하고 새로고침하세요. 마지막 목록과 초안은 보존합니다.', 'Could not load subagents. Check the connection and refresh. The last list and draft are preserved.'); controls(); }
      }
    }, true);
  }
  function mutate(type: Action) {
    const session = selected; const agent = currentAgent(); const version = epoch; const text = prompt.value;
    const expectedParentRuntimeId = parentRuntimeId;
    if (!session || !agent || !live(version) || request || !fresh || !supported || !connected || uncertain() || remoteUnknown || unresolved.size >= MAX_WORKSPACE_SESSIONS || !agent.capabilities[type] || agent.parentThreadId !== session.threadId || !validUuid(expectedParentRuntimeId) || (agent.runtimeId !== null && !validUuid(agent.runtimeId))) return Promise.resolve();
    if (type === 'interrupt' && (agent.status !== 'running' || !validText(agent.activeTurnId))) return Promise.resolve();
    if (type === 'close' && !confirming) return Promise.resolve();
    if (type === 'send' && (!text.trim() || text.includes('\0') || new TextEncoder().encode(text).byteLength > 65536)) { error('보낼 내용을 입력하세요. UTF-8 기준 64 KiB까지 보낼 수 있습니다.', 'Enter a message, up to 64 KiB in UTF-8.'); return Promise.resolve(); }
    const requestId = crypto.randomUUID();
    return track(version, async () => {
      if (!live(version)) return;
      unresolved.set(key(session), { requestId, expectedParentRuntimeId, expectedRuntimeId: agent.runtimeId });
      try {
        const result = await rpc<Reply>(session, { type, agentId: agent.id, requestId, expectedParentRuntimeId, expectedRuntimeId: agent.runtimeId, ...(type === 'send' ? { text } : {}), ...(type === 'interrupt' ? { expectedTurnId: agent.activeTurnId } : {}) });
        if (!result || !['applied', 'notApplied', 'unsupported'].includes(result.status)) throw Error('하위 에이전트 변경 결과 미확인');
        unresolved.delete(key(session)); if (!live(version)) return;
        fresh = false; confirming = false;
        if (result.status === 'applied') {
          if (type === 'send' && agentId === agent.id && prompt.value === text) prompt.value = '';
          status(type === 'interrupt' ? '현재 작업 중단을 요청했습니다. 새로고침하여 종료 여부를 확인하세요.' : type === 'close' ? '에이전트 종료를 요청했습니다. 새로고침하여 상태를 확인하세요.' : '요청이 처리되었습니다. 새로고침하여 상태를 확인하세요.', type === 'interrupt' ? 'Current turn interruption requested. Refresh to confirm it ended.' : type === 'close' ? 'Agent closure requested. Refresh to check its state.' : 'Request applied. Refresh to check its state.');
        } else if (result.status === 'unsupported') status('이 연결에서는 해당 관리 작업을 지원하지 않습니다. 새로고침하여 지원 상태를 확인하세요.', 'This connection does not support that action. Refresh to check support.');
        else error('요청이 적용되지 않았습니다. 새로고침하여 상태를 확인하세요. 초안은 보존합니다.', 'The request was not applied. Refresh to check its state. Your draft is preserved.');
      } catch {
        if (live(version)) { fresh = false; confirming = false; error('요청 결과를 확인하지 못했습니다. 자동으로 재전송하지 않습니다. 새로고침하여 원래 요청의 처리 여부를 확인하세요.', 'The request result is unknown. It will not be retried automatically. Refresh to reconcile the original request.'); }
      }
    });
  }
  function clear() {
    epoch++; selected = undefined; agentId = undefined; parentRuntimeId = undefined; available = false; agents = []; rows.clear();
    supported = false; connected = false; fresh = false; knownList = false; remoteUnknown = false; cursor = null; nextCursor = null; page = 1; overflow = false; confirming = false;
    clearTimeout(timer); prompt.value = ''; search.value = ''; $('.subagent-error').hidden = true; status('', ''); render();
  }
  const onClose = () => { if (!dialog.open) clear(); };
  const visibilityChanged = () => { clearTimeout(timer); if (visible && !document.hidden && dialog.open) void refresh(); else { epoch++; controls(); } };
  const onSubmit = (event: SubmitEvent) => { event.preventDefault(); void mutate('send'); };
  $('.subagent-close').onclick = () => { dialog.close(); clear(); }; dialog.addEventListener('close', onClose);
  $('.subagent-refresh').onclick = () => void refresh();
  $('.subagent-first').onclick = () => { if (readyToRead()) void refresh(null, 1); };
  $('.subagent-next').onclick = () => { if (readyToRead() && fresh && nextCursor) void refresh(nextCursor, page + 1); };
  search.oninput = render; $('.subagent-form').addEventListener('submit', onSubmit as EventListener);
  $('.subagent-interrupt').onclick = () => void mutate('interrupt'); $('.subagent-resume').onclick = () => void mutate('resume');
  $('.subagent-end').onclick = () => { if (!$<HTMLButtonElement>('.subagent-end').disabled) { confirming = true; controls(); $<HTMLButtonElement>('.subagent-confirm-cancel').focus(); } };
  $('.subagent-confirm-end').onclick = () => void mutate('close'); $('.subagent-confirm-cancel').onclick = () => { confirming = false; controls(); };
  document.addEventListener('visibilitychange', visibilityChanged);
  const labels: [string, string, string][] = [
    ['#subagent-title', '하위 에이전트', 'Subagents'], ['.subagent-help', '선택한 세션의 직접 하위 에이전트만 조회합니다. 전체 대화는 불러오지 않습니다.', 'View direct subagents of the selected session. Full conversations are not loaded.'],
    ['.subagent-refresh', '새로고침', 'Refresh'], ['.subagent-unknown', '이 세션에 결과 미확인 요청이 있습니다. 같은 요청의 처리 여부가 확인될 때까지 새 변경을 제한합니다.', 'This session has an unresolved request. New changes remain blocked until that request is reconciled.'],
    ['.subagent-remote-unknown', '다른 관리 요청의 결과를 확인하고 있습니다. 새로고침하여 확인이 끝난 뒤 변경할 수 있습니다.', 'Another management request is being checked. Refresh to confirm it has been resolved before making changes.'],
    ['.subagent-search-label span', '현재 페이지 검색', 'Search this page'], ['.subagent-first', '처음', 'First'], ['.subagent-next', '다음', 'Next'], ['.subagent-limit', '한 페이지에 최대 100개만 표시합니다.', 'At most 100 agents are displayed per page.'],
    ['.subagent-empty-detail', '목록에서 에이전트를 선택하세요.', 'Select an agent from the list.'], ['.subagent-prompt-label', '보낼 내용', 'Message'], ['.subagent-send', '보내기', 'Send'], ['.subagent-interrupt', '현재 작업 중단', 'Interrupt current turn'], ['.subagent-resume', '재개', 'Resume'], ['.subagent-end', '에이전트 종료', 'Close agent'],
    ['.subagent-confirm p', '이 에이전트와 하위 에이전트의 작업이 중단됩니다. 저장된 대화를 삭제하지 않습니다. 종료를 요청할까요?', 'This agent and its subagents will be interrupted. Saved conversations will not be deleted. Request closure?'], ['.subagent-confirm-end', '종료 요청 확인', 'Confirm closure'], ['.subagent-confirm-cancel', '취소', 'Cancel'],
  ];
  for (const [selector, ko, en] of labels) bindText($(selector), ko, en);
  bindAttribute($('.subagent-close'), 'aria-label', '하위 에이전트 창 닫기', 'Close subagents'); bindAttribute($('.subagent-list'), 'aria-label', '직접 하위 에이전트 목록', 'Direct subagent list');
  const removeLanguageListener = onLanguageChange(render); render();
  return {
    async open(session: Session) {
      if (disposed || exiting) return;
      clear(); selected = { ...session }; available = session.active && validText(session.threadId); render(); if (!dialog.open) dialog.showModal(); controls();
      if (available) await refresh(); else status('실행 중인 세션의 대화 식별자가 확인된 뒤 사용할 수 있습니다.', 'An active session with a confirmed thread identifier is required.');
    },
    updateSessions(sessions: Session[]) {
      const current = selected && sessions.find(session => key(session) === key(selected!) && session.active);
      if (selected && !current) { epoch++; available = false; fresh = false; remoteUnknown = false; confirming = false; agents = []; agentId = undefined; rows.clear(); clearTimeout(timer); status('원래 세션이 종료되거나 바뀌었습니다. 창을 다시 열어 현재 세션을 확인하세요.', 'The original session ended or changed. Reopen to select the current session.'); render(); }
    },
    setVisible(value: boolean) { if (visible === value || disposed) return; visible = value; epoch++; clearTimeout(timer); controls(); if (visible && dialog.open && !document.hidden) void refresh(); },
    async beginExit() { exiting = true; if (dialog.open) dialog.close(); clear(); await request; controls(); },
    cancelExit() { if (!disposed) { exiting = false; controls(); } },
    dispose() { if (disposed) return; disposed = true; exiting = true; if (dialog.open) dialog.close(); clear(); document.removeEventListener('visibilitychange', visibilityChanged); dialog.removeEventListener('close', onClose); removeLanguageListener(); dialog.remove(); unresolved.clear(); },
  };
}
