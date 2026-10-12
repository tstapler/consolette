import { ComponentFixture, TestBed } from '@angular/core/testing';
import { OverviewDashboardComponent } from './overview-dashboard.component';
import { MetricsStore } from '../../core/stores/metrics.store';
import { SseService } from '../../core/services/sse.service';
import { BehaviorSubject, Subject } from 'rxjs';
import { signal } from '@angular/core';
import { DashboardEvent } from '../../core/models/telemetry.models';
import { provideRouter } from '@angular/router';

describe('OverviewDashboardComponent', () => {
  let component: OverviewDashboardComponent;
  let fixture: ComponentFixture<OverviewDashboardComponent>;
  let mockSseEvents$: Subject<DashboardEvent>;
  let statusSignal: ReturnType<typeof signal<string>>;
  let reconnectSpy: jasmine.Spy;
  let mockConnectionStatusSignal: BehaviorSubject<'Connected' | 'Reconnecting' | 'Disconnected'>;

  beforeEach(async () => {
    mockSseEvents$ = new Subject<DashboardEvent>();
    mockConnectionStatusSignal = new BehaviorSubject<'Connected' | 'Reconnecting' | 'Disconnected'>('Connected');

    statusSignal = signal<string>('connected');
    reconnectSpy = jasmine.createSpy('reconnect');
    const mockSseService = {
      events$: mockSseEvents$.asObservable(),
      connectionStatus: statusSignal.asReadonly(),
      retryCount: () => 0,
      reconnect: reconnectSpy
    };

    await TestBed.configureTestingModule({
      imports: [OverviewDashboardComponent],
      providers: [
        provideRouter([]),
        { provide: SseService, useValue: mockSseService },
        MetricsStore
      ]
    }).compileComponents();

    fixture = TestBed.createComponent(OverviewDashboardComponent);
    component = fixture.componentInstance;
    fixture.detectChanges();
  });

  it('labels the /metrics-derived rate as upstream attempts, not client requests', () => {
    const text = (fixture.nativeElement as HTMLElement).textContent ?? '';
    expect(text).toContain('Upstream Attempts Per Min');
    expect(text).toContain('Counts every upstream attempt, not client requests');
    expect(text).not.toContain('Requests Per Min');
  });

  it('should create the component', () => {
    expect(component).toBeTruthy();
  });

  it('should render all overview subcomponents (KPI cards, health grid, chart, traces table)', () => {
    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.querySelector('app-metric-kpi-card')).toBeTruthy();
    expect(compiled.querySelector('app-provider-health-grid')).toBeTruthy();
    expect(compiled.querySelector('app-rpm-trend-chart')).toBeTruthy();
    expect(compiled.querySelector('app-recent-trace-table')).toBeTruthy();
  });

  it('should reflect MetricsStore signal updates in the template', () => {
    component.metricsStore.handleDashboardEvent({
      type: 'MetricsTick',
      data: {
        rpm: 120,
        totalRequests: 500,
        totalTokensSaved: 10000,
        tps: 45,
        medianLatencyMs: 150,
        tokenSavingsPercent: 35.5,
        currentLagMs: 12
      }
    });

    fixture.detectChanges();

    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.textContent).toContain('120');
    expect(compiled.textContent).toContain('45');
    expect(compiled.textContent).toContain('150ms');
    expect(compiled.textContent).toContain('35.5%');
  });

  it('labels the latency card "to headers" and shows n/a, not 0, for fields with no source', () => {
    component.metricsStore.handleDashboardEvent({
      type: 'MetricsTick',
      data: {
        rpm: 3, totalRequests: 9, totalTokensSaved: 0, currentLagMs: 1,
        medianLatencyMs: 800, medianFirstByteMs: null, tps: null, tokenSavingsPercent: null,
      },
    });
    fixture.detectChanges();
    const text = (fixture.nativeElement as HTMLElement).textContent ?? '';
    expect(text).toContain('Median latency (to headers)');
    expect(text).toContain('800ms');
    expect(text).toContain('n/a (compression off)');
    expect(text).toContain('n/a (not measured)');
    expect(text).not.toContain('Median TTFT');
    expect(text).not.toContain('0.0%');
    const cards = Array.from((fixture.nativeElement as HTMLElement).querySelectorAll('app-metric-kpi-card'));
    const tpsCard = cards.find((c) => c.textContent?.includes('Tokens Per Second'));
    expect(tpsCard?.textContent).toContain('\u2013');
  });

  it('Inspect opens the in-page drawer for that trace instead of navigating to sessions', () => {
    component.metricsStore.handleDashboardEvent({
      type: 'RequestTrace',
      data: {
        id: 'req_1', requestId: 'req_1', timestamp: '2026-10-10T18:09:53+00:00', provider: 'anthropic', model: 'm',
        timeToHeadersMs: 5, firstByteMs: null, totalMs: null, tokensBefore: 10, tokensAfter: null,
        compressionStatus: 'off', stream: false, traceKind: 'routed', statusCode: 200, traceRevision: 0,
      },
    });
    fixture.detectChanges();
    const el = fixture.nativeElement as HTMLElement;
    (el.querySelector('button[data-inspect]') as HTMLButtonElement).click();
    fixture.detectChanges();
    expect(el.querySelector('[role="dialog"]')).not.toBeNull();
    expect(document.activeElement?.id).toBe('inspect-drawer-heading');
  });

  it('shows a feed-lagging status chip for system_lag and clears it on the next event', () => {
    component.metricsStore.handleDashboardEvent({ type: 'system_lag', data: { skipped: 37 } });
    fixture.detectChanges();
    const el = fixture.nativeElement as HTMLElement;
    const chip = el.querySelector('[data-lag-chip]');
    expect(chip?.getAttribute('role')).toBe('status');
    expect(chip?.textContent).toContain('feed lagging - 37 events skipped');

    component.metricsStore.handleDashboardEvent({
      type: 'MetricsTick',
      data: { rpm: 1, totalRequests: 1, totalTokensSaved: 0, currentLagMs: 0 },
    });
    fixture.detectChanges();
    expect(el.querySelector('[data-lag-chip]')).toBeNull();
  });

  it('shows a stale-data alert with a Reconnect button once retries are exhausted', () => {
    const el = fixture.nativeElement as HTMLElement;
    expect(el.querySelector('[data-disconnected-alert]')).toBeNull();

    statusSignal.set('disconnected');
    fixture.detectChanges();
    const alert = el.querySelector('[data-disconnected-alert]');
    expect(alert?.getAttribute('role')).toBe('alert');
    expect(alert?.textContent).toContain('Live feed disconnected. Data below is stale.');
    (alert?.querySelector('button') as HTMLButtonElement).click();
    expect(reconnectSpy).toHaveBeenCalledTimes(1);
  });

  it('lists recent errors with an empty state and a latest-20 footer', () => {
    const el = fixture.nativeElement as HTMLElement;
    expect(el.querySelector('[data-recent-errors]')?.textContent).toContain('No errors since this page loaded.');

    for (let i = 1; i <= 21; i++) {
      component.metricsStore.handleDashboardEvent({
        type: 'ErrorLogged',
        data: { timestamp: '2026-10-10T18:10:02+00:00', provider: 'openrouter', model: 'm', errorType: 'rate_limited', statusCode: 429, message: `msg${i}` },
      });
    }
    fixture.detectChanges();
    const section = el.querySelector('[data-recent-errors]') as HTMLElement;
    expect(section.querySelectorAll('li').length).toBe(20);
    expect(section.textContent).toContain('Showing latest 20');
    expect(section.querySelector('li')?.textContent).toContain('msg21');
    expect(section.textContent).not.toContain('No errors since this page loaded.');
    expect(section.getAttribute('aria-live')).toBeNull();
  });
});
