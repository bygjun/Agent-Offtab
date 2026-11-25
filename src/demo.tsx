import { useLayoutEffect, useMemo, useRef, useState } from 'react';
import type { KeyboardEvent, PointerEvent } from 'react';
import { createRoot } from 'react-dom/client';
import { AppWindow, Clock3, FileText, Grip, Layers2, RotateCcw } from 'lucide-react';
import { App } from './app';
import { ProviderLogo } from './provider-logo';
import { compactWindow, type DeskApi, type Session, type Waiting } from './types';
import './demo.css';

type Edges = 'right' | 'bottom' | 'corner';
type Size = { width: number; height: number };
type Drag = { edge: Edges; id: number; x: number; y: number; size: Size };

const examples: (Omit<Session, 'updatedAt'> & { minutesAgo: number })[] = [
  {
    id: 'one', provider: 'codex', title: '재고 대시보드 조회 속도 개선', cwd: '/demo/warehouse',
    preview: '재고 집계를 한 번만 실행하도록 쿼리를 수정했습니다. 상품 목록과 검색 결과가 이전과 동일하게 표시되는지 확인하고 있습니다.', role: 'assistant', minutesAgo: 4,
  },
  {
    id: 'two', provider: 'claude', title: '진료 기록 사진 업로드 검증', cwd: '/demo/clinic-web',
    preview: '카메라 촬영과 파일 선택으로 추가한 사진을 확인했습니다. 큰 이미지를 올릴 때 진행 상태가 유지되는지 검증하겠습니다.', role: 'assistant', minutesAgo: 12,
  },
  {
    id: 'three', provider: 'codex', title: '예약 내역에서 환불 상태 표시', cwd: '/demo/booking-web',
    preview: '예약 상세에서 결제 취소와 부분 환불 상태가 표시됩니다. 날짜가 다른 예약에서도 같은 상태가 나오는지 확인했습니다.', role: 'assistant', minutesAgo: 35,
  },
  {
    id: 'four', provider: 'claude', title: '주간 실험 결과 비교', cwd: '/demo/research',
    preview: '이번 주 실험 결과를 모델과 데이터셋 기준으로 나눠서 보여줘. 지난주 대비 수치가 달라진 항목도 표시해줘.', role: 'user', minutesAgo: 88,
  },
  {
    id: 'five', provider: 'codex', title: '알림 설정 화면 검토', cwd: '/demo/settings',
    preview: '알림을 끈 뒤 다시 화면을 열어도 설정이 유지되는지 확인했습니다.', role: 'assistant', minutesAgo: 258,
  },
  {
    id: 'six', provider: 'claude', title: '월별 주문 내역 정리', cwd: '/demo/commerce',
    preview: '취소 주문을 따로 표시하고 월별 합계가 맞는지 검증했습니다.', role: 'assistant', minutesAgo: 2680,
  },
];

const edgeLabels: Record<Edges, string> = { right: '가로 크기 조절', bottom: '세로 크기 조절', corner: '가로와 세로 크기 조절' };
const defaultSize = { width: compactWindow.width, height: compactWindow.height };

function bound(size: Size, max: Size) {
  const width = Math.min(compactWindow.minWidth, max.width);
  const height = Math.min(compactWindow.minHeight, max.height);
  return {
    width: Math.max(width, Math.min(max.width, size.width)),
    height: Math.max(height, Math.min(max.height, size.height)),
  };
}

