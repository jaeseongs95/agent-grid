import test from 'node:test';
import assert from 'node:assert/strict';
import { acceptOutput, broadcast, broadcastTargets, chooseSavedSessions, parseLayout, updateSession, type SessionInfo } from './model.ts';

const base: SessionInfo = { id: 'owned', generation: 2, title: 'test', cwd: 'C:\\work', state: 'running' };

test('late process generations and duplicate output cannot replace or write to the current terminal', () => {
  const current = { ...base, seq: 10 };
  assert.equal(updateSession(current, { ...base, generation: 1, state: 'stopped' }), current);
  assert.equal(updateSession(current, { ...base, state: 'idle' }).seq, 10);
  assert.equal(updateSession(current, { ...base, generation: 3 }).seq, -1);
  for (const output of [
    { id: base.id, generation: 1, seq: 11, data: 'old process' },
    { id: base.id, generation: 3, seq: 11, data: 'future process' },
    { id: base.id, generation: 2, seq: 10, data: 'duplicate' },
    { id: 'external', generation: 2, seq: 11, data: 'not owned' },
  ]) assert.equal(acceptOutput(current, output), false);
  assert.equal(acceptOutput(current, { id: base.id, generation: 2, seq: 11, data: 'new' }), true);
});

test('broadcast snapshots selected owned writable sessions, never waiting, external, stopped or unknown', () => {
  const owned = [base, { ...base, id: 'idle', state: 'idle' }, ...['waiting', 'stopped', 'unknown', 'exited', 'starting'].map(state => ({ ...base, id: state, state }))];
  const selected = new Set(['owned', 'idle', 'waiting', 'external', 'stopped', 'unknown', 'exited', 'starting']);
  const targets = broadcastTargets(owned, selected);
  assert.deepEqual(targets.map(s => s.id), ['owned', 'idle']);
  base.generation = 3;
  assert.equal(targets[0].generation, 2);
  base.generation = 2;
  assert.deepEqual(broadcastTargets(owned, new Set()), []);
});

test('persisted layout is bounded and restores metadata without starting any process', () => {
  assert.equal(parseLayout(null), undefined);
  assert.equal(parseLayout({ version: 2, sessions: [] }), undefined);
  const parsed = parseLayout({ version: 1, columns: 99, columnSizes: [NaN, -10, 1e9], sessions: [{ title: 'a', cwd: 'C:\\a', threadId: 't' }, { title: 1 }] });
  assert.equal(parsed?.columns, 4);
  assert.deepEqual(parsed?.columnSizes, [1, 0.2, 10]);
  assert.deepEqual(parsed?.sessions, [{ title: 'a', cwd: 'C:\\a', threadId: 't' }]);
  assert.equal(parseLayout({ version: 1, columns: NaN, sessions: [] })?.columns, 2);
});

test('an explicitly empty saved workspace stays empty; only a missing layout falls back to restored metadata', () => {
  const history = [{ ...base, state: 'restored', threadId: 'historical-thread' }, { ...base, id: 'live', state: 'running' }];
  const empty = parseLayout({ version: 1, columns: 2, sessions: [] });
  assert.deepEqual(chooseSavedSessions(empty, history), []);
  assert.deepEqual(chooseSavedSessions(undefined, history), [{ title: base.title, cwd: base.cwd, threadId: 'historical-thread' }]);
  const saved = parseLayout({ version: 1, sessions: [{ title: 'kept', cwd: 'C:\\kept' }] });
  assert.deepEqual(chooseSavedSessions(saved, history), [{ title: 'kept', cwd: 'C:\\kept' }]);
});

test('broadcast reports partial failure once per target and skips a changed generation or waiting state', async () => {
  const targets = [base, ...['failed', 'restarted', 'waiting'].map(id => ({ ...base, id }))];
  const current = new Map(targets.map(session => [session.id, session]));
  current.set('restarted', { ...base, id: 'restarted', generation: 3 });
  current.set('waiting', { ...base, id: 'waiting', state: 'waiting' });
  const sent: string[] = [];
  const outcomes = await broadcast(targets, id => current.get(id), async session => {
    sent.push(session.id);
    if (session.id === 'failed') throw new Error('PTY closed');
  });
  assert.deepEqual(sent, ['owned', 'failed']);
  assert.deepEqual(outcomes.map(outcome => outcome.status), ['sent', 'failed', 'skipped', 'skipped']);
  assert.equal(outcomes[1].error, 'PTY closed');
});
