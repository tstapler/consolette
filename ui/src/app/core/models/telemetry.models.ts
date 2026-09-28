export interface MetricsTickData {
  rpm: number;
  totalRequests: number;
  tokensSaved: number;
  tps?: number;
  medianTtftMs?: number;
  tokenSavingsPercent?: number;
  currentLagMs: number;
  providerHealth?: Record<string, 'Healthy' | 'Degraded' | 'Cooldown' | 'AuthError' | string>;
}

export interface RequestTraceData {
  id: string;
  requestId?: string;
  timestamp: string;
  provider: string;
  model: string;
  durationMs: number;
  firstByteMs: number;
  tokensBefore: number;
  tokensAfter: number;
  compressionStatus: string;
  statusCode: number;
}

export interface ErrorLoggedData {
  providerId: string;
  statusCode: number;
  errorDetails: string;
  timestamp: string;
}

export interface ConfigChangedData {
  timestamp: string;
  changedKeys?: string[];
}

export type DashboardEvent =
  | { type: 'MetricsTick'; data: MetricsTickData }
  | { type: 'RequestTrace'; data: RequestTraceData }
  | { type: 'ErrorLogged'; data: ErrorLoggedData }
  | { type: 'ConfigChanged'; data: ConfigChangedData }
  | { type: 'system_lag'; data: { message: string; skipped: number } };

export interface SessionData {
  id: string;
  turnCount: number;
  tokenSavingsPercent: number;
  lastActiveTimestamp: string;
  pinned: boolean;
  model: string;
  provider: string;
}

export interface BenchmarkData {
  model: string;
  provider: string;
  ttftP50: number;
  ttftP90: number;
  ttftP95: number;
  ttftP99: number;
  durationP50?: number;
  durationP90?: number;
  durationP95?: number;
  durationP99?: number;
  speedTokSec: number;
  successRatePercent: number;
  errorRate?: number;
  costPer1k: number;
  tokensSavedPercent: number;
  aiderScore?: number;
}

export interface ConfigData {
  webUi?: 'angular' | 'legacy';
  providers?: Record<string, { apiKey?: string; baseUrl?: string; weight?: number }>;
  rateLimits?: { rpm?: number; tpm?: number; concurrent?: number };
  fallbackCascade?: string[];
}
