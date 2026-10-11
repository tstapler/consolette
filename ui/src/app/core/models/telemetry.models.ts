/** Field names mirror the backend wire contract, pinned by `src/testing/fixtures/sse/*.json` (ADR-001). */
export interface MetricsTickData {
  rpm: number;
  totalRequests: number;
  totalTokensSaved: number;
  currentLagMs: number;
  providerHealth?: Record<string, string>;
  // No backend source yet; read by the Overview tiles, which tolerate absence.
  tps?: number;
  medianTtftMs?: number;
  tokenSavingsPercent?: number;
}

export type CompressionStatus = 'off' | 'none' | 'skipped' | 'failed' | 'compressed';
export type TraceKind = 'routed' | 'cache_hit' | 'local';
export type FirstByteState = 'pending' | 'measured' | 'ended_empty' | 'client_closed';

export type RoutedModelSource = 'pinned' | 'passthrough';
export type RouteReason =
  | 'session_pin' | 'strategy' | 'exploration' | 'fallback' | 'rate_limit_hold' | 'default' | 'cache' | 'local';
export type AttemptOutcome =
  | 'ok' | 'rate_limited' | 'timeout' | 'auth' | 'validation' | 'shape_mismatch' | 'upstream' | 'shed' | 'cancelled';
export type AttemptErrorClass =
  | 'auth' | 'rate_limited' | 'overloaded' | 'timeout' | 'server_error'
  | 'unreachable' | 'client_error' | 'bad_response' | 'shed' | 'cancelled';

/** One upstream try; sanitized by the backend (never an upstream body). */
export interface AttemptTraceData {
  iteration: number;
  upstream: string;
  model: string | null;
  outcome: AttemptOutcome;
  status: number | null;
  latencyMs: number;
  errorClass: AttemptErrorClass | null;
  errorMessage: string | null;
}

export interface RequestTraceData {
  id: string;
  requestId: string;
  timestamp: string;
  provider: string;
  model: string;
  timeToHeadersMs: number;
  firstByteMs: number | null;
  totalMs: number | null;
  tokensBefore: number;
  tokensAfter: number | null;
  compressionStatus: CompressionStatus;
  stream: boolean;
  traceKind: TraceKind;
  statusCode: number;
  /** 0 = terminal trace; a lower revision never overwrites a higher one. */
  traceRevision: number;
  firstByteState?: FirstByteState;
  /** Absent on events from before the route-mapping work. A `routeReason` outside `RouteReason` renders raw. */
  requestedModel?: string | null;
  routedModel?: string | null;
  routedModelSource?: RoutedModelSource | null;
  routedProvider?: string | null;
  routeReason?: RouteReason | string | null;
  routeStrategy?: string | null;
  attempts?: AttemptTraceData[];
  attemptsTruncated?: boolean;
  /** Search-emulation rounds; 1 for an ordinary request. */
  iterations?: number;
}

export interface ErrorLoggedData {
  timestamp: string;
  provider: string;
  model: string;
  errorType: string;
  statusCode: number | null;
  message: string;
}

export interface ConfigChangedData {
  timestamp: string;
  routeName: string;
  strategy: string;
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
