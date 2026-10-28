export const MAX_WORKSPACE_SESSIONS = 32;

export interface SessionInfo {
  id: string;
  generation: number;
  title: string;
  cwd: string;
  threadId?: string;
  state: string;
  active: boolean;
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

// A stale generation must never overwrite a newer process or feed its terminal.
export function updateSession(previous: Session | undefined, next: SessionInfo): Session {
  if (previous && next.generation < previous.generation) return previous;
  // Cleanup is terminal for this generation; a delayed status snapshot cannot revive ownership.
  if (previous?.generation === next.generation && previous.active === false && next.active) return previous;
  return { ...next, seq: previous?.generation === next.generation ? previous.seq : -1 };
}

export function acceptOutput(session: Session | undefined, output: Output): boolean {
  return !!session && session.id === output.id && session.generation === output.generation && output.seq > session.seq;
}

export function writable(session: SessionInfo): boolean {
  return ['running', 'idle'].includes(session.state);
}

// Snapshot only selected, owned, writable generations. Never infer external ownership.
export function broadcastTargets(sessions: SessionInfo[], selected: Set<string>): SessionInfo[] {
  return sessions.filter(session => selected.has(session.id) && writable(session)).map(session => ({ ...session }));
}

export interface BroadcastOutcome { id: string; title: string; status: 'sent' | 'skipped' | 'failed'; error?: string }
export async function broadcast(
  targets: SessionInfo[],
  current: (id: string) => SessionInfo | undefined,
  send: (session: SessionInfo) => Promise<void>,
): Promise<BroadcastOutcome[]> {
  return Promise.all(targets.map(async target => {
    const session = current(target.id);
    if (!session || session.generation !== target.generation || !writable(session)) return { id: target.id, title: target.title, status: 'skipped' as const };
    try { await send(target); return { id: target.id, title: target.title, status: 'sent' as const }; }
    catch (error) { return { id: target.id, title: target.title, status: 'failed' as const, error: error instanceof Error ? error.message : String(error) }; }
  }));
}

export interface SavedSession { title: string; cwd: string; threadId?: string }
export interface Layout {
  version: 1;
  columns: number;
  columnSizes: number[];
  rowSizes: number[];
  sessions: SavedSession[];
}

export function chooseSavedSessions(layout: Layout | undefined, existing: SessionInfo[]): SavedSession[] {
  return layout?.sessions ?? existing.filter(session => session.state === 'restored').map(({ title, cwd, threadId }) => ({ title, cwd, threadId }));
}

export function parseLayout(value: unknown): Layout | undefined {
  if (!value || typeof value !== 'object') return;
  const v = value as Partial<Layout>;
  if (v.version !== 1 || !Array.isArray(v.sessions)) return;
  const sizes = (value: unknown) => Array.isArray(value) ? value.slice(0, MAX_WORKSPACE_SESSIONS).map(n => typeof n === 'number' && Number.isFinite(n) ? Math.max(0.2, Math.min(10, n)) : 1) : [];
  return {
    version: 1,
    columns: typeof v.columns === 'number' && Number.isFinite(v.columns) ? Math.max(1, Math.min(4, Math.round(v.columns))) : 2,
    columnSizes: sizes(v.columnSizes), rowSizes: sizes(v.rowSizes),
    sessions: v.sessions.filter(s => s && typeof s.title === 'string' && typeof s.cwd === 'string').slice(0, MAX_WORKSPACE_SESSIONS).map(s => ({ title: s.title, cwd: s.cwd, ...(typeof s.threadId === 'string' ? { threadId: s.threadId } : {}) })),
  };
}
