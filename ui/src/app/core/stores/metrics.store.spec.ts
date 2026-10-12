import { TestBed } from '@angular/core/testing';
import { MetricsStore } from './metrics.store';
import { ConfigStore } from './config.store';
import { SseService } from '../services/sse.service';
import { DashboardEvent, RequestTraceData, MetricsTickData } from '../models/telemetry.models';
import traceSuccess from '../../../testing/fixtures/sse/request-trace-success.json';
import metricsTickFixture from '../../../testing/fixtures/sse/metrics-tick.json';

describe('MetricsStore (TC-UNIT-10)', () => {
  let store: MetricsStore;
  let instances: any[];
  const configStoreSpy = jasmine.createSpyObj<ConfigStore>('ConfigStore', ['loadConfig']);

  beforeEach(() => {
    instances = [];
    configStoreSpy.loadConfig.calls.reset();
    spyOn(window as any, 'EventSource').and.callFake(function (this: any, url: string) {
      const inst = { url, close: () => undefined, addEventListener: jasmine.createSpy('addEventListener'), onopen: null, onmessage: null, onerror: null };
      instances.push(inst);
      return inst;
    } as any);
    TestBed.configureTestingModule({
      providers: [MetricsStore, { provide: ConfigStore, useValue: configStoreSpy }]
    });
    store = TestBed.inject(MetricsStore);
  });

  /** Feeds a raw SSE frame through the real SseService (mock EventSource) into the store. */
  function deliver(frame: unknown): void {
    const sse = TestBed.inject(SseService);
    if (instances.length === 0) {
      sse.connect({ url: '/test/events' });
    }
    instances[0].onmessage({ data: JSON.stringify(frame) });
  }

  it('pushTrace_should_populate_requestId_timeToHeadersMs_statusCode_when_success_fixture_parsed', () => {
    deliver(traceSuccess);

    const first = store.recentTraces()[0];
    expect(first.requestId).toBe('req_123');
    expect(first.timeToHeadersMs).toBe(42);
    expect(first.statusCode).toBe(200);
    expect(first.firstByteMs).toBeNull();
  });

  it('pushTrace_should_keep_row_unchanged_when_lower_traceRevision_arrives_after_higher', () => {
    const base = traceSuccess.data as unknown as RequestTraceData;
    const high: RequestTraceData = { ...base, traceRevision: 1, firstByteMs: 450 };
    const low: RequestTraceData = { ...base, traceRevision: 0, firstByteMs: null };

    store.pushTrace(high);
    store.pushTrace(low);

    expect(store.recentTraces().length).toBe(1);
    expect(store.recentTraces()[0]).toEqual(high);
  });

  it('pushTrace_should_replace_row_in_place_when_equal_or_higher_traceRevision_arrives', () => {
    const base = traceSuccess.data as unknown as RequestTraceData;
    store.pushTrace(base);
    store.pushTrace({ ...base, requestId: 'req_other', id: 'req_other' });
    store.pushTrace({ ...base, traceRevision: 1, firstByteMs: 450 });

    expect(store.recentTraces().map((t) => t.requestId)).toEqual(['req_other', 'req_123']);
    expect(store.recentTraces()[1].firstByteMs).toBe(450);
  });

  it('metricsTick_should_expose_totalRequests_and_isContended_when_tick_fixture_parsed', () => {
    deliver(metricsTickFixture);
    expect(store.totalRequests()).toBe(120);
    expect(store.isContended()).toBeFalse();

    deliver({ ...metricsTickFixture, data: { ...metricsTickFixture.data, currentLagMs: 150 } });
    expect(store.isContended()).toBeTrue();
  });

  it('system_lag_event_should_still_be_consumed_after_contract_change', () => {
    const sse = TestBed.inject(SseService);
    const seen: DashboardEvent[] = [];
    const sub = sse.events$.subscribe((e) => seen.push(e));
    sse.connect({ url: '/test/events' });
    const lagCall = instances[0].addEventListener.calls.allArgs().find((a: any[]) => a[0] === 'system_lag');
    expect(lagCall).toBeDefined();

    lagCall[1]({ data: JSON.stringify({ skipped: 7 }) } as MessageEvent);

    expect(seen.length).toBe(1);
    expect(seen[0].type).toBe('system_lag');
    expect((seen[0].data as { skipped: number }).skipped).toBe(7);
    expect(store.recentTraces().length).toBe(0);
    sub.unsubscribe();
    sse.disconnect();
  });

  it('TC-UNIT-10: should cap trace history array prepended at max 500 items', () => {
    for (let i = 1; i <= 600; i++) {
      const trace: RequestTraceData = {
        id: `req-${i}`,
        requestId: `req-${i}`,
        timestamp: new Date().toISOString(),
        provider: 'anthropic',
        model: 'claude-3-5-sonnet',
        timeToHeadersMs: 120 + i,
        firstByteMs: null,
        totalMs: null,
        tokensBefore: 1000,
        tokensAfter: null,
        compressionStatus: 'off',
        stream: false,
        traceKind: 'routed',
        statusCode: 200,
        traceRevision: 0
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
        totalTokensSaved: i * 50,
        currentLagMs: i === 800 ? 150 : i % 100
      };
      store.pushTick(tick);
    }

    expect(store.timeSeriesPoints().length).toBe(720);
    expect(store.timeSeriesPoints()[719].rpm).toBe(800);
    expect(store.metrics()?.rpm).toBe(800);
    expect(store.isContended()).toBeTrue();
  });

  it('system_lag should set the skipped count and the next non-lag event should clear it', () => {
    deliver({ type: 'system_lag', data: { skipped: 37 } });
    expect(store.lagSkipped()).toBe(37);

    deliver(metricsTickFixture);
    expect(store.lagSkipped()).toBeNull();
  });

  it('ConfigChanged should reload the config exactly once', () => {
    deliver({ type: 'ConfigChanged', data: { timestamp: 't', routeName: 'default', strategy: 'weighted' } });
    expect(configStoreSpy.loadConfig).toHaveBeenCalledTimes(1);
  });

  it('ErrorLogged should keep the latest 20 newest first and flag overflow', () => {
    expect(store.recentErrors()).toEqual([]);
    expect(store.errorsOverflowed()).toBeFalse();
    for (let i = 1; i <= 21; i++) {
      deliver({
        type: 'ErrorLogged',
        data: { timestamp: `t${i}`, provider: 'openrouter', model: 'm', errorType: 'rate_limited', statusCode: 429, message: `e${i}` },
      });
    }
    expect(store.recentErrors().length).toBe(20);
    expect(store.recentErrors()[0].message).toBe('e21');
    expect(store.recentErrors()[19].message).toBe('e2');
    expect(store.errorsOverflowed()).toBeTrue();
  });

  it('reconnect should delegate to the SSE service', () => {
    const sse = TestBed.inject(SseService);
    spyOn(sse, 'reconnect');
    store.reconnect();
    expect(sse.reconnect).toHaveBeenCalledTimes(1);
  });
});
