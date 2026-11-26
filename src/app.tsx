import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Archive, ChevronDown, Command, Layers2, Maximize2, Minimize2, Pin, PinOff, RefreshCw, Search, Settings, ShieldCheck, X } from 'lucide-react';
import { day, type DeskApi, type ListScope, type Provider, type Session, type Snapshot, type Source, type WindowMode } from './types';
import { CompactList, SessionChildren, SessionRowActions, SessionStatus, SessionTitle } from './compact-list';
import { sessionTree, type SessionTree } from './session-tree';
import { hideSession, isWorking, markRead, restoreTracked, saveTracked, sessionKey, trackSessions, type TrackedSessions } from './tracked-state';
import { DeskSettings } from './desk-settings';
import { ProviderLogo, providerNames as providers } from './provider-logo';
import './style.css';

const filters = ['all', 'codex', 'claude'] as const;
type Filter = typeof filters[number];
const statusLabels: Record<Source['status'], string> = {
  ready: '읽기 가능',
  partial: '일부 기록 읽기 실패',
  error: '기록 읽기 실패',
  missing: '로컬 기록 없음',
};
const periodLabels = { 1: '최근 24시간', 7: '최근 7일' };
const relativeTime = new Intl.RelativeTimeFormat('ko', { numeric: 'auto' });
const absoluteTime = new Intl.DateTimeFormat('ko', { dateStyle: 'medium', timeStyle: 'short' });
const syncTime = new Intl.DateTimeFormat('ko', { hour12: false, hour: '2-digit', minute: '2-digit', second: '2-digit' });
const pageSize = 40;

function timeLabel(value: number, now: number) {
  const minutes = Math.max(0, Math.floor((now - value) / 60_000));
  if (minutes === 0) return '방금';
  if (minutes < 60) return relativeTime.format(-minutes, 'minute');
  if (minutes < 1440) return relativeTime.format(-Math.floor(minutes / 60), 'hour');
  return relativeTime.format(-Math.floor(minutes / 1440), 'day');
}

function ProviderIcon({ provider }: { provider: Filter }) {
  if (provider === 'all') return <Layers2 aria-hidden="true" />;
  return <ProviderLogo provider={provider} />;
}

function SessionRow({ tree, now, onOpen, onHide, opening, searchQuery, parent }: {
  tree: SessionTree<{ session: Session }>; now: number; onOpen: (item: Session) => void;
  onHide: (item: Session) => void; opening?: string; searchQuery: string; parent?: Session;
}) {
  const { row: { session: item }, context, children } = tree;
  const folder = item.cwd === parent?.cwd ? undefined : item.cwd.split('/').filter(Boolean).at(-1);
  return (
    <SessionRowActions className="session-row" onHide={() => onHide(item)}
      descendants={children.length > 0 && <SessionChildren nodes={children} searchQuery={searchQuery}
        render={child => <SessionRow key={sessionKey(child.row.session)} tree={child} now={now}
          opening={opening} onOpen={onOpen} onHide={onHide} searchQuery={searchQuery} parent={item} />} />}>
      <button type="button" className={parent ? 'session-open session-child-open' : 'session-open'} title="원본 세션 열기"
        onClick={() => onOpen(item)} disabled={opening === sessionKey(item)}>
        {!parent && <span className="session-identity">
          <span className={`provider-icon ${item.provider}`}><ProviderLogo provider={item.provider} /></span>
          {folder && <span className="session-folder" title={item.cwd}>{folder}</span>}
        </span>}
        <span className="conversation">
          <span className="row-heading">
            <SessionTitle session={item} className="session-title" />
            {!context && (item.waiting || isWorking(item) ? <SessionStatus session={item} />
              : <time dateTime={new Date(item.updatedAt).toISOString()} title={absoluteTime.format(item.updatedAt)}>
                  {timeLabel(item.updatedAt, now)}
                </time>)}
          </span>
          {parent && folder && <span className="session-folder" title={item.cwd}>{folder}</span>}
          {!context && (item.preview
            ? <span className="preview">{item.role === 'user' && <span className="byline">나 · </span>}{item.preview}</span>
            : <span className="preview unavailable">표시할 대화가 없습니다.</span>)}
        </span>
      </button>
    </SessionRowActions>
  );
}

