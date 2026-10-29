import { onLanguageChange, t } from './i18n';

interface CodexSettings {
  savedPath: string | null;
  resolvedPath: string | null;
  source: 'saved' | 'environment' | 'path' | 'unavailable';
  error: string | null;
}

export function createExecutableSettings(call: <T>(command: string, args?: Record<string, unknown>) => Promise<T>) {
  const dialog = document.createElement('dialog');
  dialog.id = 'codex-settings-dialog';
  dialog.setAttribute('aria-labelledby', 'codex-settings-title');
  dialog.innerHTML = `
    <div class="dialog-heading"><span class="eyebrow">EXECUTABLE</span><button id="codex-settings-close" class="icon-button" aria-label="설정 닫기">×</button></div>
    <h2 id="codex-settings-title">실행 파일 설정</h2>
    <p id="codex-settings-description">사용할 Codex 실행 파일을 선택하세요. 선택한 파일은 바로 저장됩니다.</p>
    <label for="codex-executable-path">직접 지정한 파일</label>
    <input id="codex-executable-path" readonly placeholder="자동 찾기 사용 중"/>
    <dl class="executable-details"><dt id="codex-executable-resolved-label">적용 경로</dt><dd id="codex-executable-resolved"></dd><dt id="codex-executable-source-label">찾은 위치</dt><dd id="codex-executable-source"></dd></dl>
    <p class="executable-help"><span id="codex-settings-scope">새 세션과 세션 조회부터 적용됩니다. 이미 열린 세션은 계속 유지됩니다.</span><br><span id="codex-settings-discovery">자동 찾기는 환경변수, PATH 순서로 실행 파일을 찾습니다.</span></p>
    <div id="codex-settings-status" role="status" aria-live="polite"></div>
    <div id="codex-settings-error" class="inline-error" role="alert" hidden></div>
    <div class="dialog-actions"><button id="codex-executable-reset" class="quiet-button">자동 찾기로 변경</button><button id="codex-executable-choose" class="primary-button">실행 파일 선택…</button></div>`;
  document.body.append(dialog);
  const $ = <T extends HTMLElement>(selector: string) => dialog.querySelector<T>(selector)!;
  const choose = $<HTMLButtonElement>('#codex-executable-choose');
  const reset = $<HTMLButtonElement>('#codex-executable-reset');
  const status = $('#codex-settings-status');
  const error = $('#codex-settings-error');
  const path = $<HTMLInputElement>('#codex-executable-path');
  function text(element: HTMLElement, ko: string, en = ko) {
    element.dataset.ko = ko; element.dataset.en = en; element.textContent = t(ko, en);
  }
  let pathPlaceholder = () => t('자동 찾기 사용 중', 'Using automatic discovery');
  function placeholder(ko: string, en: string) {
    pathPlaceholder = () => t(ko, en); path.placeholder = pathPlaceholder();
  }
  for (const [selector, ko, en] of [
    ['.eyebrow', '실행 파일', 'EXECUTABLE'],
    ['#codex-settings-title', '실행 파일 설정', 'Executable settings'],
    ['#codex-settings-description', '사용할 Codex 실행 파일을 선택하세요. 선택한 파일은 바로 저장됩니다.', 'Choose the Codex executable to use. Your selection is saved immediately.'],
    ['label[for="codex-executable-path"]', '직접 지정한 파일', 'Selected file'],
    ['#codex-executable-resolved-label', '적용 경로', 'Resolved path'],
    ['#codex-executable-source-label', '찾은 위치', 'Source'],
    ['#codex-settings-scope', '새 세션과 세션 조회부터 적용됩니다. 이미 열린 세션은 계속 유지됩니다.', 'Applies to new sessions and session browsing. Existing sessions remain open.'],
    ['#codex-settings-discovery', '자동 찾기는 환경변수, PATH 순서로 실행 파일을 찾습니다.', 'Automatic discovery checks the environment variable, then PATH.'],
    ['#codex-executable-reset', '자동 찾기로 변경', 'Use automatic discovery'],
    ['#codex-executable-choose', '실행 파일 선택…', 'Choose executable…'],
  ]) text($(selector), ko, en);
  function refreshLabels() {
    for (const element of dialog.querySelectorAll<HTMLElement>('[data-ko]')) element.textContent = t(element.dataset.ko!, element.dataset.en!);
    $('#codex-settings-close').setAttribute('aria-label', t('설정 닫기', 'Close settings'));
    path.placeholder = pathPlaceholder();
  }
  refreshLabels(); onLanguageChange(refreshLabels);
  let version = 0;
  let busy = false;
  let mutation: Promise<CodexSettings | null> | undefined;
  const current = (token: number) => dialog.open && version === token;
  function setBusy(value: boolean, ko?: string, en = ko) {
    busy = value; choose.disabled = value; reset.disabled = value; if (ko !== undefined) text(status, ko, en);
    dialog.setAttribute('aria-busy', String(value));
  }
  function showError(value: unknown) {
    error.textContent = value instanceof Error ? value.message : String(value); error.hidden = false;
  }
  function show(settings: CodexSettings) {
    path.value = settings.savedPath ?? '';
    placeholder('자동 찾기 사용 중', 'Using automatic discovery');
    text($('#codex-executable-resolved'), settings.resolvedPath ?? '사용 가능한 실행 파일이 없습니다', settings.resolvedPath ?? 'No executable available');
    const source = { saved: ['직접 지정', 'Selected file'], environment: ['환경변수', 'Environment variable'], path: ['PATH 자동 찾기', 'PATH discovery'], unavailable: ['찾지 못함', 'Not found'] }[settings.source];
    text($('#codex-executable-source'), source[0], source[1]);
    error.hidden = !settings.error; error.textContent = settings.error ?? '';
  }
  async function change(command: string, args?: Record<string, unknown>) {
    if (busy) return;
    const token = version;
    error.hidden = true; setBusy(true, command === 'choose_codex_executable' ? '파일 선택을 기다리는 중…' : '자동 찾기로 변경 중…', command === 'choose_codex_executable' ? 'Waiting for file selection…' : 'Switching to automatic discovery…');
    const operation = call<CodexSettings | null>(command, args);
    mutation = operation;
    try {
      const settings = await operation;
      if (current(token)) { if (settings) show(settings); text(status, settings ? '설정이 저장되었습니다.' : '파일 선택을 취소했습니다.', settings ? 'Settings saved.' : 'File selection canceled.'); }
    } catch (cause) { if (current(token)) { text(status, ''); showError(cause); } }
    finally { if (mutation === operation) mutation = undefined; if (current(token)) { busy = false; choose.disabled = false; reset.disabled = false; dialog.setAttribute('aria-busy', 'false'); } }
  }
  choose.onclick = () => void change('choose_codex_executable');
  reset.onclick = () => void change('set_codex_executable', { path: null });
  $('#codex-settings-close').onclick = () => dialog.close();
  dialog.onclose = () => { version++; };
  return {
    async open() {
      if (dialog.open) return;
      const token = ++version;
      path.value = '';
      placeholder('확인 중…', 'Checking…');
      text($('#codex-executable-resolved'), '확인 중…', 'Checking…');
      text($('#codex-executable-source'), '확인 중…', 'Checking…');
      error.hidden = true; setBusy(true, '현재 설정을 확인하는 중…', 'Checking current settings…');
      dialog.showModal(); $('#codex-settings-close').focus();
      try {
        // A chooser may finish after the popup closes. Read back its persisted result on reopen.
        if (mutation) { try { await mutation; } catch { /* Read the unchanged saved setting after a rejected selection. */ } }
        if (!current(token)) return;
        const settings = await call<CodexSettings>('get_codex_settings');
        if (current(token)) { show(settings); text(status, ''); }
      } catch (cause) { if (current(token)) {
        placeholder('설정을 확인하지 못했습니다', 'Could not check settings');
        text($('#codex-executable-resolved'), '확인하지 못했습니다', 'Could not verify');
        text($('#codex-executable-source'), '확인하지 못함', 'Unknown');
        text(status, ''); showError(cause);
      } }
      finally { if (current(token)) setBusy(false); }
    },
  };
}
