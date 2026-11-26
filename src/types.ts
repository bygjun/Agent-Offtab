export type Provider = 'codex' | 'claude';
export type Waiting = 'input' | 'approval';

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
  background?: number;
  recentlyActive?: boolean;
  activityAt?: number;
  lastUserAt?: number;
  nativeId?: string;
  parentNativeId?: string;
  contextOnly?: boolean;
  hasTranscript?: boolean;
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

export interface DeskApi {
  list(scope: ListScope): Promise<Snapshot>;
  open(provider: Provider, id: string): Promise<OpenResult>;
  pin(value?: boolean): Promise<boolean>;
  mode?(value?: WindowMode): Promise<WindowMode>;
  hooks?(provider: Provider, enabled?: boolean): Promise<HookStatus>;
  notifications?: { permission(): Promise<boolean>; send(title: string, body: string): void };
}

export const deskWindow = { width: 1040, height: 740, minWidth: 360, minHeight: 360 };

export const compactWindow = { width: 420, height: 280, minWidth: 290, minHeight: 162 };
export type WindowMode = 'compact' | 'full';

export interface ListScope {
  mode: WindowMode;
  watched: (Pick<Session, 'id' | 'provider' | 'nativeId' | 'hasTranscript'> & { hiddenAt?: number })[];
}
