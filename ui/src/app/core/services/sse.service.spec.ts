import { TestBed, fakeAsync, tick } from '@angular/core/testing';
import { SseService } from './sse.service';

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
});
