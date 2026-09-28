import { ComponentFixture, TestBed } from '@angular/core/testing';
import { RpmTrendChartComponent } from './rpm-trend-chart.component';
import { MetricsTickData } from '../../../core/models/telemetry.models';

describe('RpmTrendChartComponent (TC-UNIT-15 & TC-UNIT-16)', () => {
  let component: RpmTrendChartComponent;
  let fixture: ComponentFixture<RpmTrendChartComponent>;

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [RpmTrendChartComponent]
    }).compileComponents();

    fixture = TestBed.createComponent(RpmTrendChartComponent);
    component = fixture.componentInstance;
    fixture.detectChanges(); // Triggers ngAfterViewInit & initChart
  });

  afterEach(() => {
    if (component.chartInstance) {
      component.chartInstance.destroy();
      component.chartInstance = null;
    }
  });

  it('should create the component and initialize Chart.js instance', () => {
    expect(component).toBeTruthy();
    expect(component.chartInstance).not.toBeNull();
  });

  it('TC-UNIT-15: should enforce sliding window cap at max 60 data points using .shift()', () => {
    expect(component.chartInstance).not.toBeNull();

    // Push 70 data points
    for (let i = 1; i <= 70; i++) {
      const mockTick: MetricsTickData = {
        rpm: 10 + i,
        totalRequests: i,
        tokensSaved: i * 100,
        currentLagMs: 5 + (i % 3)
      };
      component.pushDataPoint(mockTick);
    }

    const labelsLength = component.chartInstance!.data.labels?.length ?? 0;
    const rpmDatasetLength = component.chartInstance!.data.datasets[0].data.length;
    const lagDatasetLength = component.chartInstance!.data.datasets[1].data.length;

    expect(labelsLength).toBe(60);
    expect(rpmDatasetLength).toBe(60);
    expect(lagDatasetLength).toBe(60);

    // The oldest 10 items should have been shifted out
    // Point 71st push should have values corresponding to i = 70 at the end
    const lastRpmValue = component.chartInstance!.data.datasets[0].data[59];
    expect(lastRpmValue).toBe(80); // 10 + 70
  });

  it('TC-UNIT-16: should invoke chartInstance.destroy() on ngOnDestroy', () => {
    expect(component.chartInstance).not.toBeNull();

    const chartSpy = spyOn(component.chartInstance!, 'destroy').and.callThrough();

    component.ngOnDestroy();

    expect(chartSpy).toHaveBeenCalled();
    expect(component.chartInstance).toBeNull();
  });

  it('should load historical data points correctly', () => {
    const history: MetricsTickData[] = Array.from({ length: 30 }, (_, i) => ({
      rpm: i * 2,
      totalRequests: i,
      tokensSaved: i * 50,
      currentLagMs: 2
    }));

    component.loadHistory(history);

    expect(component.chartInstance!.data.labels?.length).toBe(30);
    expect(component.chartInstance!.data.datasets[0].data.length).toBe(30);
  });
});
