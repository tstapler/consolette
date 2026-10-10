import { naReason, NA_REASON_TEXTS } from './na-reason';
import { RequestTraceData } from './telemetry.models';

function trace(over: Partial<RequestTraceData> = {}): RequestTraceData {
  return {
    id: 'r1', requestId: 'r1', timestamp: '2026-10-10T18:00:00Z', provider: 'anthropic', model: 'm',
    timeToHeadersMs: 40, firstByteMs: null, totalMs: null, tokensBefore: 1000, tokensAfter: null,
    compressionStatus: 'off', stream: false, traceKind: 'routed', statusCode: 200, traceRevision: 0,
    ...over,
  };
}

describe('naReason', () => {
  const firstByteCases: Array<[string, Partial<RequestTraceData>, string | null]> = [
    ['routed non-stream', {}, 'n/a (not streamed)'],
    ['routed stream, no firstByteState', { stream: true }, 'n/a (not measured)'],
    ['stream with non-null firstByteMs', { stream: true, firstByteMs: 910 }, null],
    ['cache hit', { traceKind: 'cache_hit' }, 'n/a (served from cache)'],
    ['local non-stream', { traceKind: 'local' }, 'n/a (not streamed)'],
    ['local with firstByteMs', { traceKind: 'local', firstByteMs: 5 }, null],
    ['failed', { statusCode: 503, stream: true }, 'n/a (no response)'],
    ['cancelled', { statusCode: 499 }, 'n/a (cancelled)'],
  ];

  const tokensCases: Array<[string, Partial<RequestTraceData>, string | null]> = [
    ['off', { compressionStatus: 'off' }, 'n/a (compression off)'],
    ['skipped', { compressionStatus: 'skipped' }, 'n/a (compression skipped)'],
    ['failed', { compressionStatus: 'failed' }, 'n/a (compression failed)'],
    ['none with value', { compressionStatus: 'none', tokensAfter: 1000 }, null],
    ['compressed with value', { compressionStatus: 'compressed', tokensAfter: 600 }, null],
    ['cache hit', { traceKind: 'cache_hit', compressionStatus: 'compressed', tokensAfter: 1 }, 'n/a (served from cache)'],
    ['local off', { traceKind: 'local' }, 'n/a (compression off)'],
    ['failed request, off', { statusCode: 502 }, 'n/a (compression off)'],
    ['cancelled, off', { statusCode: 499 }, 'n/a (compression off)'],
  ];

  it('naReason_should_equal_truth_table_text_for_every_trace_state_and_column', () => {
    for (const [label, over, want] of firstByteCases) {
      expect(naReason(trace(over), 'firstByte')).withContext(`firstByte ${label}`).toBe(want);
    }
    for (const [label, over, want] of tokensCases) {
      expect(naReason(trace(over), 'tokensAfter')).withContext(`tokensAfter ${label}`).toBe(want);
    }
  });

  it('naReason_should_render_duration_when_firstByteMs_non_null_and_read_not_measured_for_stream_without_firstByteState', () => {
    expect(naReason(trace({ stream: true, firstByteMs: 910 }), 'firstByte')).toBeNull();
    expect(naReason(trace({ stream: true }), 'firstByte')).toBe('n/a (not measured)');
  });

  it('naReason_should_return_null_when_value_is_real', () => {
    expect(naReason(trace({ compressionStatus: 'compressed', tokensAfter: 5 }), 'tokensAfter')).toBeNull();
    expect(naReason(trace({ firstByteMs: 0 }), 'firstByte')).toBeNull();
  });

  it('naReason_should_read_not_measured_and_compression_off_when_stream_row_has_no_first_byte_state_and_status_off', () => {
    const t = trace({ stream: true, compressionStatus: 'off', firstByteMs: null });
    expect(naReason(t, 'firstByte')).toBe('n/a (not measured)');
    expect(naReason(t, 'tokensAfter')).toBe('n/a (compression off)');
  });

  it('naReason_should_read_compression_off_skipped_failed_and_never_no_savings', () => {
    for (const status of ['off', 'skipped', 'failed'] as const) {
      const text = naReason(trace({ compressionStatus: status }), 'tokensAfter') ?? '';
      expect(text).toBe(`n/a (compression ${status})`);
      expect(text.toLowerCase()).not.toContain('no savings');
    }
  });

  it('exposes every reason string for the legend', () => {
    expect(NA_REASON_TEXTS.length).toBeGreaterThanOrEqual(9);
    expect(NA_REASON_TEXTS.map(r => r.text)).toContain('n/a (compression failed)');
  });
});
