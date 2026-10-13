import { TestBed, fakeAsync, tick } from '@angular/core/testing';
import { SseService } from './sse.service';
import { DashboardEvent } from '../models/telemetry.models';
import traceSuccess from '../../../testing/fixtures/sse/request-trace-success.json';
import traceFailure from '../../../testing/fixtures/sse/request-trace-failure.json';
import metricsTick from '../../../testing/fixtures/sse/metrics-tick.json';
import errorLogged from '../../../testing/fixtures/sse/error-logged.json';
import configChanged from '../../../testing/fixtures/sse/config-changed.json';

describe('SseService (TC-UNIT-08 & TC-UNIT-09)', () => {
  let service: SseService;
  let activeMockInstances: any[];

  beforeEach(() => {
    activeMockInstances = [];

    function MockEventSource(this: any, url: string) {
      const instance = {
        url,
        close: jasmine.createSpy('close'),
        addEventListener: jasmine.createSpy('addEventListener'),
        onopen: null as any,
        onmessage: null as any,
        onerror: null as any
      };
      activeMockInstances.push(instance);
      return instance;
    }

    spyOn(window as any, 'EventSource').and.callFake(MockEventSource as any);

    TestBed.configureTestingModule({
      providers: [SseService]
    });

    service = TestBed.inject(SseService);
  });

  afterEach(() => {
    service.disconnect();
  });

  it('TC-UNIT-08: should initialize connection and handle exponential backoff on error', fakeAsync(() => {
    service.connect({ url: '/test/events', initialBackoffMs: 100, maxBackoffMs: 1000, maxRetryAttempts: 3 });
    expect(window.EventSource).toHaveBeenCalledWith('/test/events');
    const instance1 = activeMockInstances[0];
    expect(instance1).toBeDefined();

    // Simulate onopen
    instance1.onopen();
    expect(service.connectionStatus()).toBe('connected');
    expect(service.isConnected()).toBeTrue();
    expect(service.retryCount()).toBe(0);

    // Simulate error -> should enter reconnecting state
    instance1.onerror();
    expect(service.connectionStatus()).toBe('reconnecting');
    expect(service.retryCount()).toBe(1);

    // Advance backoff time (100ms * 2^0 = 100ms)
    tick(100);
    expect(window.EventSource).toHaveBeenCalledTimes(2);
    const instance2 = activeMockInstances[1];

    // Second error -> retry #2 (100ms * 2^1 = 200ms)
    instance2.onerror();
    expect(service.retryCount()).toBe(2);

    tick(200);
    expect(window.EventSource).toHaveBeenCalledTimes(3);
    const instance3 = activeMockInstances[2];

    // Third error -> retry #3 (100ms * 2^2 = 400ms)
    instance3.onerror();
    expect(service.retryCount()).toBe(3);

    tick(400);
    expect(window.EventSource).toHaveBeenCalledTimes(4);
    const instance4 = activeMockInstances[3];

    // Fourth error -> exceeds maxRetryAttempts (3) -> disconnected
    instance4.onerror();
    expect(service.connectionStatus()).toBe('disconnected');
  }));

  it('TC-UNIT-09: should close event source on disconnect and teardown', () => {
    service.connect({ url: '/test/events' });
    const instance = activeMockInstances[0];
    expect(instance).toBeDefined();

    service.disconnect();

    expect(instance.close).toHaveBeenCalled();
    expect(service.connectionStatus()).toBe('disconnected');
  });

  it('should emit each shared fixture frame unchanged as a typed DashboardEvent', () => {
    const seen: DashboardEvent[] = [];
    service.events$.subscribe((e) => seen.push(e));
    service.connect({ url: '/test/events' });
    const frames = [traceSuccess, traceFailure, metricsTick, errorLogged, configChanged];

    frames.forEach((f) => activeMockInstances[0].onmessage({ data: JSON.stringify(f) }));

    expect(seen.map((e) => e.type)).toEqual(['RequestTrace', 'RequestTrace', 'MetricsTick', 'ErrorLogged', 'ConfigChanged']);
    expect(seen).toEqual(frames as unknown as DashboardEvent[]);
  });

  // E-02: a connection that goes silent (stopped-but-lingering proxy, half-open socket)
  // never fires onerror, so the badge used to stay "connected" forever.
  it('should leave connected and reconnect when the stream goes silent past the stale threshold', fakeAsync(() => {
    service.connect({ url: '/test/events', initialBackoffMs: 100, staleAfterMs: 5000 });
    activeMockInstances[0].onopen();
    expect(service.connectionStatus()).toBe('connected');

    tick(4000);
    activeMockInstances[0].onmessage({ data: JSON.stringify(metricsTick) });
    tick(4000);
    expect(service.connectionStatus()).toBe('connected');

    tick(1000);
    expect(service.connectionStatus()).toBe('reconnecting');
    tick(100);
    expect(window.EventSource).toHaveBeenCalledTimes(2);
    service.disconnect();
  }));

  it('should end disconnected after maxRetryAttempts failures and reconnect() should reset the counter and reconnect', fakeAsync(() => {
    service.connect({ url: '/test/events', initialBackoffMs: 10, maxBackoffMs: 10, maxRetryAttempts: 3 });
    for (let i = 0; i < 4; i++) {
      activeMockInstances[activeMockInstances.length - 1].onerror();
      tick(10);
    }
    expect(service.connectionStatus()).toBe('disconnected');
    expect(service.retryCount()).toBe(3);
    const before = activeMockInstances.length;

    service.reconnect();

    expect(service.retryCount()).toBe(0);
    expect(service.connectionStatus()).toBe('connecting');
    expect(activeMockInstances.length).toBe(before + 1);
    activeMockInstances[before].onopen();
    expect(service.connectionStatus()).toBe('connected');
    service.disconnect();
  }));
});
