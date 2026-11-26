import { isWorking, sessionKey } from './tracked-state';
import type { Session } from './types';

export type SessionTree<Row> = {
  row: Row;
  context: boolean;
  children: SessionTree<Row>[];
};

export function sessionTree<Row extends { session: Session }>(rows: Row[], parents: Row[]) {
  const roots: SessionTree<Row>[] = [];
  const visible = new Map(rows.map(row => [sessionKey(row.session), row]));
  const candidates = new Map(parents.map(row => [sessionKey(row.session), row]));
  const attached = new Map<string, SessionTree<Row>>();
  const visiting = new Set<string>();

  function attach(key: string, row: Row): SessionTree<Row> {
    const existing = attached.get(key);
    if (existing) return existing;
    const node: SessionTree<Row> = { row, context: !visible.has(key), children: [] };
    const { provider, parentNativeId } = row.session;
    const parentKey = provider === 'claude' && parentNativeId ? `claude:${parentNativeId}` : undefined;
    const parent = parentKey && (visible.get(parentKey) ?? candidates.get(parentKey));
    attached.set(key, node);
    visiting.add(key);
    if (parentKey && parent && !visiting.has(parentKey)) {
      attach(parentKey, parent).children.push(node);
    } else {
      roots.push(node);
    }
    visiting.delete(key);
    return node;
  }

  // Use the first visible member's priority to place the group.
  for (const [key, row] of visible) attach(key, row);
  return roots;
}


export function hasPendingSessions<Row extends { session: Session }>(nodes: SessionTree<Row>[]): boolean {
  return nodes.some(({ row: { session }, children }) =>
    !!session.waiting || isWorking(session) || hasPendingSessions(children),
  );
}
