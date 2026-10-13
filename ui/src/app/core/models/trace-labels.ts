import { AttemptTraceData, RequestTraceData } from './telemetry.models';

/** Single source for chip text, legend, Why text and labels (plan "Route reason and label maps"). */

export interface ReasonLabel { chip: string; legend: string; why: string; }

/** Keyed by wire `routeReason`. `why` may contain `<...>` placeholders filled by `whyText`. */
export const ROUTE_REASONS: Readonly<Record<string, ReasonLabel>> = {
  session_pin: { chip: 'pinned', legend: 'a session pinned this conversation to one upstream', why: 'pinned to this upstream by the session' },
  strategy: { chip: 'strategy', legend: 'the routing strategy picked this upstream (name shown in the drawer)', why: 'chosen by the <routeStrategy> strategy' },
  exploration: { chip: 'explore', legend: 'an occasional exploration pick to test another upstream', why: 'exploration pick' },
  fallback: { chip: 'fallback', legend: 'an earlier upstream failed, so the next one was tried', why: 'moved to the next candidate after <first failed attempt>' },
  rate_limit_hold: { chip: 'held', legend: 'waited for a rate-limit cooldown, then retried', why: 'waited for a rate-limit cooldown, then retried (<n>x)' },
  default: { chip: 'default', legend: 'no special rule; the default route', why: 'default route: no session pin, exploration or strategy decision applied' },
  cache: { chip: 'cache', legend: 'answered from the response cache', why: 'served from the response cache; no upstream was called' },
  local: { chip: 'local', legend: 'answered by a local model', why: 'served by a local model' },
};

const CANCELLED: ReasonLabel = { chip: 'cancelled', legend: 'the client closed the connection', why: 'client closed the connection' };

export const CHIP_LEGEND: ReadonlyArray<{ chip: string; meaning: string }> = [
  ...Object.values(ROUTE_REASONS).map(r => ({ chip: r.chip, meaning: r.legend })),
  { chip: CANCELLED.chip, meaning: CANCELLED.legend },
];

export const ERROR_CLASS_LABELS: Readonly<Record<string, string>> = {
  auth: 'auth failed',
  rate_limited: 'rate limited',
  overloaded: 'overloaded',
  timeout: 'timed out',
  server_error: 'server error',
  unreachable: 'unreachable',
  client_error: 'request rejected',
  bad_response: 'bad response',
  shed: 'shed, not tried',
  cancelled: 'cancelled',
};

export const isNum = (v: unknown): v is number => typeof v === 'number' && Number.isFinite(v);
export const fmt = (n: number): string => n.toLocaleString('en-US');

export function errorLabel(a: AttemptTraceData): string {
  const key = a.errorClass ?? a.outcome;
  return ERROR_CLASS_LABELS[key] ?? String(key);
}

export function isFailed(a: AttemptTraceData): boolean {
  return a.outcome !== 'ok';
}

/** Chip for a row: status 499 and the non-routed kinds win over `routeReason`; an unknown reason shows raw. */
export function routeChip(t: RequestTraceData): string | null {
  if (t.statusCode === 499) return CANCELLED.chip;
  if (t.traceKind === 'cache_hit') return ROUTE_REASONS['cache'].chip;
  if (t.traceKind === 'local') return ROUTE_REASONS['local'].chip;
  const reason = t.routeReason;
  if (!reason) return null;
  return ROUTE_REASONS[reason]?.chip ?? reason;
}

export function chipMeaning(chip: string): string {
  return CHIP_LEGEND.find(c => c.chip === chip)?.meaning ?? chip;
}

/** Sub-label for a successful row that followed a failed attempt; derived, no wire field. */
export function statusSubLabel(t: RequestTraceData): string | null {
  const attempts = t.attempts;
  if (!attempts || attempts.length <= 1) return null;
  if (isNum(t.statusCode) && t.statusCode >= 400) return null;
  return attempts[attempts.length - 1].upstream !== attempts[0].upstream ? 'fell back' : 'retried';
}

/** "anthropic failed (overloaded, HTTP 529)" for the first failed attempt, or null when none failed. */
export function firstFailureText(t: RequestTraceData): string | null {
  const a = (t.attempts ?? []).find(isFailed);
  if (!a) return null;
  const http = isNum(a.status) ? `, HTTP ${a.status}` : '';
  return `${a.upstream} failed (${errorLabel(a)}${http})`;
}

