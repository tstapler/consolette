import { TestBed } from '@angular/core/testing';
import { MetricsStore } from './metrics.store';
import { RequestTraceData, MetricsTickData } from '../models/telemetry.models';

describe('MetricsStore (TC-UNIT-10)', () => {
  let store: MetricsStore;

  beforeEach(() => {
    TestBed.configureTestingModule({
      providers: [MetricsStore]
    });
    store = TestBed.inject(MetricsStore);
  });

  it('TC-UNIT-10: should cap trace history array prepended at max 500 items', () => {
    for (let i = 1; i <= 600; i++) {
      const trace: RequestTraceData = {
        id: `req-${i}`,
        timestamp: new Date().toISOString(),
        provider: 'anthropic',
        model: 'claude-3-5-sonnet',
        durationMs: 120 + i,
        firstByteMs: 40,
        tokensBefore: 1000,
        tokensAfter: 800,
        compressionStatus: 'compressed',
        statusCode: 200
      };
      store.pushTrace(trace);
    }

    expect(store.recentTraces().length).toBe(500);
    expect(store.recentTraces()[0].id).toBe('req-600');
    expect(store.recentTraces()[499].id).toBe('req-101');
  });

  it('TC-UNIT-10: should cap time series points array at max 720 items', () => {
    for (let i = 1; i <= 800; i++) {
      const tick: MetricsTickData = {
        rpm: i,
        totalRequests: i * 10,
        tokensSaved: i * 50,
        currentLagMs: i === 800 ? 150 : i % 100
      };
      store.pushTick(tick);
    }

    expect(store.timeSeriesPoints().length).toBe(720);
    expect(store.timeSeriesPoints()[719].rpm).toBe(800);
    expect(store.metrics()?.rpm).toBe(800);
    expect(store.isContended()).toBeTrue();
  });
});
