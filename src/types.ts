export type Provider = 'codex' | 'claude';
export type Waiting = 'input' | 'approval';
export type ReadStatus = { version: string; at?: number };
export type ReadUpdate = { provider: Provider; id: string; version: string; at: number };

export interface Session {
  id: string;
  provider: Provider;
  title: string;
  preview: string;
  role?: 'user' | 'assistant';
  cwd: string;
  updatedAt: number;
  active?: boolean;
  waiting?: Waiting;
  waitingAt?: number;
  waitingDetails?: { questions?: { question: string; options: string[] }[]; tool?: string };
  choiceToken?: string;
  background?: number;
  recentlyActive?: boolean;
  activityAt?: number;
  lastUserAt?: number;
  nativeId?: string;
  parentNativeId?: string;
  contextOnly?: boolean;
  hasTranscript?: boolean;
  read?: ReadStatus;
}

export interface Source {
  provider: Provider;
  status: 'ready' | 'missing' | 'partial' | 'error';
}

export interface Snapshot {
  sessions: Session[];
  removed?: Pick<Session, 'provider' | 'id'>[];
  sources: Source[];
  refreshedAt: number;
}

export const day = 86_400_000;

export type OpenResult = 'opened' | 'unavailable' | 'failed' | 'terminal-permission' | 'terminal-unavailable';

export type HookStatus = 'off' | 'connected' | 'invalid';

export interface MobileConnection {
  id: string;
  label: string;
  replyEnabled: boolean;
  phase: 'scan' | 'pin' | 'paired' | 'locked' | 'expired' | 'revoking';
  url: string | null;
  pin: string | null;
  interrupted: boolean;
}

export interface DeskApi {
  list(scope: ListScope): Promise<Snapshot>;
  publishCompact?(view: CompactView): Promise<void>;
  open(provider: Provider, id: string): Promise<OpenResult>;
  syncReads?(updates: ReadUpdate[]): Promise<void>;
  pin(value?: boolean): Promise<boolean>;
  mode?(value?: WindowMode): Promise<WindowMode>;
  hooks?(provider: Provider, enabled?: boolean): Promise<HookStatus>;
  mobile?: {
    connection(options?: { enabled?: boolean; device?: string; label?: string; replyEnabled?: boolean; allDevices?: boolean; relayUrl?: string; localRelayUrl?: string; checkRelay?: boolean }): Promise<{ localPreview: boolean; devices: MobileConnection[]; relayUrl?: string; localRelayUrl?: string | null; reachable?: boolean | null; claudeDesktopAccess?: boolean | null }>;
    openPreview(device: string): Promise<void>;
    openClaudeAccess?(): Promise<void>;
  };
  notifications?: { permission(): Promise<boolean>; send(title: string, body: string): void };
}

export interface CompactPosition {
  workAt: number;
  readAt?: number;
}

export interface CompactView {
  entries: (CompactPosition & { session: Session })[];
  parents: Session[];
}

export const deskWindow = { width: 1040, height: 740, minWidth: 360, minHeight: 360 };

export const compactWindow = { width: 420, height: 280, minWidth: 290, minHeight: 162 };
export type WindowMode = 'compact' | 'full';

export interface ListScope {
  mode: WindowMode;
  watched: (Pick<Session, 'id' | 'provider' | 'nativeId' | 'hasTranscript'> & { hiddenAt?: number })[];
}
