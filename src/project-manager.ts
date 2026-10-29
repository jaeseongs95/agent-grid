import { bindAttribute, bindText, t } from './i18n';
import type { ProjectInfo, SessionInfo } from './model';

interface ProjectHost {
  call<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  projects(): ProjectInfo[];
  changed(project?: ProjectInfo, deletedId?: string): Promise<void>;
  moved(session: SessionInfo): void;
}

// 팝업은 한 번 만들고 재사용한다. 사용자 이름과 경로는 텍스트로만 삽입한다.
export function createProjectManager(host: ProjectHost) {
  function element<K extends keyof HTMLElementTagNameMap>(tag: K, parent: HTMLElement, id?: string) {
    const node = document.createElement(tag); if (id) node.id = id; parent.append(node); return node;
  }
  function button(parent: HTMLElement, id: string, ko: string, en: string, action?: () => void) {
    const node = element('button', parent, id); node.type = 'button'; node.className = 'quiet-button'; bindText(node, ko, en); if (action) node.onclick = action; return node;
  }
  function popup(id: string, ko: string, en: string) {
    const dialog = element('dialog', document.body, id); dialog.className = 'project-dialog'; dialog.setAttribute('aria-labelledby', `${id}-title`);
    const heading = element('div', dialog); heading.className = 'dialog-heading'; const title = element('h2', heading, `${id}-title`); bindText(title, ko, en);
    const close = button(heading, `${id}-close`, '닫기', 'Close', () => dialog.close()); bindAttribute(close, 'aria-label', '프로젝트 창 닫기', 'Close project dialog');
    return { dialog, title };
  }
  const { dialog, title } = popup('project-dialog', '새 프로젝트', 'New project');
  const form = element('form', dialog, 'project-form');
  const fields = element('fieldset', form); fields.className = 'project-fields';
  const nameLabel = element('label', fields); nameLabel.htmlFor = 'project-name'; bindText(nameLabel, '프로젝트 이름', 'Project name');
  const name = element('input', fields, 'project-name'); name.required = true;
  const folderLabel = element('label', fields); folderLabel.htmlFor = 'project-folder'; bindText(folderLabel, '기본 실행 폴더 · 선택', 'Default working folder · optional');
  const folder = element('input', fields, 'project-folder'); folder.readOnly = true;
  const folderActions = element('div', fields); folderActions.className = 'project-folder-actions';
  const choose = button(folderActions, 'project-folder-choose', '폴더 선택', 'Choose folder', () => void work(async () => { const selected = await host.call<string | null>('choose_project_folder'); if (selected !== null) folder.value = selected; }));
  const clear = button(folderActions, 'project-folder-clear', '폴더 없이 만들기', 'Use no folder', () => { folder.value = ''; });
  const favoriteLabel = element('label', fields); favoriteLabel.className = 'project-favorite-label';
  const favorite = element('input', favoriteLabel, 'project-favorite'); favorite.type = 'checkbox'; const favoriteText = element('span', favoriteLabel); bindText(favoriteText, '즐겨찾기', 'Favorite');
  const description = element('p', fields); bindText(description, '프로젝트는 세션 묶음입니다. 세션을 이동해도 실행 중인 터미널과 실제 실행 폴더는 바뀌지 않습니다.', 'Projects group sessions. Moving a session keeps its running terminal and actual working folder.');
  const error = element('div', form, 'project-error'); error.className = 'inline-error'; error.setAttribute('role', 'alert'); error.hidden = true;
  const actions = element('div', form); actions.className = 'dialog-actions';
  const remove = button(actions, 'project-delete', '프로젝트 삭제', 'Delete project', () => { if (!current) return; deleteName.textContent = current.name; deleteError.hidden = true; deletion.dialog.showModal(); }); remove.className = 'danger-button';
  const save = button(actions, 'project-save', '저장', 'Save'); save.type = 'submit'; save.className = 'primary-button';
  let current: ProjectInfo | undefined; let busy = false; let deleteError: HTMLElement;
  const nameValue = () => name.value === current?.name ? name.value : name.value.trim();
  function validName() {
    const value = nameValue(); name.setCustomValidity(current?.name === value ? '' : !value ? t('프로젝트 이름을 입력하세요.', 'Enter a project name.') : [...value].length > 120 || /\p{Cc}/u.test(value) ? t('제어문자 없이 120자 이하로 입력하세요.', 'Use at most 120 characters without control characters.') : '');
  }
  name.oninput = validName; name.oninvalid = validName;
  function setBusy(value: boolean) {
    busy = value; fields.disabled = value;
    for (const node of dialog.querySelectorAll<HTMLButtonElement>('button')) node.disabled = value;
    for (const node of deletion.dialog.querySelectorAll<HTMLButtonElement>('button')) node.disabled = value;
  }
  async function work(action: () => Promise<void>) {
    if (busy) return; setBusy(true); error.hidden = true;
    try { await action(); }
    catch (reason) { const visibleError = deletion.dialog.open ? deleteError : error; visibleError.hidden = false; const detail = reason instanceof Error ? reason.message : String(reason); bindText(visibleError, `프로젝트 변경 실패: ${detail}`, `Could not update project: ${detail}`); }
    finally { setBusy(false); }
  }
  form.onsubmit = event => {
    event.preventDefault(); validName(); if (!form.reportValidity()) return;
    void work(async () => {
      let project = current;
      if (!project) project = await host.call<ProjectInfo>('create_project', { name: nameValue(), cwd: folder.value || null });
      else if (project.name !== nameValue()) { project = await host.call<ProjectInfo>('rename_project', { id: project.id, name: nameValue() }); current = project; }
      current = project;
      if (!!project.favorite !== favorite.checked) { project = await host.call<ProjectInfo>('set_project_favorite', { id: project.id, favorite: favorite.checked }); current = project; }
      // 생성 성공 뒤 갱신이 실패해도 재시도에서 중복 생성하지 않는다.
      current = project; await host.changed(project); dialog.close();
    });
  };
  const deletion = popup('project-delete-dialog', '프로젝트를 삭제할까요?', 'Delete this project?');
  const deleteName = element('strong', deletion.dialog, 'project-delete-name');
  const warning = element('p', deletion.dialog); bindText(warning, '프로젝트 묶음만 삭제합니다. 코드 폴더와 대화는 보존되며, 소속 세션은 실행을 유지한 채 미분류로 이동합니다.', 'Only the project group is deleted. Code folders and conversations are kept. Its sessions keep running and become unassigned.');
  deleteError = element('p', deletion.dialog, 'project-delete-error'); deleteError.className = 'inline-error'; deleteError.setAttribute('role', 'alert'); deleteError.hidden = true;
  const deleteActions = element('div', deletion.dialog); deleteActions.className = 'dialog-actions';
  button(deleteActions, 'project-delete-cancel', '취소', 'Cancel', () => deletion.dialog.close());
  const confirm = button(deleteActions, 'project-delete-confirm', '프로젝트 삭제 확인', 'Confirm project deletion', () => void work(async () => {
    if (!current) return; const id = current.id; await host.call('delete_project', { id }); deletion.dialog.close(); current = undefined; remove.hidden = true; await host.changed(undefined, id); dialog.close();
  })); confirm.className = 'danger-button';
  for (const modal of [dialog, deletion.dialog]) modal.addEventListener('cancel', event => { if (busy) event.preventDefault(); });

  const moving = popup('project-move-dialog', '프로젝트로 이동', 'Move to project');
  const moveTitle = element('strong', moving.dialog, 'project-move-title');
  const moveDescription = element('p', moving.dialog); bindText(moveDescription, '소속만 변경합니다. 실행 중인 터미널과 실제 실행 폴더는 바뀌지 않습니다.', 'Only the group changes. The running terminal and actual working folder stay the same.');
  const moveLabel = element('label', moving.dialog); moveLabel.htmlFor = 'project-move-target'; bindText(moveLabel, '대상 프로젝트', 'Destination project');
  const target = element('select', moving.dialog, 'project-move-target');
  const moveError = element('p', moving.dialog, 'project-move-error'); moveError.setAttribute('role', 'alert'); moveError.className = 'inline-error'; moveError.hidden = true;
  let movingSession: SessionInfo | undefined; let movingBusy = false;
  const moveSubmit = button(moving.dialog, 'project-move-confirm', '이동', 'Move', () => {
    if (movingBusy || !movingSession) return;
    const { id, generation } = movingSession; const projectId = target.value || null;
    movingBusy = true; for (const node of moving.dialog.querySelectorAll<HTMLButtonElement | HTMLSelectElement>('button,select')) node.disabled = true;
    void host.call<SessionInfo>('move_session_to_project', { id, generation, projectId }).then(info => {
      if (info.id !== id || info.generation !== generation) throw new Error(t('세션 실행 세대가 변경되었습니다. 다시 선택하세요.', 'The session generation changed. Select the session again.'));
      host.moved(info); moving.dialog.close();
    }).catch(reason => { moveError.hidden = false; const detail = reason instanceof Error ? reason.message : String(reason); bindText(moveError, `프로젝트 이동 실패: ${detail}`, `Could not move session: ${detail}`); }).finally(() => { movingBusy = false; for (const node of moving.dialog.querySelectorAll<HTMLButtonElement | HTMLSelectElement>('button,select')) node.disabled = false; });
  }); moveSubmit.className = 'primary-button';
  moving.dialog.addEventListener('cancel', event => { if (movingBusy) event.preventDefault(); });
  return {
    open(project?: ProjectInfo) {
      if (dialog.open || busy) return; current = project; name.value = project?.name ?? ''; folder.value = project?.cwd ?? ''; favorite.checked = !!project?.favorite; choose.hidden = clear.hidden = !!project; remove.hidden = !project; error.hidden = true; name.setCustomValidity('');
      bindText(title, project ? '프로젝트 관리' : '새 프로젝트', project ? 'Manage project' : 'New project'); dialog.showModal(); name.focus();
    },
    move(session: SessionInfo) {
      if (moving.dialog.open) return; movingSession = { ...session }; moveTitle.textContent = session.title; moveError.hidden = true; target.replaceChildren();
      const unassigned = new Option('', ''); bindText(unassigned, '미분류', 'Unassigned'); target.add(unassigned);
      for (const project of [...host.projects()].sort((a, b) => Number(!!b.favorite) - Number(!!a.favorite))) target.add(new Option(project.name, project.id));
      target.value = session.projectId ?? ''; moving.dialog.showModal(); target.focus();
    },
  };
}