export function App({ desk, clock = Date.now, persist = false }: { desk: DeskApi; clock?: () => number; persist?: boolean }) {
  const [modeError, setModeError] = useState(false);
  const [mode, setMode] = useState<WindowMode>('compact');
  const [modePending, setModePending] = useState(false);
  const [storageError, setStorageError] = useState(false);
  const [initial] = useState(() => {
    try { return { tracked: persist ? restoreTracked(window.localStorage) : new Map(), invalid: false }; }
    catch { return { tracked: new Map(), invalid: true }; }
  });
  const [tracked, setTracked] = useState<TrackedSessions>(initial.tracked);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [alerts, setAlerts] = useState(() => {
    try { return persist && window.localStorage.getItem('session-desk.wait-alerts') === 'on'; }
    catch { return false; }
  });
  const [opening, setOpening] = useState<string>();
  const [openError, setOpenError] = useState<string>();
  const compactMode = mode === 'compact';
  const [pinned, setPinned] = useState(true);
  const [pinPending, setPinPending] = useState(false);
  const [pinError, setPinError] = useState(false);
  const [snapshot, setSnapshot] = useState<Snapshot>();
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState(false);
  const [filter, setFilter] = useState<Filter>('all');
  const [days, setDays] = useState<1 | 7>(1);
  const [query, setQuery] = useState('');
  const [limit, setLimit] = useState(pageSize);
  const search = useRef<HTMLInputElement>(null);
  const scroll = useRef<HTMLDivElement>(null);
  const inFlight = useRef(false);
  const openingRef = useRef(false);
  const alerted = useRef(new Map<string, string>());
  const alertsReady = useRef(false);
  const scope = useRef<ListScope>({ mode: 'compact', watched: [] });

  const refresh = useCallback(async () => {
    if (inFlight.current) return;
    inFlight.current = true;
    setIsLoading(true);
    try {
      while (true) {
        const request = scope.current;
        const newest = await desk.list(request);
        if (request.mode !== scope.current.mode) continue;
        setSnapshot(newest);
        setTracked(previous => trackSessions(previous, newest));
        setError(false);
        break;
      }
    } catch {
      setError(true);
    } finally {
      setIsLoading(false);
      inFlight.current = false;
    }
  }, [desk]);

  useEffect(() => {
    scope.current = {
      mode,
      watched: [...tracked.values()].map(({ session, hiddenAt }) => ({
        id: session.id, provider: session.provider, nativeId: session.nativeId,
        hasTranscript: session.hasTranscript, hiddenAt,
      })),
    };
  }, [mode, tracked]);

  useEffect(() => {
    if (!persist) return;
    try { saveTracked(window.localStorage, tracked); setStorageError(false); }
    catch { setStorageError(true); }
  }, [tracked, persist]);

  useEffect(() => {
    if (!snapshot) return;
    const next = new Map<string, string>();
    for (const { session, restoring, hiddenAt } of tracked.values()) {
      if (!session.waiting || restoring || hiddenAt !== undefined) continue;
      const key = sessionKey(session);
      const stamp = `${session.waiting}:${session.waitingAt ?? ''}`;
      next.set(key, stamp);
      if (alerts && alertsReady.current && alerted.current.get(key) !== stamp) {
        desk.notifications?.send(session.waiting === 'approval' ? '승인 대기' : '입력 대기', session.title || providers[session.provider]);
      }
    }
    alerted.current = next;
    alertsReady.current = true;
  }, [alerts, desk, snapshot, tracked]);

  async function toggleAlerts() {
    const enabled = !alerts;
    if (enabled && !await desk.notifications?.permission()) throw new Error('macOS 알림 권한을 확인해 주세요.');
    if (persist) window.localStorage.setItem('session-desk.wait-alerts', enabled ? 'on' : 'off');
    setAlerts(enabled);
  }

  useEffect(() => {
    void refresh();
  }, [mode, refresh]);

  useEffect(() => {
    let current = true;
    desk.pin().then(value => { if (current) setPinned(value); }).catch(() => { if (current) setPinError(true); });
    desk.mode?.().then(value => { if (current) setMode(value); }).catch(() => { if (current) setModeError(true); });
    return () => { current = false; };
  }, [desk]);

  async function togglePin() {
    setPinPending(true);
    try {
      setPinned(await desk.pin(!pinned));
      setPinError(false);
    } catch {
      setPinError(true);
    } finally {
      setPinPending(false);
    }
  }

  async function openSession(item: Session) {
    if (openingRef.current) return;
    openingRef.current = true;
    const key = sessionKey(item);
    const clickedAt = clock();
    setOpening(key);
    setOpenError(undefined);
    try {
      const result = await desk.open(item.provider, item.id);
      if (result === 'terminal-permission' || result === 'terminal-unavailable') {
        setOpenError(result === 'terminal-permission'
          ? '시스템 설정에서 Offtab의 터미널 자동화 권한을 확인해 주세요.'
          : '원래 터미널 탭을 찾지 못했습니다. 세션이 열려 있는지 확인해 주세요.');
        return;
      }
      if (result === 'unavailable') {
        setOpenError(item.provider === 'claude'
          ? '연결된 Claude 채팅방이나 열려 있는 터미널 탭을 찾지 못했습니다.'
          : '이 세션의 Codex 채팅방을 찾을 수 없습니다.');
        return;
      }
      if (result === 'failed') throw new Error('Session could not be opened');
      setTracked(previous => markRead(previous, key, clickedAt));
    } catch {
      setOpenError('채팅방을 열지 못했습니다. 다시 눌러 주세요.');
    } finally {
      openingRef.current = false;
      setOpening(undefined);
    }
  }

  async function toggleMode() {
    if (!desk.mode || modePending) return;
    setModePending(true);
    try {
      const next = await desk.mode(compactMode ? 'full' : 'compact');
      setSnapshot(undefined);
      setMode(next);
      setModeError(false);
    } catch {
      setModeError(true);
    } finally {
      setModePending(false);
    }
  }

  useEffect(() => {
    const interval = window.setInterval(() => void refresh(), 5_000);
    const visible = () => { if (!document.hidden) void refresh(); };
    window.addEventListener('focus', refresh);
    document.addEventListener('visibilitychange', visible);
    document.addEventListener('desk-demo-change', refresh);
    return () => {
      window.clearInterval(interval);
      window.removeEventListener('focus', refresh);
      document.removeEventListener('visibilitychange', visible);
      document.removeEventListener('desk-demo-change', refresh);
    };
  }, [refresh]);

  useEffect(() => {
    const shortcuts = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'f') {
        event.preventDefault();
        search.current?.focus();
        search.current?.select();
      }
      if (event.key === 'Escape') {
        if (document.activeElement === search.current) setQuery('');
        setSettingsOpen(false);
      }
    };
    window.addEventListener('keydown', shortcuts);
    return () => window.removeEventListener('keydown', shortcuts);
  }, []);

  useEffect(() => {
    setLimit(pageSize);
    if (scroll.current) scroll.current.scrollTop = 0;
  }, [filter, days, query]);

  const now = snapshot?.refreshedAt ?? Date.now();
  const needle = query.trim().toLocaleLowerCase();
  const availableSessions = useMemo(
    () => snapshot?.sessions.filter(item => tracked.get(sessionKey(item))?.hiddenAt === undefined) ?? [],
    [snapshot, tracked],
  );
  const periodSessions = useMemo(() => availableSessions.filter(item => !item.contextOnly
    && (item.waiting || isWorking(item) || item.updatedAt >= now - days * day)), [availableSessions, now, days]);
  const parentSessions = useMemo(() => availableSessions.filter(item => item.provider === 'claude' && item.nativeId), [availableSessions]);
  function hide(item: Session) {
    const hiddenAt = clock();
    setTracked(previous => hideSession(previous, item, hiddenAt));
  }
  const totals = { all: periodSessions.length, codex: 0, claude: 0 };
  for (const item of periodSessions) totals[item.provider]++;
  const visible = periodSessions.filter(item =>
    (filter === 'all' || item.provider === filter) &&
    (!needle || [item.title, item.cwd, item.preview].some(text => text.toLocaleLowerCase().includes(needle))),
  ).sort((a, b) => Number(Boolean(b.waiting)) - Number(Boolean(a.waiting)));
  const tree = compactMode ? [] : sessionTree(
    visible.slice(0, limit).map(session => ({ session })),
    parentSessions.map(session => ({ session })),
  );
  const failedSources = snapshot?.sources.filter(source =>
    (compactMode || filter === 'all' || filter === source.provider) && ['error', 'partial'].includes(source.status),
  ) ?? [];
  const filterTitle = filter === 'all' ? '최근 세션' : providers[filter];
  const persistenceNotice = storageError ? '목록 상태를 저장할 수 없습니다.' : initial.invalid ? '이전에 저장한 목록 상태를 복원하지 못했습니다.' : undefined;

  return (
    <div className={`desk-mount app ${compactMode ? 'compact' : ''}`}>
      <header className="window-bar" data-tauri-drag-region>
        <span data-tauri-drag-region>{compactMode ? '진행 중 세션' : 'Offtab'}</span>
        <div className="window-actions">
          {(desk.hooks || desk.notifications) && <button type="button" className="window-mode" aria-label="세션 설정"
            aria-expanded={settingsOpen} title="세션 설정" onClick={() => setSettingsOpen(value => !value)}><Settings aria-hidden="true" /></button>}
          <button type="button" className="pin-window" aria-label="항상 위에 고정" aria-pressed={pinned}
            title={pinned ? '항상 위에 고정 해제' : '항상 위에 고정'} disabled={pinPending} onClick={() => void togglePin()}>
            {pinned ? <Pin aria-hidden="true" /> : <PinOff aria-hidden="true" />}
            {!compactMode && <span>{pinned ? '항상 위' : '고정 해제됨'}</span>}
          </button>
          {desk.mode && (
            <button type="button" className="window-mode" aria-label={compactMode ? '전체 창 열기' : '작은 창 열기'}
              title={compactMode ? '전체 창 열기' : '작은 창 열기'} disabled={modePending} onClick={() => void toggleMode()}>
              {compactMode ? <Maximize2 aria-hidden="true" /> : <Minimize2 aria-hidden="true" />}
            </button>
          )}
        </div>
        {settingsOpen && <DeskSettings desk={desk} alerts={alerts} onAlerts={toggleAlerts} />}
      </header>
      {compactMode ? (
        <CompactList sessions={tracked} parents={parentSessions} onOpen={item => void openSession(item)} opening={opening} loading={isLoading && !snapshot}
          onRead={item => setTracked(previous => markRead(previous, sessionKey(item), clock(), true))}
          onHide={hide}
          notice={openError ?? persistenceNotice ?? (modeError ? '창 크기를 전환하지 못했습니다.' : pinError ? '창 고정 상태를 확인하지 못했습니다.' : error || failedSources.length > 0 ? '일부 세션의 작업 상태를 확인하지 못했습니다.' : undefined)} />
      ) : <div className="workspace">
        <aside className="sidebar">
          <nav aria-label="세션 종류">
            {filters.map(value => (
              <button type="button" key={value} className={`nav-item ${value === filter ? 'selected' : ''}`}
                aria-pressed={value === filter} onClick={() => setFilter(value)}>
                <span className={`nav-icon ${value}`}><ProviderIcon provider={value} /></span>
                <span>{value === 'all' ? '전체 세션' : providers[value]}</span>
                {snapshot && <span className="count">{totals[value]}</span>}
              </button>
            ))}
          </nav>
          <div className="sources">
            {snapshot?.sources.map(source => (
              <div className="source" key={source.provider} title={`${providers[source.provider]} · ${statusLabels[source.status]}`}>
                <span className={`dot ${source.status}`} />
                <span>{providers[source.provider]}</span>
                <span className="sr-only">{statusLabels[source.status]}</span>
              </div>
            ))}
            <div className="local"><ShieldCheck aria-hidden="true" /> 로컬 기록만 읽음</div>
          </div>
        </aside>
        <main>
          <div className="main-header">
            <div className="heading">
              <h1>{filterTitle}</h1>
              <div className="range">
                <select aria-label="조회 기간" value={days} onChange={event => setDays(event.target.value === '7' ? 7 : 1)}>
                  <option value={1}>{periodLabels[1]}</option>
                  <option value={7}>{periodLabels[7]}</option>
                </select>
                <ChevronDown aria-hidden="true" />
              </div>
            </div>
            <div className="toolbar">
              <div className="search">
                <Search aria-hidden="true" />
                <input ref={search} type="search" aria-label="세션 검색" placeholder="제목, 프로젝트, 대화 검색"
                  value={query} onChange={event => setQuery(event.target.value)} />
                {query
                  ? <button className="clear" type="button" aria-label="검색어 지우기" onClick={() => { setQuery(''); search.current?.focus(); }}><X aria-hidden="true" /></button>
                  : <kbd aria-hidden="true"><Command /> F</kbd>}
              </div>
              <button type="button" className="refresh" aria-label="새로고침" title="새로고침" disabled={isLoading} onClick={() => void refresh()}>
                <RefreshCw aria-hidden="true" className={isLoading ? 'spin' : ''} />
              </button>
            </div>
          </div>
          <div className="scroll" ref={scroll} aria-busy={isLoading && !snapshot}>
            {(openError || persistenceNotice || modeError || pinError || error || failedSources.length > 0) && (
              <p className="notice" role="status">
                {openError ?? persistenceNotice ?? (modeError ? '창 크기를 전환하지 못했습니다.' : pinError ? '창 고정 상태를 확인할 수 없습니다.' : error
                  ? '목록을 새로고침하지 못했습니다. 잠시 뒤 다시 시도해 주세요.'
                  : `${failedSources.map(source => providers[source.provider]).join(' · ')} 기록 일부를 읽지 못했습니다. 새로고침해 주세요.`)}
              </p>
            )}
            {!snapshot && isLoading ? (
              <ul className="session-list" aria-label="세션을 불러오는 중">
                {[0, 1, 2].map(index => <li className="session-row skeleton" key={index}><i /><div><b /><b /><b /></div></li>)}
              </ul>
            ) : visible.length > 0 ? (
              <>
                <ul className="session-list" aria-label="세션 목록">
                  {tree.map(node => <SessionRow key={sessionKey(node.row.session)} tree={node} now={now}
                    opening={opening} onOpen={item => void openSession(item)} onHide={hide} searchQuery={needle} />)}
                </ul>
                {limit < visible.length && <button type="button" className="more" onClick={() => setLimit(value => value + pageSize)}>세션 더 보기</button>}
              </>
            ) : snapshot && (
              <div className="empty">
                {needle ? <Search aria-hidden="true" /> : <Archive aria-hidden="true" />}
                <span>{needle ? '검색 결과가 없습니다' : '이 기간에 표시할 세션이 없습니다'}</span>
                {!needle && days === 1 && <button type="button" onClick={() => setDays(7)}>최근 7일 보기</button>}
              </div>
            )}
          </div>
          <footer className="footer">
            <span>{snapshot ? `${visible.length}개 세션` : '세션 기록'}</span>
            {snapshot && <span>마지막 새로고침 <time dateTime={new Date(now).toISOString()}>{syncTime.format(now)}</time></span>}
          </footer>
        </main>
      </div>}
    </div>
  );
}
