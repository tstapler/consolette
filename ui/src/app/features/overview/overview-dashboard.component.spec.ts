import { ComponentFixture, TestBed } from '@angular/core/testing';
import { OverviewDashboardComponent } from './overview-dashboard.component';
import { MetricsStore } from '../../core/stores/metrics.store';
import { SseService } from '../../core/services/sse.service';
import { BehaviorSubject, Subject } from 'rxjs';
import { DashboardEvent } from '../../core/models/telemetry.models';
import { provideRouter } from '@angular/router';

describe('OverviewDashboardComponent', () => {
  let component: OverviewDashboardComponent;
  let fixture: ComponentFixture<OverviewDashboardComponent>;
  let mockSseEvents$: Subject<DashboardEvent>;
  let mockConnectionStatusSignal: BehaviorSubject<'Connected' | 'Reconnecting' | 'Disconnected'>;

  beforeEach(async () => {
    mockSseEvents$ = new Subject<DashboardEvent>();
    mockConnectionStatusSignal = new BehaviorSubject<'Connected' | 'Reconnecting' | 'Disconnected'>('Connected');

    const mockSseService = {
      events$: mockSseEvents$.asObservable(),
      connectionStatus: () => 'Connected',
      retryCount: () => 0
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
        medianTtftMs: 150,
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
});
