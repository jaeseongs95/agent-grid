export const MAX_WORKSPACE_SESSIONS = 32;
export const MAX_SAVED_WORKSPACE_SESSIONS = 512;

export interface SessionInfo {
  id: string;
  generation: number;
  title: string;
  cwd: string;
  threadId?: string;
  state: string;
  active: boolean;
  projectId?: string | null;
  projectAssigned?: boolean;
  projectRevision?: number;
}

export interface Output {
  id: string;
  generation: number;
  seq: number;
  data: string;
}

export interface Session extends SessionInfo { seq: number }

// Native canonical paths may carry the Windows extended-length prefix.
// Match the directory itself, never its similarly named siblings or children.
export function projectKey(cwd: string): string {
  return cwd.replace(/^\\\\\?\\UNC\\/i, '\\\\').replace(/^\\\\\?\\/, '').replace(/\//g, '\\').replace(/\\+$/, '').toLowerCase();
}

export interface ProjectInfo { id: string; name: string; cwd: string | null; createdAt: number; favorite: boolean }
export interface ProjectGroup { key: string; id?: string; name: string; cwd: string | null; count: number; favorite: boolean }

// 명시적 미분류는 실행 폴더가 등록 프로젝트와 같아도 자동 분류하지 않는다.
export function sessionProjectKey(session: Pick<SessionInfo, 'cwd' | 'projectId' | 'projectAssigned'>, projects: ProjectInfo[]): string {
  if (session.projectAssigned || session.projectId) return session.projectId && projects.some(project => project.id === session.projectId) ? `id:${session.projectId}` : 'unassigned';
  const registered = projects.find(project => project.cwd && projectKey(project.cwd) === projectKey(session.cwd));
  return registered ? `id:${registered.id}` : `cwd:${projectKey(session.cwd)}`;
}
export function projectGroups(projects: ProjectInfo[], sessions: SessionInfo[]): ProjectGroup[] {
  const groups = new Map<string, ProjectGroup>(projects.map(project => [`id:${project.id}`, { ...project, key: `id:${project.id}`, count: 0 }]));
  for (const session of sessions) {
    const key = sessionProjectKey(session, projects);
    if (!groups.has(key)) groups.set(key, { key, name: key === 'unassigned' ? '' : session.cwd.replace(/\\/g, '/').split('/').filter(Boolean).at(-1) || session.cwd, cwd: key === 'unassigned' ? null : session.cwd, count: 0, favorite: false });
    groups.get(key)!.count++;
  }
  return [...groups.values()].sort((a, b) => Number(!!b.favorite) - Number(!!a.favorite));
}
export function savedSessionMetadata({ title, cwd, threadId, projectId, projectAssigned, projectRevision, sessionId, resumeOnStartup }: SavedSession): SavedSession {
  return { title, cwd, ...(typeof sessionId === 'string' && sessionId ? { sessionId } : {}), ...(typeof resumeOnStartup === 'boolean' ? { resumeOnStartup } : {}), ...(threadId ? { threadId } : {}), ...(typeof projectId === 'string' || projectId === null ? { projectId } : {}), ...(typeof projectAssigned === 'boolean' ? { projectAssigned } : {}), ...(typeof projectRevision === 'number' && Number.isSafeInteger(projectRevision) && projectRevision >= 0 ? { projectRevision } : {}) };
}

// A stale generation must never overwrite a newer process or feed its terminal.
export function updateSession(previous: Session | undefined, next: SessionInfo): Session {
  if (previous && next.generation < previous.generation) return previous;
  // Cleanup is terminal for this generation; a delayed status snapshot cannot revive ownership.
  if (previous?.generation === next.generation && previous.active === false && next.active) return previous;
  if (previous?.generation === next.generation && (previous.projectRevision ?? 0) > (next.projectRevision ?? 0)) next = { ...next, projectId: previous.projectId, projectAssigned: previous.projectAssigned, projectRevision: previous.projectRevision };
  return { ...next, seq: previous?.generation === next.generation ? previous.seq : -1 };
}

export function acceptOutput(session: Session | undefined, output: Output): boolean {
  return !!session && session.id === output.id && session.generation === output.generation && output.seq > session.seq;
}

export interface SavedSession { sessionId?: string; resumeOnStartup?: boolean; title: string; cwd: string; threadId?: string; projectId?: string | null; projectAssigned?: boolean; projectRevision?: number }
export interface Layout {
  version: 1;
  columns: number;
  columnSizes: number[];
  rowSizes: number[];
  sessions: SavedSession[];
}

export function chooseSavedSessions(layout: Layout | undefined, existing: SessionInfo[]): SavedSession[] {
  return layout?.sessions ?? existing.filter(session => session.state === 'restored').map(savedSessionMetadata);
}

export function parseLayout(value: unknown): Layout | undefined {
  if (!value || typeof value !== 'object') return;
  const v = value as Partial<Layout>;
  if (v.version !== 1 || !Array.isArray(v.sessions)) return;
  if (v.sessions.length > MAX_SAVED_WORKSPACE_SESSIONS) throw new RangeError('Saved workspace exceeds 512 metadata entries');
  const sizes = (value: unknown) => Array.isArray(value) ? value.slice(0, MAX_WORKSPACE_SESSIONS).map(n => typeof n === 'number' && Number.isFinite(n) ? Math.max(0.2, Math.min(10, n)) : 1) : [];
  return {
    version: 1,
    columns: typeof v.columns === 'number' && Number.isFinite(v.columns) ? Math.max(1, Math.min(4, Math.round(v.columns))) : 2,
    columnSizes: sizes(v.columnSizes), rowSizes: sizes(v.rowSizes),
    sessions: v.sessions.filter(s => s && typeof s.title === 'string' && typeof s.cwd === 'string').slice(0, MAX_SAVED_WORKSPACE_SESSIONS).map(s => savedSessionMetadata({ title: s.title, cwd: s.cwd, ...(typeof s.sessionId === 'string' ? { sessionId: s.sessionId } : {}), ...(typeof s.resumeOnStartup === 'boolean' ? { resumeOnStartup: s.resumeOnStartup } : {}), ...(typeof s.threadId === 'string' ? { threadId: s.threadId } : {}), ...(typeof s.projectId === 'string' || s.projectId === null ? { projectId: s.projectId } : {}), ...(typeof s.projectAssigned === 'boolean' ? { projectAssigned: s.projectAssigned } : {}), ...(typeof s.projectRevision === 'number' ? { projectRevision: s.projectRevision } : {}) })),
  };
}
