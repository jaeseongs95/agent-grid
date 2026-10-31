import { bindAttribute, bindText, onLanguageChange, t } from './i18n';
import './conversation.css';

type Json = Record<string, any>;
export interface ConversationOptions {
  sessionId: string;
  generation: number;
  host: HTMLElement;
  call<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  ownerChanged?(owned: boolean): void;
}
interface Attachment { type: 'localImage' | 'file'; path: string; name?: string }
interface RuntimeEvent { seq: number; method: string; params: Json; requestId?: string; responded?: boolean }
interface Entry { id: string; role: string; text: string; status?: string; detail?: boolean }
interface QuestionDraft { value: string; other?: string }
const MAX_VISIBLE_ITEMS = 100;
const MAX_ATTACHMENTS = 12;
const PREVIEW_CHARACTERS = 16 * 1024;
const string = (value: unknown) => typeof value === 'string' ? value : '';

/** One controller owns one session's draft, bounded visible page and GUI input lease. */
export function createConversation(options: ConversationOptions) {
  const { host } = options;
  host.classList.add('conversation-host');
  host.hidden = true;
  host.innerHTML = `<div class="conversation-navigation"><button class="quiet-button conversation-refresh" type="button">최근 대화</button><select class="conversation-turns" aria-label="대화 구간"></select><button class="quiet-button conversation-older" type="button" hidden>이전 구간</button></div>
    <div class="conversation-status" role="status" aria-live="polite"></div><small class="conversation-current-settings" aria-live="polite"></small>
    <div class="conversation-messages" role="log" aria-label="대화 내용" tabindex="0"></div>
    <button class="quiet-button conversation-more" type="button" hidden>이전 항목</button>
    <div class="conversation-requests"></div>
    <form class="conversation-compose"><div class="conversation-attachments" aria-label="첨부 파일"></div>
      <textarea class="conversation-input" rows="3" aria-label="메시지" placeholder="메시지 입력 · Shift+Enter 줄바꿈" maxlength="65536"></textarea>
      <div class="conversation-controls"><button class="quiet-button conversation-attach" type="button">+ 첨부</button><label><span class="conversation-model-label">모델</span> <select class="conversation-model" aria-label="모델"></select></label><label><span class="conversation-effort-label">추론</span> <select class="conversation-effort" aria-label="추론 수준"></select></label><button class="quiet-button conversation-models-more" type="button" hidden>모델 더 보기</button><button class="quiet-button conversation-stop" type="button" disabled>중단</button><button class="primary-button conversation-send" type="submit" disabled>전송</button></div>
      <small class="conversation-hint">Enter 전송 · Shift+Enter 줄바꿈 · 초안은 이 세션에만 유지됩니다.</small>
    </form>`;
  const $ = <T extends HTMLElement = HTMLElement>(selector: string) => host.querySelector<T>(selector)!;
  const input = $<HTMLTextAreaElement>('.conversation-input');
  const model = $<HTMLSelectElement>('.conversation-model');
  const effort = $<HTMLSelectElement>('.conversation-effort');
  const turns = $<HTMLSelectElement>('.conversation-turns');
  const messages = $('.conversation-messages');
  const requests = $('.conversation-requests');
  const abort = new AbortController();
  const events = { signal: abort.signal };
  let visible = false; let foreground = true; let disposed = false; let attached = false; let connected = false;
  let changing: Promise<void> = Promise.resolve(); let epoch = 0;
  let contentVersion = 0; let pollVersion = 0;
  let historyQuery = 0; let liveRevision = 0;
  let activityRevision = 0;
  let polling: ReturnType<typeof setTimeout> | undefined;
  let sending = false; let interrupting = false; let uncertain = false; let composing = false; let loading = false; let staging = false;
  let modelsLoading = false;
  let sendFailed = false;
  let activeTurn = ''; let selectedTurn = ''; let nextTurns = ''; let nextItems = ''; let nextModels = '';
  let activeTurnKnown = false; let historyUnavailable = false; let deleted = false;
  let items: Entry[] = []; let turnPage: Json[] = []; let models: Json[] = [];
  let itemsDirty = false;
  const renderedItems = new Map<string, { signature: string; row: HTMLElement }>();
  let attachments: Attachment[] = []; let pendingRequests: RuntimeEvent[] = [];
  const stagedPaths = new Set<string>();
  const responding = new Set<string>();
  const questionDrafts = new Map<string, Map<string, QuestionDraft>>();
  let lastSeq = 0; let modelKnown = false; let currentModel = ''; let currentEffort = '';
  let threadId = ''; let modelDraft = false; let effortDraft = false;
  let requestSignature = '';
  const rpc = <T = Json>(request: Json) => options.call<T>('conversation_request', { id: options.sessionId, generation: options.generation, request });
  const alive = (version: number) => !disposed && visible && version === epoch;
  function status(ko: string, en = ko) { if (!disposed) bindText($('.conversation-status'), ko, en); }
  function failure(_error: unknown) { status('요청을 완료하지 못했습니다. 연결과 진행 중인 요청을 확인해 주세요. 초안은 보존됩니다.', 'Could not complete the request. Check the connection and any pending requests. Your draft is preserved.'); }
  function button(selector: string) { return $<HTMLButtonElement>(selector); }
  function updateControls() {
    const ready = visible && attached && connected && !loading && !deleted;
    input.disabled = !ready || sending || uncertain;
    button('.conversation-send').disabled = !ready || sending || staging || uncertain || !!activeTurn || (!input.value.trim() && !attachments.length);
    button('.conversation-stop').disabled = !ready || !activeTurn || interrupting || uncertain;
    button('.conversation-attach').disabled = !ready || sending || staging || uncertain || attachments.length >= MAX_ATTACHMENTS;
    model.disabled = !ready || sending || uncertain || !models.length || !!activeTurn;
    effort.disabled = model.disabled || effort.options.length <= 1;
    button('.conversation-models-more').hidden = !nextModels;
    button('.conversation-models-more').disabled = !ready || modelsLoading;
    turns.disabled = !ready; button('.conversation-older').disabled = !ready;
    button('.conversation-more').disabled = !ready;
    button('.conversation-refresh').disabled = !ready || sending;
  }
  function option(select: HTMLSelectElement, value: string, label: string) { select.add(new Option(label, value)); }
  function renderCurrentSettings() {
    bindText($('.conversation-current-settings'), `서버 확인 모델: ${currentModel || '미확인'} · 추론: ${currentEffort || '미확인'}`, `Server-confirmed model: ${currentModel || 'unknown'} · reasoning: ${currentEffort || 'unknown'}`);
  }
  function confirmedSettings(model: unknown, effort: unknown) {
    currentModel = string(model); currentEffort = string(effort); modelKnown = !!currentModel; renderCurrentSettings(); renderModels();
  }
  function renderEfforts() {
    const chosen = models.find(value => value.model === model.value || value.id === model.value);
    const selected = effortDraft ? effort.value : model.value === currentModel ? currentEffort : '';
    effort.replaceChildren();
    const values: Json[] = Array.isArray(chosen?.supportedReasoningEfforts) ? chosen.supportedReasoningEfforts : [];
    option(effort, '', t('현재 설정 미확인', 'Current setting unknown'));
    for (const value of values) { const id = string(value.reasoningEffort ?? value.effort); if (id) option(effort, id, id); }
    effort.value = [...effort.options].some(value => value.value === selected) ? selected : '';
    updateControls();
  }
  function renderModels() {
    const value = modelDraft ? model.value : currentModel;
    model.replaceChildren();
    option(model, '', modelKnown ? t('실행 환경 기본값', 'Runtime default') : t('현재 모델 미확인', 'Current model unknown'));
    for (const entry of models) { const id = string(entry.model ?? entry.id); if (id) option(model, id, string(entry.displayName) || id); }
    if (value && ![...model.options].some(entry => entry.value === value)) option(model, value, `${value} · ${t('지원 미확인', 'support unknown')}`);
    model.value = value; renderEfforts();
  }
  function renderAttachments() {
    const list = $('.conversation-attachments'); list.replaceChildren();
    attachments.forEach((attachment, index) => {
      const chip = document.createElement('span'); chip.className = 'conversation-attachment'; chip.title = attachment.path;
      const label = document.createElement('span'); label.textContent = `${attachment.type === 'localImage' ? '▧' : '▤'} ${attachment.name || attachment.path.split(/[\\/]/).pop()}`;
      const remove = document.createElement('button'); remove.type = 'button'; remove.textContent = '×';
      bindAttribute(remove, 'aria-label', `${label.textContent} 제거`, `Remove ${label.textContent}`);
      remove.disabled = sending || staging || uncertain;
      remove.onclick = () => { attachments.splice(index, 1); renderAttachments(); updateControls(); if (stagedPaths.delete(attachment.path)) void options.call('cleanup_conversation_attachment', { id: options.sessionId, generation: options.generation, path: attachment.path }).catch(failure); };
      chip.append(label, remove); list.append(chip);
    });
  }
  function addAttachments(values: Attachment[]) {
    for (const value of values) {
      if (!value.path || !['file', 'localImage'].includes(value.type)) continue;
      if (!attachments.some(known => known.path === value.path)) attachments.push(value);
    }
    if (attachments.length > MAX_ATTACHMENTS) { attachments = attachments.slice(0, MAX_ATTACHMENTS); status('한 번에 12개까지 첨부할 수 있습니다.', 'You can attach up to 12 files.'); }
    renderAttachments(); updateControls();
  }
  function appendFormatted(parent: HTMLElement, content: string) {
    // Only text and code nodes: messages never supply HTML, links, event handlers or image URLs.
    const fence = /```[^\n]*\n([\s\S]*?)(?:```|$)/g;
    let position = 0; let match: RegExpExecArray | null;
    while ((match = fence.exec(content))) {
      if (match.index > position) parent.append(document.createTextNode(content.slice(position, match.index)));
      const block = document.createElement('div'); block.className = 'conversation-code';
      const pre = document.createElement('pre'); const code = document.createElement('code'); code.textContent = match[1]; pre.append(code);
      const copy = document.createElement('button'); copy.type = 'button'; copy.className = 'quiet-button'; bindText(copy, '코드 복사', 'Copy code');
      // Keep the matched string, not the mutable RegExp result used by the loop.
      const codeText = match[1]; copy.onclick = () => { void navigator.clipboard.writeText(codeText).then(() => status('코드를 복사했습니다.', 'Code copied.')).catch(failure); };
      block.append(copy, pre); parent.append(block); position = fence.lastIndex;
    }
    if (position < content.length) parent.append(document.createTextNode(content.slice(position)));
  }
  function renderItems() {
    const nearEnd = messages.scrollHeight - messages.scrollTop - messages.clientHeight < 80;
    messages.querySelector('.conversation-placeholder')?.remove();
    const keep = new Set(items.map(entry => entry.id));
    for (const [id, rendered] of renderedItems) if (!keep.has(id)) { rendered.row.remove(); renderedItems.delete(id); }
    for (const entry of items) {
      const signature = JSON.stringify(entry); const previous = renderedItems.get(entry.id);
      if (previous?.signature === signature) continue;
      const row = document.createElement('article'); row.className = `conversation-message conversation-${entry.role}`; row.dataset.itemId = entry.id;
      const title = document.createElement('strong');
      const labels: Record<string, [string, string]> = { user: ['나', 'You'], assistant: ['도우미', 'Assistant'], tool: ['도구 실행', 'Tool execution'], error: ['오류', 'Error'], system: ['상태', 'Status'] };
      const label = labels[entry.role] ?? labels.system; bindText(title, ...label); row.append(title);
      if (entry.status || entry.detail) { const stamp = document.createElement('small'); stamp.textContent = [entry.status, entry.detail ? t('미리보기 · 원문 조회 가능', 'Preview · full content available') : ''].filter(Boolean).join(' · '); row.append(stamp); }
      const body = document.createElement('div'); body.className = 'conversation-content';
      if (entry.role === 'tool') {
        const detail = document.createElement('details'); const summary = document.createElement('summary'); bindText(summary, '실행 내용 펼치기', 'Show execution details');
        detail.append(summary, body); row.append(detail);
      } else row.append(body);
      appendFormatted(body, entry.text);
      if (entry.detail) { const detail = document.createElement('button'); detail.type = 'button'; detail.className = 'quiet-button'; bindText(detail, '전체 내용 보기', 'Read full content'); detail.onclick = () => void loadDetail(entry, body, detail, 0); row.append(detail); }
      if (previous) previous.row.replaceWith(row); else messages.append(row);
      renderedItems.set(entry.id, { signature, row });
    }
    // 현재 순서와 다를 때만 이동해 변하지 않은 행의 초점과 읽는 위치를 보존한다.
    let position = messages.firstElementChild;
    for (const entry of items) {
      const row = renderedItems.get(entry.id)!.row;
      if (row !== position) messages.insertBefore(row, position);
      position = row.nextElementSibling;
    }
    if (!items.length) {
      const empty = document.createElement('p'); empty.className = 'conversation-placeholder';
      if (deleted) bindText(empty, '원래 대화가 삭제되어 이력을 표시하지 않습니다. 입력 초안을 보존했습니다.', 'The original thread was deleted. History is unavailable and your draft is preserved.');
      else if (historyUnavailable) bindText(empty, '이력을 아직 조회할 수 없습니다. 대화가 비어 있거나 실행 중인 작업이 없다고 확인된 상태가 아닙니다.', 'History is not available yet. This does not confirm an empty or idle conversation.');
      else bindText(empty, '표시할 대화가 없습니다. 메시지를 보내 작업을 시작하세요.', 'No conversation to display. Send a message to start.');
      messages.append(empty);
    }
    button('.conversation-more').hidden = !nextItems;
    if (nearEnd) messages.scrollTop = messages.scrollHeight;
  }
  async function loadDetail(entry: Entry, body: HTMLElement, control: HTMLButtonElement, offset: number, snapshotId?: string) {
    if (deleted) return;
    control.disabled = true; const version = epoch;
    try {
      const value = await rpc({ type: 'detail', turnId: selectedTurn, itemId: entry.id, offset, ...(offset > 0 && snapshotId ? { snapshotId } : {}) });
      if (!alive(version) || !body.isConnected) return;
      body.replaceChildren(); appendFormatted(body, string(value.text));
      const pager = document.createElement('div'); pager.className = 'conversation-detail-pager';
      const position = document.createElement('small'); position.textContent = `${offset}–${value.nextOffset ?? value.totalBytes} / ${value.totalBytes} ${t('바이트', 'bytes')}`; pager.append(position);
      if (offset > 0) { const first = document.createElement('button'); first.type = 'button'; first.className = 'quiet-button'; bindText(first, '처음부터', 'From start'); first.onclick = () => void loadDetail(entry, body, control, 0); pager.append(first); }
      if (typeof value.nextOffset === 'number') { control.disabled = false; bindText(control, '다음 내용', 'Next part'); control.onclick = () => void loadDetail(entry, body, control, value.nextOffset, string(value.snapshotId)); }
      else { control.disabled = false; bindText(control, '처음부터 보기', 'Read from start'); control.onclick = () => void loadDetail(entry, body, control, 0); }
      body.append(pager);
    } catch (error) { if (alive(version)) { failure(error); control.disabled = false; } }
  }
  function itemEntry(value: Json): Entry {
    const type = string(value.type); let role = 'system'; let text = '';
    if (type === 'userMessage') { role = 'user'; text = (value.content ?? []).map((part: Json) => part.type === 'text' ? string(part.text) : part.type === 'localImage' || part.type === 'image' ? `[${t('이미지', 'Image')}: ${string(part.path ?? part.url)}]` : `[${t('첨부', 'Attachment')}: ${string(part.path ?? part.name)}]`).join('\n'); }
    else if (type === 'agentMessage') { role = 'assistant'; text = string(value.text); }
    else if (type === 'commandExecution') { role = 'tool'; text = [string(value.command), string(value.aggregatedOutput)].filter(Boolean).join('\n\n'); }
    else if (type === 'fileChange') { role = 'tool'; text = (value.changes ?? []).map((change: Json) => `${string(change.path)}\n${string(change.diff)}`).join('\n\n'); }
    else if (type === 'mcpToolCall') { role = 'tool'; text = `${string(value.server)} / ${string(value.tool)}\n${JSON.stringify(value.arguments ?? {})}\n${JSON.stringify(value.result ?? value.error ?? {})}`; }
    else if (type === 'reasoning') { text = [...(value.summary ?? []), ...(value.content ?? [])].map((part: unknown) => typeof part === 'string' ? part : JSON.stringify(part)).join('\n'); }
    else text = string(value.text ?? value.message) || JSON.stringify(value, null, 2);
    const hasDetail = (part: unknown): boolean => !!part && typeof part === 'object' && ((part as Json).__detail === true || Object.values(part).some(hasDetail));
    const detail = hasDetail(value) || value.truncated === true || value.hasMore === true || text.length > PREVIEW_CHARACTERS;
    if (text.length > PREVIEW_CHARACTERS) text = `${text.slice(0, PREVIEW_CHARACTERS)}\n${t('미리보기입니다. 나머지는 전체 내용 보기로 조회하세요.', 'Preview. Open full content to read the rest.')}`;
    if (detail && !text) text = t('큰 내용은 전체 내용 보기로 조회할 수 있습니다.', 'Open full content to read this large item.');
    return { id: string(value.id), role, text, status: string(value.status), detail };
  }
  function upsert(entry: Entry, delta = false) {
    liveRevision++;
    const found = items.findIndex(value => value.id === entry.id);
    if (found >= 0) items[found] = delta ? { ...items[found], detail: items[found].detail || entry.detail, text: items[found].text + entry.text } : entry;
    else items.push(entry);
    const current = items.find(value => value.id === entry.id)!;
    if (current.text.length > PREVIEW_CHARACTERS) { current.text = current.text.slice(0, PREVIEW_CHARACTERS); current.detail = true; }
    if (items.length > MAX_VISIBLE_ITEMS) { items = items.slice(-MAX_VISIBLE_ITEMS); status('화면에는 최근 100개 항목을 유지합니다. 이전 내용은 최근 대화에서 구간을 선택해 조회하세요.', 'Keeping 100 recent items. Select a turn from Recent conversation to revisit earlier content.'); }
    itemsDirty = true;
  }
  function renderTurns() {
    turns.replaceChildren();
    for (const value of turnPage) option(turns, string(value.id), `${string(value.id).slice(0, 10)} · ${string(value.status) || t('상태 미확인', 'Unknown status')}`);
    turns.value = selectedTurn; button('.conversation-older').hidden = !nextTurns;
  }
  function staleHistory() {
    status('조회 중 새 실시간 변경을 받아 이전 이력 응답을 적용하지 않았습니다. 필요한 구간을 다시 조회하세요.', 'Live updates arrived during the query. The older history response was not applied. Reload the desired turn.');
    button('.conversation-refresh').classList.add('needs-refresh');
  }
  async function loadItems(turnId: string, cursor = '', query = ++historyQuery) {
    if (deleted) return;
    const version = epoch; const content = contentVersion; const live = liveRevision;
    if (selectedTurn !== turnId) { items = []; nextItems = ''; renderItems(); }
    selectedTurn = turnId; loading = true; updateControls();
    try {
      const value = await rpc({ type: 'items', turnId, ...(cursor ? { cursor } : {}) });
      if (!alive(version) || !foreground || content !== contentVersion || query !== historyQuery || selectedTurn !== turnId) return;
      // The protocol supplies no snapshot revision to order a history response against live deltas.
      if (live !== liveRevision) { staleHistory(); return; }
      if (!Array.isArray(value.data)) throw new Error(t('대화 이력 목록 형식이 올바르지 않습니다.', 'Invalid conversation history list.'));
      const ids = new Set<string>();
      const data = value.data.map((entry: Json) => {
        if (!entry || entry.turnId !== turnId) throw new Error(t('다른 대화 구간의 이력 항목을 거부했습니다.', 'Rejected an item from another conversation turn.'));
        const item = entry.item;
        if (!item || typeof item !== 'object' || Array.isArray(item) || !string(item.type)) throw new Error(t('대화 이력 항목 형식이 올바르지 않습니다.', 'Invalid conversation history item.'));
        const id = string(item.id);
        if (!id || id.length > 256 || /[\u0000-\u001f\u007f-\u009f]/.test(id) || ids.has(id)) throw new Error(t('대화 이력 항목 식별자가 없거나 올바르지 않습니다.', 'Missing or invalid conversation history item ID.'));
        ids.add(id); return itemEntry(item);
      });
      items = data.reverse(); nextItems = string(value.nextCursor); renderItems();
    } catch (error) { if (alive(version) && content === contentVersion && query === historyQuery && selectedTurn === turnId) failure(error); }
    finally { if (alive(version) && content === contentVersion && query === historyQuery) { loading = false; updateControls(); } }
  }
  async function loadPage(cursor = '') {
    if (deleted) return;
    const version = epoch; const content = contentVersion; const live = liveRevision; const activity = activityRevision; const query = ++historyQuery; loading = true; updateControls();
    try {
      const value = await rpc({ type: 'page', ...(cursor ? { cursor } : {}) });
      if (!alive(version) || !foreground || content !== contentVersion || query !== historyQuery) return;
      if (live !== liveRevision || activity !== activityRevision) { staleHistory(); return; }
      if (!cursor && value.historyUnavailable === true && value.reason === 'unmaterialized' && Object.keys(value).length === 2) {
        historyUnavailable = true; activeTurnKnown = false;
        status('대화 이력을 저장할 준비를 기다리고 있습니다. 기존 내용과 초안을 보존합니다. 최근 대화를 다시 조회해 주세요.', 'Waiting for conversation history to be ready for storage. Existing content and your draft are preserved. Reload Recent conversation.');
        renderItems(); return;
      }
      if ('historyUnavailable' in value) throw new Error(t('이력 대기 응답 형식이 올바르지 않습니다.', 'Invalid history waiting response.'));
      if (!Array.isArray(value.data)) throw new Error(t('대화 구간 목록 형식이 올바르지 않습니다.', 'Invalid conversation turn list.'));
      const wasUnknown = historyUnavailable || !activeTurnKnown; historyUnavailable = false;
      turnPage = value.data; nextTurns = string(value.nextCursor);
      const nextTurn = string(turnPage[0]?.id);
      if (selectedTurn !== nextTurn || !nextTurn) { items = []; nextItems = ''; renderItems(); }
      selectedTurn = nextTurn; renderTurns();
      if (!cursor) { activeTurn = turnPage[0]?.status === 'inProgress' ? string(turnPage[0]?.id) : ''; activeTurnKnown = true; }
      if (wasUnknown) status('대화 이력과 실행 상태를 확인했습니다.', 'Conversation history and activity state confirmed.');
      if (selectedTurn) await loadItems(selectedTurn, '', query);
    } catch (error) { if (alive(version) && content === contentVersion && query === historyQuery) failure(error); }
    finally { if (alive(version) && content === contentVersion && query === historyQuery) { loading = false; updateControls(); } }
  }
  async function loadModels(cursor = '') {
    if (deleted) return;
    if (modelsLoading) return; modelsLoading = true; updateControls();
    const version = epoch;
    try { const value = await rpc({ type: 'models', ...(cursor ? { cursor } : {}) }); if (!alive(version)) return; models = value.data ?? []; nextModels = string(value.nextCursor); renderModels(); }
    catch (error) { if (alive(version)) { modelKnown = false; models = []; renderModels(); failure(error); } }
    finally { modelsLoading = false; if (alive(version)) updateControls(); }
  }
  function renderRequests() {
    const signature = JSON.stringify(pendingRequests); if (signature === requestSignature) return; requestSignature = signature;
    requests.replaceChildren();
    for (const event of pendingRequests) {
      const panel = document.createElement('section'); panel.className = 'conversation-request';
      const title = document.createElement('strong'); title.textContent = event.method;
      const description = document.createElement('pre'); description.textContent = JSON.stringify(event.params, null, 2); panel.append(title, description);
      const id = event.requestId;
      if (!id) { const reason = document.createElement('p'); bindText(reason, '요청 식별자를 확인하지 못해 응답할 수 없습니다.', 'Cannot respond without a verified request ID.'); panel.append(reason); requests.append(panel); continue; }
      if (event.method === 'item/commandExecution/requestApproval' || event.method === 'item/fileChange/requestApproval') {
        const advertised = event.params.availableDecisions;
        const choices = !Object.prototype.hasOwnProperty.call(event.params, 'availableDecisions') ? ['accept', 'decline', 'cancel']
          : Array.isArray(advertised) ? ['accept', 'acceptForSession', 'decline', 'cancel'].filter(value => advertised.includes(value)) : [];
        for (const choice of choices) {
          const action = document.createElement('button'); action.type = 'button'; action.className = 'quiet-button';
          const labels: Record<string, string> = { accept: t('이번만 승인', 'Approve once'), acceptForSession: t('세션 동안 승인', 'Approve for session'), decline: t('거절', 'Decline'), cancel: t('취소', 'Cancel') };
          action.textContent = labels[choice];
          action.onclick = () => void respond(id, { decision: choice }, panel); panel.append(action);
        }
        if (!choices.length) { const reason = document.createElement('p'); bindText(reason, '지원되는 승인 선택지가 없어 응답할 수 없습니다.', 'No supported approval choices are available.'); panel.append(reason); }
      } else if (event.method === 'item/tool/requestUserInput') {
        const form = document.createElement('form'); const answerInputs = new Map<string, () => string>();
        const draft = questionDrafts.get(id) ?? new Map<string, QuestionDraft>(); questionDrafts.set(id, draft);
        for (const question of event.params.questions ?? []) {
          const label = document.createElement('label'); label.textContent = string(question.question ?? question.header);
          let control: HTMLInputElement | HTMLSelectElement;
          if (Array.isArray(question.options) && question.options.length) {
            control = document.createElement('select'); option(control, '', t('선택하세요', 'Choose an answer'));
            for (const value of question.options) option(control, string(value.label), `${string(value.label)}${value.description ? ` — ${value.description}` : ''}`);
          } else { control = document.createElement('input'); control.type = question.isSecret ? 'password' : 'text'; control.maxLength = 8192; }
          const questionId = string(question.id); const answer = draft.get(questionId) ?? { value: '' }; draft.set(questionId, answer); control.value = answer.value;
          control.oninput = () => { answer.value = control.value; }; control.onchange = () => { answer.value = control.value; };
          control.required = true; control.setAttribute('aria-label', label.textContent); answerInputs.set(questionId, () => control.value); label.append(control);
          if (question.isOther && control instanceof HTMLSelectElement) {
            let otherValue = '__other__'; while ([...control.options].some(value => value.value === otherValue)) otherValue += '_';
            option(control, otherValue, t('직접 입력', 'Write an answer')); control.value = answer.value;
            const other = document.createElement('input'); other.type = question.isSecret ? 'password' : 'text'; other.maxLength = 8192; other.autocomplete = 'off'; other.value = answer.other ?? '';
            other.setAttribute('aria-label', `${string(question.question ?? question.header)} · ${t('직접 입력', 'Write an answer')}`);
            const selectAnswer = () => { answer.value = control.value; other.hidden = control.value !== otherValue; other.disabled = other.hidden; other.required = !other.hidden; };
            control.onchange = selectAnswer; selectAnswer(); other.oninput = () => { answer.other = other.value; };
            answerInputs.set(questionId, () => control.value === otherValue ? other.value : control.value); label.append(other);
          }
          form.append(label);
        }
        const submit = document.createElement('button'); submit.type = 'submit'; submit.className = 'primary-button'; bindText(submit, '답변 보내기', 'Send answers'); form.append(submit);
        form.onsubmit = event => { event.preventDefault(); const answers = Object.fromEntries([...answerInputs].map(([key, value]) => [key, { answers: [value()] }])); void respond(id, { answers }, panel); }; panel.append(form);
      } else { const reason = document.createElement('p'); bindText(reason, '이 요청 형식은 지원하지 않아 자동 응답하지 않습니다. 요청이 시작된 클라이언트에서 확인하세요.', 'This request type is unsupported and will not be answered automatically. Check the client where the request originated.'); panel.append(reason); }
      if (event.responded || responding.has(id)) { panel.querySelectorAll<HTMLButtonElement | HTMLInputElement | HTMLSelectElement>('button,input,select').forEach(control => { control.disabled = true; }); const waiting = document.createElement('small'); bindText(waiting, '응답을 보냈습니다. 서버의 처리 확인을 기다립니다.', 'Response sent. Waiting for server confirmation.'); panel.append(waiting); }
      requests.append(panel);
    }
  }
  async function respond(requestId: string, result: Json, panel: HTMLElement) {
    const pending = pendingRequests.find(event => event.requestId === requestId); if (pending?.responded || responding.has(requestId)) return; if (pending) pending.responded = true; responding.add(requestId);
    panel.querySelectorAll<HTMLButtonElement | HTMLInputElement | HTMLSelectElement>('button,input,select').forEach(value => { value.disabled = true; });
    const version = epoch;
    try { await rpc({ type: 'respond', requestId, result }); if (!alive(version)) return; const current = pendingRequests.find(event => event.requestId === requestId); if (current) current.responded = true; requestSignature = ''; renderRequests(); }
    catch (error) { if (alive(version)) { failure(error); requestSignature = ''; renderRequests(); } }
    finally { responding.delete(requestId); }
  }
  function receive(event: RuntimeEvent) {
    if (deleted) return;
    if (event.seq <= lastSeq) return;
    if (lastSeq && event.seq > lastSeq + 1) { confirmedSettings(undefined, undefined); status('실시간 이벤트 순번이 누락되었습니다. 최근 대화를 다시 불러오세요.', 'Live event sequence has a gap. Reload Recent conversation.'); button('.conversation-refresh').classList.add('needs-refresh'); }
    lastSeq = event.seq;
    const value = event.params;
    if (event.method === 'thread/deleted') {
      if (threadId && value.threadId === threadId) {
        deleted = true; connected = false; activeTurn = ''; activeTurnKnown = false; historyUnavailable = true; epoch++; stopPolling();
        models = []; nextModels = ''; modelDraft = false; effortDraft = false; questionDrafts.clear(); responding.clear();
        clearDisplay(); confirmedSettings(undefined, undefined); status('원래 대화가 삭제되었습니다. 연결을 종료하고 초안을 보존합니다.', 'The original thread was deleted. Closing the connection and preserving your draft.');
        changing = changing.catch(failure).then(async () => {
          if (!attached || disposed) return;
          try { await rpc({ type: 'detach' }); if (disposed) return; attached = false; options.ownerChanged?.(false); status('원래 대화가 삭제되어 연결을 해제했습니다. 초안은 보존했습니다.', 'The deleted thread connection was released. Your draft is preserved.'); }
          catch { if (!disposed) status('원래 대화가 삭제되었습니다. 연결 해제를 확인하지 못해 입력을 제한합니다. 초안은 보존됩니다.', 'The original thread was deleted. Input remains blocked until connection release is confirmed. Your draft is preserved.'); }
        });
      }
      return;
    }
    if (event.requestId) { if (!pendingRequests.some(entry => entry.requestId === event.requestId)) pendingRequests.push(event); renderRequests(); return; }
    if (event.method === 'thread/settings/updated') {
      if (threadId && value.threadId === threadId && value.threadSettings && typeof value.threadSettings === 'object' && !Array.isArray(value.threadSettings)) confirmedSettings(value.threadSettings.model, value.threadSettings.effort);
    }
    else if (event.method === 'turn/started') { liveRevision++; activityRevision++; activeTurn = string(value.turn?.id ?? value.turnId); activeTurnKnown = true; selectedTurn = activeTurn; items = []; nextItems = ''; itemsDirty = true; }
    else if (event.method === 'turn/completed') { liveRevision++; activityRevision++; if (activeTurn === string(value.turn?.id ?? value.turnId)) { activeTurn = ''; activeTurnKnown = true; } if (value.turn?.error) upsert({ id: `error-${event.seq}`, role: 'error', text: t('작업을 완료하지 못했습니다. 연결과 진행 중인 요청을 확인해 주세요.', 'The task could not finish. Check the connection and any pending requests.') }); }
    else if (event.method === 'error') upsert({ id: `error-${event.seq}`, role: 'error', text: t('작업을 완료하지 못했습니다. 연결과 진행 중인 요청을 확인해 주세요.', 'The task could not finish. Check the connection and any pending requests.') });
    else if (!value.turnId || value.turnId === selectedTurn) {
      if (event.method === 'item/started' || event.method === 'item/completed') { if (value.item) upsert(itemEntry(value.item)); }
      else if (event.method === 'item/agentMessage/delta') upsert({ id: string(value.itemId), role: 'assistant', text: string(value.delta), detail: value.delta?.__detail === true }, true);
      else if (event.method === 'item/commandExecution/outputDelta') upsert({ id: string(value.itemId), role: 'tool', text: string(value.delta), detail: value.delta?.__detail === true }, true);
    }
    updateControls();
  }
  function stopPolling() { pollVersion++; clearTimeout(polling); }
  function startPolling() { stopPolling(); if (visible && foreground && attached && !disposed && !deleted) void poll(epoch, pollVersion); }
  async function poll(version: number, revision: number) {
    if (!alive(version) || !foreground || !attached || revision !== pollVersion) return;
    try {
      const value = await rpc({ type: 'poll' }); if (!alive(version) || !foreground || revision !== pollVersion) return;
      connected = value.connected === true;
      if (typeof value.unknownResult === 'boolean') {
        const wasUncertain = uncertain; uncertain = value.unknownResult;
        if (wasUncertain && !uncertain && sendFailed) { status('전송이 거절되었습니다. 초안을 보존했습니다. 입력 내용을 확인한 뒤 다시 전송할 수 있습니다.', 'Send was rejected. Your draft is preserved. Review it before sending again.'); sendFailed = false; }
      }
      if (value.gap) { confirmedSettings(undefined, undefined); status('실시간 수신 구간이 누락되었습니다. 최근 대화를 다시 불러오세요.', 'Some live events were missed. Reload Recent conversation.'); button('.conversation-refresh').classList.add('needs-refresh'); }
      for (const event of value.events ?? []) { receive(event); if (deleted) return; }
      if (itemsDirty) { itemsDirty = false; renderItems(); }
      pendingRequests = value.pendingRequests ?? pendingRequests; renderRequests();
      const knownRequests = new Set(pendingRequests.map(event => event.requestId)); for (const id of questionDrafts.keys()) if (!knownRequests.has(id)) questionDrafts.delete(id);
      if (value.error) status('연결을 확인하지 못했습니다. 연결이 복구될 때까지 기다려 주세요. 초안은 보존됩니다.', 'Could not confirm the connection. Wait for it to recover. Your draft is preserved.');
      updateControls();
    } catch (error) { if (alive(version) && foreground && revision === pollVersion) { connected = false; failure(error); updateControls(); } }
    finally { if (alive(version) && foreground && attached && revision === pollVersion) polling = setTimeout(() => void poll(version, revision), 400); }
  }
  function adoptStartedTurn(turn: Json | undefined, version: number, activity: number) {
    const id = string(turn?.id);
    if (!id) return false;
    // 시작 응답보다 완료 알림이 먼저 올 수 있으므로 더 최근 실행 상태를 보존한다.
    if (!disposed && !deleted && version === epoch && activity === activityRevision) { activityRevision++; activeTurn = id; activeTurnKnown = true; }
    return true;
  }
  async function send() {
    if (button('.conversation-send').disabled || composing) return;
    if (new TextEncoder().encode(input.value).byteLength > 64 * 1024) { status('메시지는 UTF-8 기준 64 KiB까지 전송할 수 있습니다. 초안을 보존했습니다.', 'Messages are limited to 64 KiB in UTF-8. Your draft is preserved.'); return; }
    sending = true; const version = epoch; const activity = activityRevision; const draft = input.value; const sentAttachments = [...attachments]; updateControls(); renderAttachments();
    try {
      const value = await rpc({ type: 'send', requestId: crypto.randomUUID(), text: draft, attachments: sentAttachments.map(({ type, path }) => ({ type, path })), ...(model.value ? { model: model.value } : {}), ...(effort.value ? { effort: effort.value } : {}) });
      if (disposed || deleted) return;
      adoptStartedTurn(value.turn, version, activity);
      modelDraft = false; effortDraft = false; renderModels();
      if (input.value === draft) input.value = ''; attachments = []; for (const value of sentAttachments) stagedPaths.delete(value.path); renderAttachments();
      if (alive(version)) status('전송했습니다.', 'Sent.');
    } catch {
      if (!disposed && !deleted) { uncertain = true; sendFailed = true; status('전송 결과를 확인하지 못했습니다. 자동으로 다시 보내지 않습니다. 초안을 보존했습니다. 연결과 진행 중인 요청을 확인해 주세요.', 'Send result is unknown. It will not be resent automatically. Your draft is preserved. Check the connection and any pending requests.'); }
    } finally { sending = false; if (!disposed) { renderAttachments(); updateControls(); } }
  }
  async function interrupt() {
    if (!activeTurn || button('.conversation-stop').disabled) return;
    const id = activeTurn; interrupting = true; updateControls();
    try { await rpc({ type: 'interrupt', turnId: id }); if (!disposed && !deleted) status('중단을 요청했습니다. 실제 완료 이벤트를 기다립니다.', 'Interruption requested. Waiting for confirmation.'); }
    catch (error) { if (!disposed && !deleted) failure(error); }
    finally { interrupting = false; if (!disposed) updateControls(); }
  }
  async function compact(): Promise<{ status?: string }> {
    if (!visible || !attached || !connected || disposed || deleted || sending || staging || uncertain || activeTurn) return { status: 'notStarted' };
    sending = true; const version = epoch; const activity = activityRevision; updateControls(); renderAttachments();
    try {
      const value = await rpc({ type: 'compact' });
      if (!disposed && !deleted && version === epoch && value.status === 'started') {
        if (!adoptStartedTurn(value.turn, version, activity)) uncertain = true;
        status('압축을 요청했습니다. 서버의 완료 확인을 기다립니다.', 'Compaction requested. Waiting for server confirmation.');
      }
      return value;
    } catch (error) {
      if (!disposed && !deleted) { uncertain = true; status('압축 결과를 확인하지 못했습니다. 자동으로 다시 요청하지 않습니다. 초안은 보존됩니다.', 'The compaction result could not be confirmed. It will not be retried automatically. Your draft is preserved.'); }
      throw error;
    } finally { sending = false; if (!disposed) { updateControls(); renderAttachments(); } }
  }
  async function pickAttachments() {
    const version = epoch; staging = true; updateControls();
    try { const value = await options.call<Attachment[]>('choose_conversation_attachments'); if (alive(version)) addAttachments(value); }
    catch (error) { if (alive(version)) failure(error); }
    finally { staging = false; if (alive(version)) { renderAttachments(); updateControls(); } }
  }
  async function stageFiles(files: File[]) {
    if (input.disabled || staging || sending || uncertain) return;
    const version = epoch;
    staging = true; updateControls();
    try { for (const file of files.slice(0, MAX_ATTACHMENTS - attachments.length)) {
      if (!['image/png', 'image/jpeg'].includes(file.type)) { status('파일 경로를 안전하게 확인할 수 없습니다. 파일 참조는 + 첨부로 선택하세요. 이미지 붙여넣기·끌어놓기는 PNG와 JPEG를 지원합니다.', 'Cannot safely verify a file path. Use + Attach for file references. Image paste and drop support PNG and JPEG.'); continue; }
      if (file.size > 10 * 1024 * 1024) { status('붙여넣기 이미지는 10 MiB까지 지원합니다. 큰 파일은 첨부 선택을 사용하세요.', 'Pasted images are limited to 10 MiB. Use the attachment picker for larger files.'); continue; }
      try {
        const buffer = new Uint8Array(await file.arrayBuffer()); if (!alive(version)) return;
        let binary = ''; for (let offset = 0; offset < buffer.length; offset += 8192) binary += String.fromCharCode(...buffer.subarray(offset, offset + 8192));
        const value = await options.call<Attachment>('stage_conversation_image', { id: options.sessionId, generation: options.generation, dataUrl: `data:${file.type};base64,${btoa(binary)}` });
        if (alive(version)) { stagedPaths.add(value.path); addAttachments([value as Attachment]); }
      } catch (error) { if (alive(version)) failure(error); }
    } } finally { staging = false; if (alive(version)) { renderAttachments(); updateControls(); } }
  }
  function setVisible(value: boolean): Promise<void> {
    return changeVisibility(value, false);
  }
  function handoffToTerminal(): Promise<void> {
    return changeVisibility(false, true);
  }
  function changeVisibility(value: boolean, handoff: boolean): Promise<void> {
    if (disposed) return Promise.resolve();
    if (visible === value) return changing;
    visible = value; if (!handoff) host.hidden = !value; const version = ++epoch; stopPolling(); updateControls();
    changing = changing.catch(failure).then(async () => {
      if (attached) {
        try { await rpc({ type: handoff ? 'handoffToTerminal' : 'detach' }); }
        catch (error) { if (!disposed) { visible = true; host.hidden = false; connected = false; failure(error); updateControls(); startPolling(); } throw error; }
        attached = false; connected = false; options.ownerChanged?.(false);
        if (!visible) clearDisplay();
      }
      if (!visible) host.hidden = true;
      if (deleted) { updateControls(); return; }
      if (!alive(version)) return;
      status('대화에 연결하는 중…', 'Connecting to conversation…');
      try {
        // Claim locally before invoking: an attach error can leave the server's lease intact.
        attached = true; options.ownerChanged?.(true);
        const value = await rpc({ type: 'attach' });
        if (!alive(version)) {
          try { await rpc({ type: 'detach' }); }
          catch (error) { if (!disposed) { visible = true; host.hidden = false; connected = false; failure(error); updateControls(); startPolling(); } throw error; }
          attached = false; options.ownerChanged?.(false); return;
        }
        connected = true; uncertain = false; sendFailed = false; lastSeq = 0; threadId = string(value.threadId ?? value.thread?.id); confirmedSettings(value.model, value.reasoningEffort);
        historyUnavailable = value.historyUnavailable === true; activeTurnKnown = value.activeTurnKnown === true;
        activeTurn = activeTurnKnown ? string(value.activeTurnId) : ''; pendingRequests = []; renderRequests(); renderModels(); renderItems();
        if (historyUnavailable || !activeTurnKnown) status('연결됨 · 이력 또는 실행 상태 미확인. 전송 시 실행 환경이 입력 가능 여부를 확인합니다.', 'Connected · History or activity state is unknown. The runtime checks whether a new message can start.');
        else status('연결됨', 'Connected');
        await Promise.all([foreground && !historyUnavailable ? loadPage() : Promise.resolve(), loadModels()]); if (alive(version)) startPolling();
      } catch (error) { if (alive(version)) { connected = false; failure(error); } throw error; }
      finally { if (alive(version)) updateControls(); }
    });
    return changing;
  }
  async function setForeground(value: boolean) {
    if (disposed || foreground === value) return;
    foreground = value; contentVersion++; stopPolling();
    if (!value) {
      clearDisplay();
    } else if (visible && attached) { await loadPage(); startPolling(); }
  }
  function clearDisplay() {
    contentVersion++; items = []; itemsDirty = false; turnPage = []; pendingRequests = []; selectedTurn = ''; nextItems = ''; nextTurns = ''; loading = false; requestSignature = '';
    renderItems(); renderTurns(); renderRequests(); updateControls();
  }
  $('.conversation-compose').addEventListener('submit', event => { event.preventDefault(); void send(); }, events);
  input.addEventListener('input', updateControls, events);
  input.addEventListener('compositionstart', () => { composing = true; }, events);
  input.addEventListener('compositionend', () => { composing = false; }, events);
  input.addEventListener('keydown', event => { if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && !composing && event.keyCode !== 229) { event.preventDefault(); void send(); } }, events);
  input.addEventListener('paste', event => { const files = [...(event.clipboardData?.files ?? [])]; if (files.length) { event.preventDefault(); void stageFiles(files); } }, events);
  host.addEventListener('dragover', event => { if (event.dataTransfer?.types.includes('Files')) { event.preventDefault(); event.stopPropagation(); } }, events);
  host.addEventListener('drop', event => { const files = [...(event.dataTransfer?.files ?? [])]; if (files.length) { event.preventDefault(); event.stopPropagation(); void stageFiles(files); } }, events);
  button('.conversation-stop').addEventListener('click', () => void interrupt(), events);
  button('.conversation-attach').addEventListener('click', () => void pickAttachments(), events);
  button('.conversation-refresh').addEventListener('click', () => { button('.conversation-refresh').classList.remove('needs-refresh'); void loadPage(); }, events);
  button('.conversation-older').addEventListener('click', () => void loadPage(nextTurns), events);
  button('.conversation-more').addEventListener('click', () => void loadItems(selectedTurn, nextItems), events);
  button('.conversation-models-more').addEventListener('click', () => void loadModels(nextModels), events);
  turns.addEventListener('change', () => void loadItems(turns.value), events);
  model.addEventListener('change', () => { modelDraft = true; effortDraft = false; renderEfforts(); }, events);
  effort.addEventListener('change', () => { modelDraft = true; effortDraft = true; }, events);
  const labels: [string, string, string][] = [
    ['.conversation-refresh', '최근 대화', 'Recent conversation'], ['.conversation-older', '이전 구간', 'Earlier turns'], ['.conversation-more', '이전 항목', 'Earlier items'], ['.conversation-attach', '+ 첨부', '+ Attach'], ['.conversation-model-label', '다음 전송 모델', 'Next message model'], ['.conversation-effort-label', '다음 전송 추론', 'Next message reasoning'], ['.conversation-models-more', '모델 더 보기', 'More models'], ['.conversation-stop', '중단', 'Stop'], ['.conversation-send', '전송', 'Send'], ['.conversation-hint', 'Enter 전송 · Shift+Enter 줄바꿈 · 초안은 이 세션에만 유지됩니다.', 'Enter to send · Shift+Enter for a new line · Draft stays in this session.'],
  ];
  for (const [selector, ko, en] of labels) bindText($(selector), ko, en);
  bindAttribute(input, 'aria-label', '메시지', 'Message'); bindAttribute(input, 'placeholder', '메시지 입력 · Shift+Enter 줄바꿈', 'Message · Shift+Enter for a new line');
  for (const [element, ko, en] of [[model, '모델', 'Model'], [effort, '추론 수준', 'Reasoning effort'], [turns, '대화 구간', 'Conversation turns'], [messages, '대화 내용', 'Conversation'], [$('.conversation-attachments'), '첨부 파일', 'Attachments']] as const) bindAttribute(element, 'aria-label', ko, en);
  const unlanguage = onLanguageChange(() => { requestSignature = ''; renderedItems.clear(); messages.replaceChildren(); renderModels(); renderTurns(); renderItems(); renderRequests(); renderAttachments(); });
  renderCurrentSettings(); renderModels(); renderItems(); updateControls();
  return {
    setVisible,
    handoffToTerminal,
    compact,
    setForeground,
    refresh: () => visible && foreground && !loading ? loadPage() : Promise.resolve(),
    hasDraft: () => !!input.value || !!attachments.length,
    async dispose({ sessionEnded = false }: { sessionEnded?: boolean } = {}) {
      if (disposed) return;
      // The host may abandon a confirmed ended generation; an active lease always needs detach.
      if (!sessionEnded) { await setVisible(false); if (attached) await rpc({ type: 'detach' }); }
      if (attached) { attached = false; options.ownerChanged?.(false); }
      visible = false; disposed = true; epoch++; contentVersion++; stopPolling(); abort.abort(); unlanguage();
      input.value = ''; attachments = []; stagedPaths.clear(); responding.clear(); questionDrafts.clear(); renderedItems.clear(); items = []; turnPage = []; models = []; pendingRequests = []; host.replaceChildren();
    },
  };
}
