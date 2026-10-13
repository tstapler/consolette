import { RequestTraceData } from './telemetry.models';

export type NaColumn = 'firstByte' | 'tokensAfter';

/** Every reason string the table can show, with its plain-language meaning (footer legend). */
export const NA_REASON_TEXTS: ReadonlyArray<{ text: string; meaning: string }> = [
  { text: 'n/a (not streamed)', meaning: 'the response was not streamed, so there is no first byte to time' },
  { text: 'n/a (not measured)', meaning: 'a streamed response whose first byte is not measured yet' },
  { text: 'n/a (served from cache)', meaning: 'answered from the response cache; no upstream was called' },
  { text: 'n/a (no response)', meaning: 'the upstream sent no response' },
  { text: 'n/a (cancelled)', meaning: 'the client closed the connection first' },
  { text: 'n/a (compression off)', meaning: 'compression is not enabled' },
  { text: 'n/a (compression skipped)', meaning: 'compression was enabled but skipped for this request' },
  { text: 'n/a (compression failed)', meaning: 'compression was attempted and failed' },
  { text: 'n/a (not recorded)', meaning: 'the value was missing from the trace' },
];

const NOT_RECORDED = 'n/a (not recorded)';

/** Reason a column has no real value for this trace (Column Truth Table), or null when the value is real. */
export function naReason(trace: RequestTraceData, column: NaColumn): string | null {
  return column === 'firstByte' ? firstByteReason(trace) : tokensAfterReason(trace);
}

function firstByteReason(t: RequestTraceData): string | null {
  if (typeof t.firstByteMs === 'number') return null;
  if (t.statusCode === 499) return 'n/a (cancelled)';
  if (t.statusCode >= 400) return 'n/a (no response)';
  if (t.traceKind === 'cache_hit') return 'n/a (served from cache)';
  return t.stream ? 'n/a (not measured)' : 'n/a (not streamed)';
}

function tokensAfterReason(t: RequestTraceData): string | null {
  if (t.traceKind === 'cache_hit') return 'n/a (served from cache)';
  switch (t.compressionStatus) {
    case 'off':
    case 'skipped':
    case 'failed':
      return `n/a (compression ${t.compressionStatus})`;
    case 'compressed':
    case 'none':
      return typeof t.tokensAfter === 'number' ? null : NOT_RECORDED;
    default:
      return NOT_RECORDED;
  }
}
