import { useEffect, useRef, useState } from 'react';
import { toCanvas } from 'qrcode';
import type { DeskApi } from './types';

type MobileApi = NonNullable<DeskApi['mobile']>;

function PairingCode({ url }: { url: string }) {
  const canvas = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    if (canvas.current) void toCanvas(canvas.current, url, { width: 288, errorCorrectionLevel: 'M' });
  }, [url]);

  return <>
    <canvas className="mobile-qr" ref={canvas} role="img" aria-label="휴대폰 연결 QR 코드" />
    <details className="mobile-link">
      <summary>연결 주소 보기</summary>
      <input aria-label="연결 주소" readOnly value={url} onFocus={event => event.currentTarget.select()} />
    </details>
  </>;
}

export function MobileSettings({ mobile }: { mobile: MobileApi }) {
  const [overview, setOverview] = useState<Awaited<ReturnType<MobileApi['connection']>>>();
  const connections = overview?.devices;
  const localPreview = overview?.localPreview === true;
  const [label, setLabel] = useState('');
  const [address, setAddress] = useState<string>();
  const [localAddress, setLocalAddress] = useState<string>();
  const [ready, setReady] = useState<boolean>();
  const [checked, setChecked] = useState(false);
  const connection = connections?.find(item => item.phase === 'scan' || item.phase === 'pin');
  const devices = connections?.filter(item => item !== connection) ?? [];
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState('');
  const [statusFailed, setStatusFailed] = useState(false);
  const action = useRef(false);

  useEffect(() => {
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    async function refresh() {
      if (!action.current) {
        try {
          const status = await mobile.connection();
          if (!disposed && !action.current) {
            setOverview(status);
            setStatusFailed(false);
          }
        } catch {
          if (!disposed && !action.current) setStatusFailed(true);
        }
      }
      if (!disposed) timer = setTimeout(refresh, 900);
    }
    void refresh();
    return () => { disposed = true; clearTimeout(timer); };
  }, [mobile]);

  const canPair = connections !== undefined && !connection && devices.length < 4;
  const paired = devices.filter(device => device.phase === 'paired');
  const allEnabled = paired.length > 0 && paired.every(device => device.replyEnabled);

  async function change(options: Parameters<MobileApi['connection']>[0]) {
    action.current = true;
    setBusy(true);
    setNotice('');
    try {
      const result = await mobile.connection(options);
      setOverview(result);
      setStatusFailed(false);
      if (options?.checkRelay) {
        setChecked(true);
        setReady(result.reachable === true && result.devices.some(device => device.phase === 'paired' && !device.interrupted));
      }
      if (options?.relayUrl) {
        setAddress(undefined);
        setLocalAddress(undefined);
      }
      if (options?.enabled === true) {
        setLabel('');
        const pending = result.devices.find(device => device.phase === 'scan');
        if (result.localPreview && pending) await mobile.openPreview(pending.id);
      }
    } catch (error) {
      setNotice(typeof error === 'string' ? error : '휴대폰 연결 상태를 변경하지 못했습니다.');
    } finally {
      action.current = false;
      setBusy(false);
    }
  }

  async function openPreview(device: string) {
    setBusy(true);
    setNotice('');
    try {
      await mobile.openPreview(device);
    } catch (error) {
      setNotice(typeof error === 'string' ? error : '브라우저를 열지 못했습니다.');
    } finally {
      setBusy(false);
    }
  }

  const phase = connection?.phase;

  return <div className="mobile-setting">
    {connections === undefined && !statusFailed && <p>연결 상태 확인 중</p>}
    <details className="mobile-relay-address">
      <summary>중계 주소와 연결 점검</summary>
      <label className="mobile-label">휴대폰 접속 주소
        <input type="url" value={address ?? overview?.relayUrl ?? ''} onChange={event => setAddress(event.currentTarget.value)}
          disabled={busy} placeholder="https://offtab.example.com/" />
      </label>
      <label className="mobile-label">Mac 내부 중계 주소
        <input type="url" value={localAddress ?? overview?.localRelayUrl ?? ''} onChange={event => setLocalAddress(event.currentTarget.value)}
          disabled={busy} placeholder="http://127.0.0.1:63915/" />
      </label>
      <button type="button" disabled={busy || !(address ?? overview?.relayUrl)}
        onClick={() => void change({ relayUrl: address ?? overview?.relayUrl, localRelayUrl: localAddress ?? overview?.localRelayUrl ?? '', checkRelay: true })}>
        주소 저장 및 연결 점검
      </button>
      {checked && !notice && <p role="status">{ready
        ? '원격 연결 준비됨. Mac이 켜져 있으면 휴대폰에서 연결할 수 있습니다.'
        : '원격 연결 준비가 필요합니다. HTTPS 주소와 등록된 기기 상태를 확인하세요.'}</p>}
    </details>
    {overview?.claudeDesktopAccess === false && mobile.openClaudeAccess && paired.some(device => device.replyEnabled) &&
      <div className="settings-line">
        <span>Claude Desktop 답장</span>
        <button type="button" disabled={busy} onClick={() => {
          void mobile.openClaudeAccess?.().catch(error => setNotice(
            typeof error === 'string' ? error : '시스템 설정 → 개인정보 보호 및 보안 → 손쉬운 사용에서 Offtab을 허용해 주세요.',
          ));
        }}>손쉬운 사용 권한 요청</button>
      </div>}
    {paired.length > 1 && <button type="button" disabled={busy}
      onClick={() => void change({ allDevices: true, replyEnabled: !allEnabled })}>
      {allEnabled ? '모든 기기 답장 끄기' : '모든 기기 답장 허용'}
    </button>}
    {devices.map(device => <div className="mobile-device" key={device.id}>
      <div className="settings-line">
        <strong>{device.label}</strong>
        <button type="button" disabled={busy || device.phase === 'revoking'} aria-label={`${device.label} 연결 해제`}
          onClick={() => void change({ enabled: false, device: device.id })}>
          {device.phase === 'revoking' ? '해제 중' : device.phase === 'paired' ? '연결 해제' : '정리'}
        </button>
      </div>
      {device.phase === 'paired' ? <>
        <label className="settings-line">
          {device.label} 답장 허용
          <input type="checkbox" checked={device.replyEnabled} disabled={busy}
            onChange={event => void change({ device: device.id, replyEnabled: event.currentTarget.checked })} />
        </label>
        {device.interrupted && !notice && <p role="status">중계 서버와 통신이 끊겼습니다.</p>}
      </> : <p role="status">{device.phase === 'revoking' ? '중계 서버에 연결되면 해제를 완료합니다.'
        : device.phase === 'locked' ? 'PIN 입력 횟수를 초과했습니다.' : '연결이 만료되었습니다.'}</p>}
    </div>)}
    {canPair && <>
      <p>{localPreview
        ? '웹 브라우저에서 휴대폰 화면을 테스트합니다.'
        : '연결하면 이 Mac의 모든 프로젝트에서 최근 세션을 볼 수 있습니다.'}</p>
      <label className="mobile-label">기기 이름 <input value={label} placeholder="예: 내 아이폰" maxLength={30}
        disabled={busy} onChange={event => setLabel(event.currentTarget.value)} /></label>
      <button type="button" disabled={busy} onClick={() => void change({ enabled: true, label })}>
        {busy ? '연결 준비 중' : localPreview ? '브라우저에서 미리보기' : 'QR 만들기'}
      </button>
    </>}
    {connection && <>
      {phase === 'scan' && connection.url && (localPreview
        ? <>
          <p>브라우저가 열리면 여기에 PIN이 표시됩니다.</p>
          <button type="button" disabled={busy} onClick={() => void openPreview(connection.id)}>브라우저 다시 열기</button>
        </>
        : <>
          <p>휴대폰 카메라로 QR을 찍으세요.</p>
          <PairingCode url={connection.url} />
        </>)}
      {phase === 'pin' && connection.pin && <>
        <p>{localPreview ? '브라우저에 이 PIN을 입력하세요.' : '휴대폰에 이 PIN을 입력하세요.'}</p>
        <strong className="mobile-pin" aria-label={`PIN ${connection.pin.split('').join(' ')}`}>{connection.pin}</strong>
      </>}
      {phase === 'pin' && !connection.pin && <p role="status">휴대폰 연결 확인 중입니다.</p>}
      {connection.interrupted && !notice && <p role="status">중계 서버와 통신이 끊겼습니다.</p>}
      <button type="button" disabled={busy} onClick={() => void change({ enabled: false, device: connection.id })}>
        {busy ? '연결 취소 중' : '연결 취소'}
      </button>
    </>}
    {statusFailed && !notice && <p role="status">휴대폰 연결 상태를 확인하지 못했습니다.</p>}
    {notice && <p role="status">{notice}</p>}
  </div>;
}