/** Drawer "Why" sentence. */
export function whyText(t: RequestTraceData): string {
  if (t.statusCode === 499) return CANCELLED.why;
  const key = t.traceKind === 'cache_hit' ? 'cache' : t.traceKind === 'local' ? 'local' : t.routeReason ?? '';
  const label = ROUTE_REASONS[key];
  if (!label) return key || 'not recorded';
  switch (key) {
    case 'strategy':
      return label.why.replace('<routeStrategy>', t.routeStrategy ?? 'configured');
    case 'fallback': {
      const failed = firstFailureText(t);
      return failed ? label.why.replace('<first failed attempt>', failed) : 'moved to the next candidate';
    }
    case 'rate_limit_hold': {
      const held = (t.attempts ?? []).filter(a => a.outcome === 'rate_limited').length;
      return label.why.replace('<n>', String(Math.max(held, 1)));
    }
    case 'local':
      return t.routedModel ? `${label.why} ${t.routedModel}` : label.why;
    default:
      return label.why;
  }
}

/** Model-source line, or null when the trace has no source (cache, local, older events). */
export function modelSourceText(t: RequestTraceData): string | null {
  const provider = t.routedProvider ?? t.provider;
  if (t.routeReason === 'session_pin') {
    return `session pin: the model came from this session's pin, not from the client's ${t.requestedModel ?? 'requested model'}`;
  }
  if (t.routedModelSource === 'pinned') {
    return `pinned: ${provider}'s configured model replaces the client's ${t.requestedModel ?? 'requested model'}`;
  }
  if (t.routedModelSource === 'passthrough') {
    return `passthrough: the client's model name was sent unchanged to ${provider}`;
  }
  return null;
}

const STATUS_WORDS: Record<number, string> = {
  200: 'OK', 400: 'Bad request', 401: 'Unauthorized', 403: 'Forbidden', 404: 'Not found',
  408: 'Timeout', 422: 'Rejected', 429: 'Rate limited', 499: 'Client closed', 500: 'Server error',
  502: 'Failed', 503: 'Unavailable', 504: 'Timed out', 529: 'Overloaded',
};

export function statusParts(t: RequestTraceData): { text: string; glyph: string; cls: string } {
  const code = t.statusCode;
  if (!isNum(code)) return { text: '—', glyph: '-', cls: 'bg-neutral-800 text-neutral-300 border-neutral-700' };
  if (t.traceKind === 'cache_hit' && code === 200) {
    return { text: '200 (cache)', glyph: '✓', cls: 'bg-emerald-950/80 text-emerald-400 border-emerald-800/60' };
  }
  const word = STATUS_WORDS[code] ?? (code >= 500 ? 'Server error' : code >= 400 ? 'Client error' : '');
  const text = word ? `${code} ${word}` : String(code);
  if (code === 499) return { text, glyph: '-', cls: 'bg-neutral-800 text-neutral-300 border-neutral-700' };
  if (code >= 200 && code < 300) return { text, glyph: '✓', cls: 'bg-emerald-950/80 text-emerald-400 border-emerald-800/60' };
  if (code === 429) return { text, glyph: '!', cls: 'bg-amber-950/80 text-amber-400 border-amber-800/60' };
  if (code >= 400 && code < 500) return { text, glyph: '!', cls: 'bg-yellow-950/80 text-yellow-400 border-yellow-800/60' };
  if (code >= 500) return { text, glyph: '✕', cls: 'bg-rose-950/80 text-rose-400 border-rose-800/60' };
  return { text, glyph: '-', cls: 'bg-neutral-800 text-neutral-300 border-neutral-700' };
}

export function tokensAfterText(t: RequestTraceData): string {
  const after = t.tokensAfter as number;
  if (t.compressionStatus !== 'compressed') return `${fmt(after)} est.`;
  if (!isNum(t.tokensBefore) || t.tokensBefore <= 0 || after >= t.tokensBefore) return `${fmt(after)} est.`;
  const pct = Math.round(((t.tokensBefore - after) / t.tokensBefore) * 100);
  return `${fmt(t.tokensBefore)} -> ${fmt(after)} (-${pct}%)`;
}
