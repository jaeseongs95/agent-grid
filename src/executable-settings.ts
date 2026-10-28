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
    <p>사용할 Codex 실행 파일을 선택하세요. 선택한 파일은 바로 저장됩니다.</p>
    <label for="codex-executable-path">직접 지정한 파일</label>
    <input id="codex-executable-path" readonly placeholder="자동 찾기 사용 중"/>
    <dl class="executable-details"><dt>적용 경로</dt><dd id="codex-executable-resolved"></dd><dt>찾은 위치</dt><dd id="codex-executable-source"></dd></dl>
    <p class="executable-help">새 세션과 세션 조회부터 적용됩니다. 이미 열린 세션은 계속 유지됩니다.<br>자동 찾기는 환경변수, PATH 순서로 실행 파일을 찾습니다.</p>
    <div id="codex-settings-status" role="status" aria-live="polite"></div>
    <div id="codex-settings-error" class="inline-error" role="alert" hidden></div>
    <div class="dialog-actions"><button id="codex-executable-reset" class="quiet-button">자동 찾기로 변경</button><button id="codex-executable-choose" class="primary-button">실행 파일 선택…</button></div>`;
  document.body.append(dialog);
  const $ = <T extends HTMLElement>(selector: string) => dialog.querySelector<T>(selector)!;
  const choose = $<HTMLButtonElement>('#codex-executable-choose');
  const reset = $<HTMLButtonElement>('#codex-executable-reset');
  const status = $('#codex-settings-status');
  const error = $('#codex-settings-error');
  let version = 0;
  let busy = false;
  let mutation: Promise<CodexSettings | null> | undefined;
  const current = (token: number) => dialog.open && version === token;
  function setBusy(value: boolean, text = '') {
    busy = value; choose.disabled = value; reset.disabled = value; status.textContent = text;
    dialog.setAttribute('aria-busy', String(value));
  }
  function showError(value: unknown) {
    error.textContent = value instanceof Error ? value.message : String(value); error.hidden = false;
  }
  function show(settings: CodexSettings) {
    $<HTMLInputElement>('#codex-executable-path').value = settings.savedPath ?? '';
    $<HTMLInputElement>('#codex-executable-path').placeholder = '자동 찾기 사용 중';
    $('#codex-executable-resolved').textContent = settings.resolvedPath ?? '사용 가능한 실행 파일이 없습니다';
    $('#codex-executable-source').textContent = { saved: '직접 지정', environment: '환경변수', path: 'PATH 자동 찾기', unavailable: '찾지 못함' }[settings.source];
    error.hidden = !settings.error; error.textContent = settings.error ?? '';
  }
  async function change(command: string, args?: Record<string, unknown>) {
    if (busy) return;
    const token = version;
    error.hidden = true; setBusy(true, command === 'choose_codex_executable' ? '파일 선택을 기다리는 중…' : '자동 찾기로 변경 중…');
    const operation = call<CodexSettings | null>(command, args);
    mutation = operation;
    try {
      const settings = await operation;
      if (current(token)) { if (settings) show(settings); status.textContent = settings ? '설정이 저장되었습니다.' : '파일 선택을 취소했습니다.'; }
    } catch (cause) { if (current(token)) { status.textContent = ''; showError(cause); } }
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
      $<HTMLInputElement>('#codex-executable-path').value = '';
      $<HTMLInputElement>('#codex-executable-path').placeholder = '확인 중…';
      $('#codex-executable-resolved').textContent = '확인 중…';
      $('#codex-executable-source').textContent = '확인 중…';
      error.hidden = true; setBusy(true, '현재 설정을 확인하는 중…');
      dialog.showModal(); $('#codex-settings-close').focus();
      try {
        // A chooser may finish after the popup closes. Read back its persisted result on reopen.
        if (mutation) { try { await mutation; } catch { /* Read the unchanged saved setting after a rejected selection. */ } }
        if (!current(token)) return;
        const settings = await call<CodexSettings>('get_codex_settings');
        if (current(token)) { show(settings); status.textContent = ''; }
      } catch (cause) { if (current(token)) {
        $<HTMLInputElement>('#codex-executable-path').placeholder = '설정을 확인하지 못했습니다';
        $('#codex-executable-resolved').textContent = '확인하지 못했습니다';
        $('#codex-executable-source').textContent = '확인하지 못함';
        status.textContent = ''; showError(cause);
      } }
      finally { if (current(token)) setBusy(false, status.textContent ?? ''); }
    },
  };
}
