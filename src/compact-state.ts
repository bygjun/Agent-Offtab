import { sessionTree } from './session-tree';
import { isWorking, sessionKey, type Entry, type TrackedSessions } from './tracked-state';
import type { CompactPosition, CompactView, Session } from './types';

function priority(entry: Entry) {
  if (entry.session.waiting === 'approval') return 4;
  if (entry.session.waiting === 'input') return 3;
  if (isWorking(entry.session)) return 2;
  return entry.readAt === undefined ? 1 : 0;
}

export function compactEntries(sessions: TrackedSessions) {
  return [...sessions.values()].filter(entry => !entry.restoring && entry.hiddenAt === undefined)
    .sort((a, b) => priority(b) - priority(a) || b.workAt - a.workAt);
}

export function compactView(sessions: TrackedSessions, parents: Session[]): CompactView {
  const entries = compactEntries(sessions).map(({ session, workAt, readAt }) => ({ session, workAt, readAt }));
  const contextParents: Session[] = [];
  const tree = sessionTree<{ session: Session }>(entries, parents
    .filter(session => sessions.get(sessionKey(session))?.hiddenAt === undefined)
    .map(session => ({ session })));
  function collect(nodes: typeof tree) {
    for (const { row, context, children } of nodes) {
      if (context) contextParents.push(row.session);
      collect(children);
    }
  }
  collect(tree);
  return { entries, parents: contextParents };
}

export function mirrorCompact(rows: (Session & { compact?: CompactPosition })[]): TrackedSessions {
  const entries: TrackedSessions = new Map();
  for (const row of rows) {
    if (!row.compact) continue;
    entries.set(sessionKey(row), {
      session: row,
      workAt: row.compact.workAt,
      activityAt: row.activityAt ?? row.updatedAt,
      readAt: row.compact.readAt,
    });
  }
  return entries;
}

export function trackMirrored(previous: TrackedSessions, rows: (Session & { compact?: CompactPosition })[]): TrackedSessions {
  const entries = mirrorCompact(rows);
  for (const [key, entry] of entries) {
    const prior = previous.get(key);
    const currentRead = entry.session.read;
    if (entry.readAt === undefined && prior?.readAt !== undefined && prior.session.read?.at === undefined
      && currentRead && currentRead.at === undefined && prior.session.read?.version === currentRead.version) {
      entry.readAt = prior.readAt;
    }
  }
  return entries;
}
