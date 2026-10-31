/** Shapes returned by the unsonido API (see src/api.rs and the types it serializes). */

export type Priority = "notification" | "critical";

export interface AlertInfo {
  id: string;
  priority: Priority;
  label: string;
  namespace?: string;
  name?: string;
}

export interface HistoryEntry extends AlertInfo {
  result: string;
  at: string;
}

export type StoredState = "playing" | "pending" | "scheduled";

export interface StoredAlert {
  namespace: string;
  name: string;
  priority: Priority;
  label: string;
  id?: string;
  state: StoredState;
  /** Present while the alert waits for its moment. */
  due_at?: string;
}

export interface NamespaceInfo {
  name: string;
  alerts: StoredAlert[];
}

export interface NowPlaying {
  title?: string | null;
  artist?: string | null;
  album?: string | null;
}

export interface ReceiverStatus {
  name: string;
  port: number;
  client?: string | null;
  playing: boolean;
  volume: number;
  volume_synced?: boolean | null;
  muted: boolean;
  now_playing: NowPlaying | null;
  remote_control: boolean;
  paused_for_alert: boolean;
}

export interface BackgroundStatus {
  url?: string | null;
  urls?: string[];
  file?: string;
  gain_db: number;
  playing: boolean;
  title?: string | null;
  state: string;
  current_url?: string | null;
}

export interface Status {
  sample_rate: number;
  volume: number;
  current: AlertInfo | null;
  queue: AlertInfo[];
  history: HistoryEntry[];
  namespaces: NamespaceInfo[];
  background: BackgroundStatus | null;
  airplay: ReceiverStatus[];
}

export interface ProviderInfo {
  name: string;
  kind: string;
  default: boolean;
}

/** POST /alerts and friends answer with a tagged outcome. */
export type Outcome =
  | { status: "queued"; id: string; provider?: string; duration_ms: number }
  | { status: "scheduled"; id: string; due_at: string }
  | { status: "unchanged"; id: string }
  | { status: "suppressed"; id: string; reason: string };

export interface SyncReport {
  namespace: string;
  results: Record<string, Outcome>;
  pruned: string[];
}