import { bindAttribute, bindText, getLanguage, onLanguageChange, t } from './i18n';
import './mobile-control.css';

interface LiveSession { id: string; generation: number; title: string; active: boolean }
interface RemoteStatus { status: 'disabled' | 'connecting' | 'connected' | 'errored'; serverName: string; environmentId: string | null }
interface Pairing { manualCode: string; expiresAt: number; environmentId: string }
interface Reply { status: RemoteStatus | null; pairing: Pairing | null }
type Action = 'status' | 'enable' | 'disable' | 'pair';
interface Host { call<T>(command: string, args?: Record<string, unknown>): Promise<T>; notify(ko: string, error?: boolean, en?: string): void }

export function createMobileControl(host: Host) {
  const button = document.createElement('button');
  button.id = 'mobile-control-open'; button.className = 'side-link';
  bindText(button, '모바일 원격 제어', 'Mobile remote control');
  document.querySelector('#extensions-open')!.insertAdjacentElement('afterend', button);
  const dialog = document.createElement('dialog');
  dialog.id = 'mobile-control-dialog'; dialog.className = 'mobile-controls';
  dialog.setAttribute('aria-labelledby', 'mobile-control-title');
  dialog.innerHTML = `<div class="dialog-heading"><h2 id="mobile-control-title"></h2><button id="mobile-control-close" class="icon-button">×</button></div>
    <p id="mobile-control-help"></p><label for="mobile-control-session" id="mobile-control-session-label"></label>
    <select id="mobile-control-session"></select><div class="mobile-control-actions"><button id="mobile-control-refresh" class="quiet-button"></button><button id="mobile-control-enable" class="primary-button"></button><button id="mobile-control-disable" class="quiet-button"></button></div>
    <p id="mobile-control-status" role="status"></p><p id="mobile-control-server"></p>
    <button id="mobile-control-pair" class="primary-button"></button><section id="mobile-control-pairing" hidden><h3 id="mobile-control-code-label"></h3><output id="mobile-control-code"></output><p id="mobile-control-expiry"></p></section>
    <p id="mobile-control-error" class="inline-error" role="alert" hidden></p><small id="mobile-control-lifetime"></small>`;
  document.body.append(dialog);
  const $ = <T extends HTMLElement = HTMLElement>(selector: string) => dialog.querySelector<T>(selector)!;
  const select = $<HTMLSelectElement>('#mobile-control-session');
  const error = $('#mobile-control-error');
  const pairing = $('#mobile-control-pairing');
  const labels: [string, string, string][] = [
    ['#mobile-control-title', '모바일 원격 제어', 'Mobile remote control'],
    ['#mobile-control-help', '선택한 세션을 모바일 Codex에서 제어합니다. 선택한 CLI에 ChatGPT 계정 로그인이 필요하며 API 키 로그인은 지원하지 않습니다.', 'Control the selected session from mobile Codex. The selected CLI must be signed in with a ChatGPT account; API key authentication is unsupported.'],
    ['#mobile-control-session-label', '실행 중인 세션', 'Running session'],
    ['#mobile-control-refresh', '상태 새로고침', 'Refresh status'],
    ['#mobile-control-enable', '원격 제어 켜기', 'Enable remote control'],
    ['#mobile-control-disable', '원격 제어 끄기', 'Disable remote control'],
    ['#mobile-control-pair', '모바일 연결 코드 만들기', 'Create mobile pairing code'],
    ['#mobile-control-code-label', '모바일 Codex에 입력할 연결 코드', 'Pairing code for mobile Codex'],
    ['#mobile-control-lifetime', '이 앱이 세션 서버를 소유합니다. 창 닫기는 숨기기이며 제어가 유지됩니다. 세션 종료나 앱 종료 시 모바일 연결도 끝납니다. 연결 코드는 저장하지 않습니다.', 'This app owns the session server. Closing the window hides it and keeps remote control available. Stopping the session or exiting the app ends the mobile connection. Pairing codes are never saved.'],
  ];
  for (const [selector, ko, en] of labels) bindText($(selector), ko, en);
  bindAttribute($('#mobile-control-close'), 'aria-label', '원격 제어 창 닫기', 'Close remote control');
  let sessions: LiveSession[] = [];
  let status: RemoteStatus | null = null;
  let expiresAt: number | null = null;
  let epoch = 0;
  let exitRequested = false;
  let request: Promise<void> | undefined;
  const selected = () => sessions.find(session => session.id === select.value);
  function clearPairing() { $('#mobile-control-code').textContent = ''; $('#mobile-control-expiry').textContent = ''; expiresAt = null; pairing.hidden = true; }
  function renderStatus() {
    const states = {
      disabled: ['원격 제어 꺼짐', 'Remote control disabled'],
      connecting: ['원격 서비스 연결 중', 'Connecting to remote service'],
      connected: ['원격 서비스 연결됨 · 모바일 연결 여부와는 별개입니다', 'Remote service connected · mobile pairing is separate'],
      errored: ['원격 서비스 연결 오류', 'Remote service connection error'],
    } as const;
    const label = status ? states[status.status] : null;
    $('#mobile-control-status').textContent = label ? t(label[0], label[1]) : t('세션을 선택하고 상태를 조회하세요.', 'Select a session and check its status.');
    $('#mobile-control-server').textContent = status?.serverName ?? '';
    if (expiresAt !== null) $('#mobile-control-expiry').textContent = t('코드 만료: ', 'Code expires: ') + new Date(expiresAt * 1000).toLocaleString(getLanguage() === 'ko' ? 'ko-KR' : 'en-US');
  }
  function controls() {
    button.disabled = !!request || exitRequested;
    select.disabled = !!request || exitRequested || !sessions.length;
    for (const id of ['refresh', 'enable', 'disable', 'pair']) $<HTMLButtonElement>(`#mobile-control-${id}`).disabled = !!request || exitRequested || !selected();
    $<HTMLButtonElement>('#mobile-control-pair').disabled ||= status?.status !== 'connected';
  }
  function clear() { epoch++; sessions = []; select.replaceChildren(); status = null; clearPairing(); error.textContent = ''; error.hidden = true; renderStatus(); controls(); }
  function failure(reason: unknown, action?: Action) {
    const detail = String(reason instanceof Error ? reason.message : reason).slice(0, 600);
    if (dialog.open) { error.textContent = t('원격 제어 요청 실패: ', 'Remote control request failed: ') + detail; error.hidden = false; }
    else if (action && action !== 'status') host.notify(`원격 제어 요청 실패: ${detail}`, true, `Remote control request failed: ${detail}`);
  }
  function track(work: () => Promise<void>) {
    if (request || exitRequested) return Promise.resolve();
    request = work().finally(() => { request = undefined; controls(); }); controls(); return request;
  }
  function run(action: Action) {
    const session = selected(); if (!session) return Promise.resolve();
    const token = epoch;
    return track(async () => {
      error.hidden = true; if (action !== 'status') clearPairing();
      try {
        const result = await host.call<Reply>('mobile_control', { id: session.id, generation: session.generation, action });
        if (!dialog.open || token !== epoch) return;
        if (result.status) { status = result.status; if (status.status !== 'connected') clearPairing(); }
        if (result.pairing) {
          const value = result.pairing;
          if (typeof value.manualCode !== 'string' || !value.manualCode || value.manualCode.length > 256 || !Number.isSafeInteger(value.expiresAt) || value.expiresAt * 1000 <= Date.now()) throw new Error(t('유효한 연결 코드가 아닙니다. 새 코드를 요청하세요.', 'Invalid or expired pairing code. Request a new code.'));
          $('#mobile-control-code').textContent = value.manualCode; expiresAt = value.expiresAt; pairing.hidden = false;
        }
        renderStatus();
      } catch (reason) { if (token === epoch || action !== 'status') failure(reason, action); }
    });
  }
  async function open() {
    if (dialog.open || request || exitRequested) return;
    clear(); dialog.showModal(); const token = epoch;
    await track(async () => {
      try {
        const values = await host.call<LiveSession[]>('list_sessions');
        if (!dialog.open || token !== epoch) return;
        sessions = values.filter(value => value.active && Number.isSafeInteger(value.generation) && value.generation > 0).slice(0, 32);
        for (const session of sessions) select.add(new Option(session.title, session.id));
        if (!sessions.length) { error.textContent = t('먼저 세션을 실행하세요.', 'Start a session first.'); error.hidden = false; }
      } catch (reason) { if (token === epoch) failure(reason); }
    });
  }
  button.onclick = () => void open();
  $('#mobile-control-close').onclick = () => dialog.close();
  dialog.addEventListener('close', clear);
  select.onchange = () => { epoch++; status = null; clearPairing(); renderStatus(); void run('status'); };
  for (const [id, action] of [['refresh', 'status'], ['enable', 'enable'], ['disable', 'disable'], ['pair', 'pair']] as const) $(`#mobile-control-${id}`).onclick = () => void run(action);
  onLanguageChange(renderStatus);
  controls();
  return { open, async beginExit() { exitRequested = true; if (dialog.open) dialog.close(); await request; controls(); }, cancelExit() { exitRequested = false; controls(); } };
}
