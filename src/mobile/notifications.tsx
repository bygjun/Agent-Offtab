import { useEffect, useState } from 'react';
import type { Device } from './connection';

function pushKey(key: string) {
  const base64 = key.replace(/-/g, '+').replace(/_/g, '/');
  return Uint8Array.from(atob(base64 + '='.repeat((4 - base64.length % 4) % 4)), char => char.charCodeAt(0));
}

export function MobileNotifications({ device }: { device: Device }) {
  const [active, setActive] = useState(false);
  const [available, setAvailable] = useState(false);
  const [registration, setRegistration] = useState<ServiceWorkerRegistration>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const supported = 'serviceWorker' in navigator && 'PushManager' in window && 'Notification' in window;
  const endpoint = `/api/links/${device.id}/push`;

  async function saveSubscription(subscription: PushSubscription | null) {
    const response = await fetch(endpoint, {
      method: 'POST', credentials: 'same-origin', headers: { 'Content-Type': 'application/json' },
      signal: AbortSignal.timeout(5_000), body: JSON.stringify({ subscription }),
    });
    if (!response.ok) throw new Error('알림 설정을 중계 서버에 저장하지 못했습니다.');
  }

  useEffect(() => {
    if (!supported) return;
    let current = true;
    void fetch(endpoint, { cache: 'no-store', signal: AbortSignal.timeout(5_000) })
      .then(async response => {
        if (!response.ok || !current) return;
        const status: { enabled: boolean } = await response.json();
        const worker = await navigator.serviceWorker.register('/offtab-sw.js');
        if (!current) return;
        setAvailable(true);
        setRegistration(await navigator.serviceWorker.ready);
        if (current) setActive(status.enabled && Notification.permission === 'granted');
      }).catch(() => {});
    return () => { current = false; };
  }, [endpoint, supported]);

  async function toggle() {
    if (busy || !registration) return;
    setBusy(true);
    setError('');
    try {
      if (active) {
        await saveSubscription(null);
        await (await registration.pushManager.getSubscription())?.unsubscribe();
        setActive(false);
      } else {
        if (await Notification.requestPermission() !== 'granted') throw new Error('알림을 받으려면 기기의 Offtab 알림을 허용해 주세요.');
        const response = await fetch(endpoint, { cache: 'no-store', signal: AbortSignal.timeout(5_000) });
        if (!response.ok) throw new Error('중계 서버가 알림을 준비하지 못했습니다.');
        const { publicKey }: { publicKey: string } = await response.json();
        const subscription = await registration.pushManager.getSubscription() ??
          await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: pushKey(publicKey) });
        await saveSubscription(subscription);
        setActive(true);
      }
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : '알림 설정을 변경하지 못했습니다.');
    } finally {
      setBusy(false);
    }
  }

  return <details className="mobile-hidden-tools">
    <summary>입력·승인 대기 알림</summary>
    {supported && available ? <button type="button" disabled={busy || !registration} onClick={() => void toggle()}>
      {active ? '알림 끄기' : '알림 켜기'}
    </button> : <p>iPhone에서는 Offtab을 홈 화면에 추가한 뒤, 홈 화면에서 열어 알림을 설정하세요.</p>}
    {error && <p role="status">{error}</p>}
  </details>;
}
