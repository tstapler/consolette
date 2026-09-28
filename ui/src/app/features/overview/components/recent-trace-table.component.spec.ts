import { ComponentFixture, TestBed } from '@angular/core/testing';
import { RecentTraceTableComponent } from './recent-trace-table.component';
import { RequestTraceData } from '../../../core/models/telemetry.models';

describe('RecentTraceTableComponent (TC-UNIT-17)', () => {
  let component: RecentTraceTableComponent;
  let fixture: ComponentFixture<RecentTraceTableComponent>;

  const mockTraces: RequestTraceData[] = [
    {
      id: 'trace-123456789',
      requestId: 'req-987654321',
      timestamp: new Date().toISOString(),
      provider: 'Anthropic',
      model: 'claude-3-5-sonnet',
      durationMs: 450,
      firstByteMs: 120,
      tokensBefore: 1000,
      tokensAfter: 600,
      compressionStatus: 'compressed',
      statusCode: 200
    },
    {
      id: 'trace-456789012',
      requestId: 'req-222333444',
      timestamp: new Date().toISOString(),
      provider: 'OpenAI',
      model: 'gpt-4o',
      durationMs: 800,
      firstByteMs: 300,
      tokensBefore: 500,
      tokensAfter: 500,
      compressionStatus: 'none',
      statusCode: 429
    }
  ];

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [RecentTraceTableComponent]
    }).compileComponents();

    fixture = TestBed.createComponent(RecentTraceTableComponent);
    component = fixture.componentInstance;
  });

  it('should create the component', () => {
    expect(component).toBeTruthy();
  });

  it('should display empty message when traces array is empty', () => {
    component.traces = [];
    fixture.detectChanges();

    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.textContent).toContain('No request traces recorded yet');
  });

  it('should render table rows for provided request traces', () => {
    component.traces = mockTraces;
    fixture.detectChanges();

    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.textContent).toContain('Anthropic');
    expect(compiled.textContent).toContain('claude-3-5-sonnet');
    expect(compiled.textContent).toContain('450ms');
    expect(compiled.textContent).toContain('120ms');
    expect(compiled.textContent).toContain('-40%'); // (1000 - 600) / 1000 = 40%
    expect(compiled.textContent).toContain('200');
    expect(compiled.textContent).toContain('429');
  });

  it('should emit inspectPayload and selectTrace events when Inspect button is clicked', () => {
    component.traces = mockTraces;
    fixture.detectChanges();

    spyOn(component.inspectPayload, 'emit');
    spyOn(component.selectTrace, 'emit');

    const inspectBtn = fixture.nativeElement.querySelector('button') as HTMLButtonElement;
    expect(inspectBtn).toBeTruthy();
    inspectBtn.click();

    expect(component.inspectPayload.emit).toHaveBeenCalledWith(mockTraces[0]);
    expect(component.selectTrace.emit).toHaveBeenCalledWith(mockTraces[0]);
  });
});