function Demo() {
  const stage = useRef<HTMLDivElement>(null);
  const origin = useRef<HTMLDivElement>(null);
  const drag = useRef<Drag | null>(null);
  const pin = useRef(true);
  const [started] = useState(Date.now);
  const offset = useRef(0);
  const [working, setWorking] = useState(true);
  const exampleWork = useRef({ active: true, changedAt: started - 4 * 60_000 });
  const exampleWaiting = useRef<{ kind?: Waiting; at?: number }>({});
  const examplePreview = useRef(examples[0].preview);
  const clock = () => Date.now() + offset.current;
  const [top, setTop] = useState(true);
  const [opened, setOpened] = useState<(typeof examples)[number]>();
  const [front, setFront] = useState<'desk' | 'work'>('desk');
  const [desired, setDesired] = useState(defaultSize);
  const [space, setSpace] = useState<Size>({ width: 1440, height: 860 });
  const shown = bound(desired, space);
  const deskFirst = top || front === 'desk';

  const desk = useMemo<DeskApi>(() => ({
    async list() {
      return {
        sessions: examples.map(({ minutesAgo, ...entry }) => {
          const initialAt = started - minutesAgo * 60_000;
          const updatedAt = entry.id === 'one' ? exampleWork.current.changedAt : initialAt;
          return { ...entry, updatedAt, activityAt: updatedAt,
            preview: entry.id === 'one' ? examplePreview.current : entry.preview,
            active: entry.id === 'one' ? exampleWork.current.active : entry.id === 'two',
            waiting: entry.id === 'two' ? exampleWaiting.current.kind : undefined,
            waitingAt: entry.id === 'two' ? exampleWaiting.current.at : undefined,
          };
        }),
        sources: [{ provider: 'codex', status: 'ready' }, { provider: 'claude', status: 'ready' }],
        refreshedAt: clock(),
      };
    },
    async open(provider, id) {
      const entry = examples.find(item => item.provider === provider && item.id === id);
      if (!entry) return 'unavailable';
      setOpened(entry);
      setFront('work');
      return 'opened';
    },
    async pin(value) {
      if (value !== undefined) {
        pin.current = value;
        setTop(value);
      }
      return pin.current;
    },
  }), [started]);

  useLayoutEffect(() => {
    const container = stage.current!;
    const anchor = origin.current!;
    const measure = () => setSpace({
      width: container.clientWidth - anchor.offsetLeft - 16,
      height: container.clientHeight - anchor.offsetTop - 16,
    });
    const observer = new ResizeObserver(measure);
    observer.observe(container);
    measure();
    return () => observer.disconnect();
  }, []);

  function simulateWork() {
    const active = !exampleWork.current.active;
    exampleWork.current = { active, changedAt: active ? clock() : exampleWork.current.changedAt };
    setWorking(active);
    document.dispatchEvent(new Event('desk-demo-change'));
  }

  function advanceTime() {
    offset.current += 60 * 60_000;
    document.dispatchEvent(new Event('desk-demo-change'));
  }

  function simulateWaiting() {
    const kind = exampleWaiting.current.kind === 'approval' ? 'input' : exampleWaiting.current.kind === 'input' ? undefined : 'approval';
    exampleWaiting.current = { kind, at: kind ? clock() : undefined };
    document.dispatchEvent(new Event('desk-demo-change'));
  }

  function simulatePreview() {
    examplePreview.current = examplePreview.current === examples[0].preview
      ? '같은 검색 조건으로 상품 목록을 다시 비교했습니다. 다음으로 재고가 없는 상품과 페이지를 넘길 때의 응답 시간을 확인하고 있습니다.' : examples[0].preview;
    exampleWork.current.changedAt = clock();
    document.dispatchEvent(new Event('desk-demo-change'));
  }

  function startResize(event: PointerEvent<HTMLButtonElement>, edge: Edges) {
    if (event.button !== 0 || drag.current) return;
    drag.current = { edge, id: event.pointerId, x: event.clientX, y: event.clientY, size: shown };
    event.currentTarget.setPointerCapture(event.pointerId);
  }

  function moveResize(event: PointerEvent<HTMLButtonElement>) {
    const current = drag.current;
    if (!current || current.id !== event.pointerId) return;
    setDesired(bound({
      width: current.size.width + (current.edge === 'bottom' ? 0 : event.clientX - current.x),
      height: current.size.height + (current.edge === 'right' ? 0 : event.clientY - current.y),
    }, space));
  }

  function stopResize(event: PointerEvent<HTMLButtonElement>) {
    if (drag.current?.id === event.pointerId) drag.current = null;
  }

  function keyResize(event: KeyboardEvent<HTMLButtonElement>, edge: Edges) {
    const step = event.shiftKey ? 48 : 12;
    const x = event.key === 'ArrowLeft' ? -step : event.key === 'ArrowRight' ? step : 0;
    const y = event.key === 'ArrowUp' ? -step : event.key === 'ArrowDown' ? step : 0;
    const dx = edge === 'bottom' ? 0 : x;
    const dy = edge === 'right' ? 0 : y;
    if (!dx && !dy) return;
    event.preventDefault();
    setDesired(bound({ width: shown.width + dx, height: shown.height + dy }, space));
  }

  return (
    <div className="demo-page">
      <header className="demo-header">
        <div className="demo-identity">
          <span className="demo-mark"><Layers2 aria-hidden="true" /></span>
          <div>
            <h1>Offtab</h1>
            <p>최소화 모드 · 웹 데모</p>
          </div>
        </div>
        <div className="demo-controls">
          <button type="button" onClick={() => setFront(front === 'desk' ? 'work' : 'desk')}>
            <AppWindow aria-hidden="true" />{front === 'desk' ? '작업 창 앞으로' : '세션 창 앞으로'}
          </button>
          <button type="button" title={`${defaultSize.width} × ${defaultSize.height}`} onClick={() => setDesired(defaultSize)}>
            <RotateCcw aria-hidden="true" />기본 크기
          </button>
        </div>
      </header>
      <div className="demo-test">
        <span>세션을 눌러 예시 채팅방 열기</span>
        <div className="demo-controls">
          <button type="button" onClick={simulateWork}>{working ? '예시 작업 완료' : '새 작업 시작'}</button>
          <button type="button" onClick={simulateWaiting}>대기 상태 전환</button>
          <button type="button" onClick={simulatePreview}>미리보기 변경</button>
          <button type="button" onClick={advanceTime}><Clock3 aria-hidden="true" />1시간 뒤</button>
        </div>
      </div>
      <div className="demo-caption">
        <span>오른쪽·아래 테두리 또는 모서리로 크기 조절</span>
        <output className="size-readout" aria-live="off">{shown.width} × {shown.height}</output>
      </div>
      <div className="demo-stage" ref={stage}>
        <section className={`work-window ${deskFirst ? '' : 'foreground'}`} aria-label="예시 작업 창"
          onPointerDown={() => setFront('work')}>
          <header className="work-chrome">
            <span className="traffic" aria-hidden="true"><i /><i /><i /></span>{opened ? '예시 채팅방' : '작업 노트'}
          </header>
          <div className="work-body" aria-live="polite">
            {opened ? (
              <>
                <span className={`work-logo ${opened.provider}`}><ProviderLogo provider={opened.provider} /></span>
                <h2 className="work-open-title">{opened.title}</h2>
                <p className="work-preview">{opened.preview}</p>
              </>
            ) : (
              <>
                <FileText aria-hidden="true" />
                <h2>오늘 할 일</h2>
                <ul>
                  <li>재고 조회 변경 확인</li>
                  <li>사진 업로드 검증</li>
                  <li>실험 결과 정리</li>
                </ul>
              </>
            )}
          </div>
        </section>
        <div ref={origin} className={`floating-desk ${deskFirst ? 'foreground' : ''}`}
          style={{ width: shown.width, height: shown.height }} onPointerDown={() => setFront('desk')} onFocus={() => setFront('desk')}>
          <App desk={desk} clock={clock} />
          <span className="traffic desk-traffic" aria-hidden="true"><i /><i /><i /></span>
          {(['right', 'bottom', 'corner'] as const).map(edge => (
            <button
              key={edge} className={`resize-handle resize-${edge}`} type="button"
              aria-label={edgeLabels[edge]} title={edgeLabels[edge]}
              onPointerDown={event => startResize(event, edge)} onPointerMove={moveResize}
              onPointerUp={stopResize} onPointerCancel={stopResize} onLostPointerCapture={stopResize}
              onKeyDown={event => keyResize(event, edge)}>
              {edge === 'corner' && <Grip aria-hidden="true" />}
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}

createRoot(document.getElementById('session-desk-demo')!).render(<Demo />);
