import { ComponentFixture, TestBed } from '@angular/core/testing';
import { LatencyPercentileChartComponent } from './latency-percentile-chart.component';
import { BenchmarkData } from '../../../core/models/telemetry.models';

describe('LatencyPercentileChartComponent', () => {
  let component: LatencyPercentileChartComponent;
  let fixture: ComponentFixture<LatencyPercentileChartComponent>;

  const mockBenchmarkData: BenchmarkData[] = [
    {
      model: 'claude-3-5-sonnet',
      provider: 'anthropic',
      ttftP50: 120,
      ttftP90: 150,
      ttftP95: 180,
      ttftP99: 210,
      speedTokSec: 45,
      successRatePercent: 99.5,
      costPer1k: 0.015,
      tokensSavedPercent: 30
    }
  ];

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [LatencyPercentileChartComponent]
    }).compileComponents();

    fixture = TestBed.createComponent(LatencyPercentileChartComponent);
    component = fixture.componentInstance;
    fixture.detectChanges();
  });

  it('should create component', () => {
    expect(component).toBeTruthy();
  });

  it('should initialize Chart.js instance on init', () => {
    expect(component.chartInstance).not.toBeNull();
  });

  it('should cap datasets at 60 items via sliding window shift', () => {
    if (!component.chartInstance) return;

    for (let i = 0; i < 70; i++) {
      component.chartInstance.data.labels?.push(`T${i}`);
      component.chartInstance.data.datasets[0].data.push(i);
    }

    component.applySlidingWindowCap();
    expect(component.chartInstance.data.labels?.length).toBe(60);
  });

  it('should toggle CDF mode', () => {
    expect(component.isCdfMode).toBeFalse();
    component.toggleCdfMode();
    expect(component.isCdfMode).toBeTrue();
  });

  it('should invoke chartInstance.destroy on ngOnDestroy', () => {
    const chart = component.chartInstance;
    spyOn(chart!, 'destroy').and.callThrough();

    component.ngOnDestroy();
    expect(chart?.destroy).toHaveBeenCalled();
    expect(component.chartInstance).toBeNull();
  });
});
