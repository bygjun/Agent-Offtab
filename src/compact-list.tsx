import { LoaderCircle } from 'lucide-react';
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { ProviderLogo } from './provider-logo';
import { isWorking, sessionKey, type Entry, type TrackedSessions } from './tracked-state';
import { TypedPreview } from './typed-preview';
import { sessionTree, type SessionTree } from './session-tree';
import type { Session } from './types';

function priority(entry: Entry) {
  if (entry.session.waiting === 'approval') return 4;
  if (entry.session.waiting === 'input') return 3;
  if (isWorking(entry.session)) return 2;
  return entry.readAt === undefined ? 1 : 0;
}

export function SessionStatus({ session, unread }: { session: Session; unread?: boolean }) {
  const { waiting, active, background = 0 } = session;
  const state = waiting ? 'waiting' : isWorking(session) ? 'running' : active === false ? 'done' : 'unknown';
  const label = waiting === 'approval' ? '승인 대기' : waiting === 'input' ? '입력 대기'
    : active ? '작업 중' : background ? `백그라운드 ${background}개` : active === false ? '완료' : '확인 중';
  return (
    <span className="compact-status" data-state={state}>
      {(state === 'waiting' || state === 'running' || unread) && <span className="status-dot" aria-hidden="true" />}
      <span>{label}</span>
      {background > 0 && (active || waiting) && <span className="background-count" aria-label={`백그라운드 ${background}개`} title={`백그라운드 ${background}개`}>+{background}</span>}
    </span>
  );
}

export function SessionTitle({ session, className }: { session: Session; className: string }) {
  return <span className="title-with-state">
    {isWorking(session) && !session.waiting && <LoaderCircle className="session-spinner" aria-hidden="true" />}
    <span className={className} title={session.title || '제목 없음'}>{session.title || '제목 없음'}</span>
  </span>;
}

export function SessionRowActions({ children, descendants, className, onRead, onHide }: {
  children: ReactNode;
  descendants?: ReactNode;
  className?: string;
  onRead?: () => void;
  onHide(): void;
}) {
  const [menu, setMenu] = useState(false);
  const row = useRef<HTMLLIElement>(null);

  useEffect(() => {
    if (!menu) return;
    const dismissOutside = (event: PointerEvent) => {
      if (event.target instanceof Element && event.target.closest('li') !== row.current) setMenu(false);
    };
    document.addEventListener('pointerdown', dismissOutside, true);
    return () => document.removeEventListener('pointerdown', dismissOutside, true);
  }, [menu]);

  return (
    <li ref={row} className={className}
      onBlur={event => {
        // WebKit can blur a button before delivering its pointer click.
        if (event.relatedTarget && !event.currentTarget.contains(event.relatedTarget)) setMenu(false);
      }}
      onContextMenu={event => {
        if (!(event.target instanceof Element) || event.target.closest('li') !== event.currentTarget) return;
        event.preventDefault();
        setMenu(true);
      }}
      onClick={event => {
        if (event.currentTarget.firstElementChild?.contains(event.target as Node)) setMenu(false);
      }}
      onKeyDown={event => {
        if (!(event.target instanceof Element) || event.target.closest('li') !== event.currentTarget) return;
        if (event.key === 'ContextMenu' || (event.shiftKey && event.key === 'F10')) {
          event.preventDefault();
          setMenu(true);
        }
        if (event.key === 'Escape' && menu) {
          event.currentTarget.querySelector('button')?.focus({ preventScroll: true });
          setMenu(false);
        }
      }}>
      {children}
      {menu && <div className="row-actions" aria-label="세션 동작" role="group">
        {onRead && <button type="button" onClick={event => {
          event.currentTarget.closest('li')?.querySelector('button')?.focus({ preventScroll: true });
          onRead();
          setMenu(false);
        }}>읽음으로 표시</button>}
        <button type="button" autoFocus onClick={onHide}>목록에서 제외</button>
      </div>}
      {descendants}
    </li>
  );
}

export function CompactList({ sessions, parents, onOpen, onRead, onHide, opening, notice, loading }: {
  sessions: TrackedSessions;
  parents: Session[];
  onOpen(session: Session): void;
  onRead(session: Session): void;
  onHide(session: Session): void;
  opening?: string;
  notice?: string;
  loading: boolean;
}) {
  const rows = [...sessions.values()].filter(entry => !entry.restoring && entry.hiddenAt === undefined)
    .sort((a, b) => priority(b) - priority(a) || b.workAt - a.workAt);
  const tree = sessionTree<Pick<Entry, 'session' | 'readAt'>>(rows, parents.map(session => ({ session })));

  function renderRow({ row: { session, readAt }, context, children }: SessionTree<Pick<Entry, 'session' | 'readAt'>>) {
    const key = sessionKey(session);
    const folder = session.cwd.split('/').filter(Boolean).at(-1);
    const canRead = !context && !session.waiting && !isWorking(session) && session.active === false && readAt === undefined;
    return (
      <SessionRowActions key={key} onRead={canRead ? () => onRead(session) : undefined} onHide={() => onHide(session)}
        descendants={children.length > 0 && <ul className="session-children" aria-label="하위 세션">{children.map(renderRow)}</ul>}>
        <button type="button" className="compact-row" data-read={readAt !== undefined}
          title="원본 세션 열기" disabled={opening === key}
          onClick={() => onOpen(session)}>
          <span className="compact-identity">
            <span className={`provider-icon ${session.provider}`}><ProviderLogo provider={session.provider} /></span>
            {folder && <span className="compact-folder" title={session.cwd}>{folder}</span>}
          </span>
          <span className="compact-content">
            <span className="compact-heading">
              <SessionTitle session={session} className="compact-title" />
              {!context && <SessionStatus session={session} unread={readAt === undefined} />}
            </span>
            {!context && <TypedPreview text={session.preview} />}
            {!context && <span className="sr-only">{readAt === undefined ? '읽지 않음' : '읽음'}</span>}
          </span>
        </button>
      </SessionRowActions>
    );
  }

  return (
    <main className="compact-scroll" aria-busy={loading}>
      {notice && <p className="compact-notice" role="status">{notice}</p>}
      {tree.length === 0 ? (
        notice ? null : <p className="compact-empty">{loading ? '세션 확인 중…' : '진행 중인 세션이 없습니다.'}</p>
      ) : (
        <ul className="compact-list" aria-label="진행 중인 세션">
          {tree.map(renderRow)}
        </ul>
      )}
    </main>
  );
}
