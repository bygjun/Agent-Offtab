import { invoke } from '@tauri-apps/api/core';
import { isPermissionGranted, requestPermission, sendNotification } from '@tauri-apps/plugin-notification';
import { createRoot } from 'react-dom/client';
import { App } from './app';
import type { DeskApi } from './types';

const desk: DeskApi = {
  list: scope => invoke('list_sessions', { scope }),
  open: (provider, id) => invoke('open_session', { provider, id }),
  syncReads: updates => invoke('mark_sessions_read', { updates }),
  mode: value => invoke('window_mode', { value }),
  pin: value => invoke('window_pin', { value }),
  hooks: (provider, enabled) => invoke('session_hooks', { provider, enabled }),
  mobile: {
    connection: options => invoke('mobile_connection', options),
    openPreview: device => invoke('mobile_open_preview', { device }),
    openClaudeAccess: () => invoke('mobile_open_claude_access'),
  },
  notifications: {
    async permission() { return await isPermissionGranted() || await requestPermission() === 'granted'; },
    send(title, body) { sendNotification({ title, body }); },
  },
};

createRoot(document.getElementById('root')!).render(<App desk={desk} persist />);
