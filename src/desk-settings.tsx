import { useEffect, useState } from 'react';
import type { DeskApi, HookStatus, Provider } from './types';
import { providerNames } from './provider-logo';
import { MobileSettings } from './mobile-settings';

const providers = ['codex', 'claude'] as const;
const statusLabel = { off: '연결 안 됨', connected: '설정됨', invalid: '설정 확인 필요' };

export function DeskSettings({ desk, alerts, onAlerts }: { desk: DeskApi; alerts: boolean; onAlerts: () => Promise<void> }) {
  const [hooks, setHooks] = useState<Partial<Record<Provider, HookStatus>>>({});
  const [busy, setBusy] = useState<Provider | 'alerts'>();
  const [mobileOpen, setMobileOpen] = useState(false);
  const [notice, setNotice] = useState<string>();
  const showCodexGuide = hooks.codex === 'off' || hooks.codex === 'connected';
  useEffect(() => {
    let current = true;
    for (const provider of providers) {
      desk.hooks?.(provider).then(value => {
        if (current) setHooks(previous => ({ ...previous, [provider]: value }));
      }).catch(() => { if (current) setHooks(previous => ({ ...previous, [provider]: 'invalid' })); });
    }
    return () => { current = false; };
  }, [desk]);

  async function toggle(provider: Provider) {
    if (!desk.hooks || busy) return;
    setBusy(provider);
    setNotice(undefined);
    try {
      const enabled = hooks[provider] !== 'connected';
      const result = await desk.hooks(provider, enabled);
      setHooks(previous => ({ ...previous, [provider]: result }));
    } catch {
      setNotice('연결 설정을 저장하지 못했습니다. 설정 파일을 확인해 주세요.');
    } finally {
      setBusy(undefined);
    }
  }

  return <section className="desk-settings" aria-label="세션 설정">
    {desk.hooks && <>
      <strong>실시간 대기 감지</strong>
      {providers.map(provider => <div className="hook-setting" key={provider}>
        <div className="settings-line">
          <span>{providerNames[provider]}{hooks[provider] && ` · ${provider === 'codex' && hooks[provider] === 'connected' ? '훅 등록됨' : statusLabel[hooks[provider]]}`}</span>
          <button type="button" disabled={Boolean(busy) || !hooks[provider] || hooks[provider] === 'invalid'}
            aria-label={`${providerNames[provider]} 대기 감지 ${hooks[provider] === 'connected' ? '해제' : '연결'}`}
            aria-describedby={provider === 'codex' && showCodexGuide ? 'codex-hook-guide' : undefined}
            onClick={() => void toggle(provider)}>{hooks[provider] === 'connected' ? '해제' : '연결'}</button>
        </div>
        {provider === 'codex' && showCodexGuide && <p id="codex-hook-guide" aria-live="polite">
          {hooks.codex === 'connected'
            ? <>Codex의 <strong>설정 → 훅(Hooks)</strong>에서 Offtab의 <code>hook-helper</code>를 승인하세요. 터미널에서는 <code>/hooks</code>를 여세요.</>
            : '연결 후 Codex에서 훅을 승인하면 대기 감지가 시작됩니다.'}
        </p>}
      </div>)}
    </>}
    {desk.notifications && <label className="settings-line">
      입력·승인 대기 알림
      <input type="checkbox" checked={alerts} disabled={Boolean(busy)} onChange={() => {
        setBusy('alerts');
        setNotice(undefined);
        void onAlerts().catch(error => setNotice(error instanceof Error ? error.message : '알림을 설정하지 못했습니다.'))
          .finally(() => setBusy(undefined));
      }} />
    </label>}
    {desk.mobile && <details className="mobile-settings-section" onToggle={event => setMobileOpen(event.currentTarget.open)}>
      <summary>휴대폰 연결</summary>
      {mobileOpen && <MobileSettings mobile={desk.mobile} />}
    </details>}
    {notice && <p role="status">{notice}</p>}
  </section>;
}
