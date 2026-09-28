import { ComponentFixture, TestBed } from '@angular/core/testing';
import { MetricKpiCardComponent } from './metric-kpi-card.component';

describe('MetricKpiCardComponent (TC-UNIT-13)', () => {
  let component: MetricKpiCardComponent;
  let fixture: ComponentFixture<MetricKpiCardComponent>;

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [MetricKpiCardComponent]
    }).compileComponents();

    fixture = TestBed.createComponent(MetricKpiCardComponent);
    component = fixture.componentInstance;
  });

  it('should create the component', () => {
    expect(component).toBeTruthy();
  });

  it('should format numbers with locale by default', () => {
    component.value = 12500;
    component.formatType = 'number';
    fixture.detectChanges();
    expect(component.formattedValue).toBe((12500).toLocaleString());
  });

  it('should format compact numbers correctly (K and M)', () => {
    component.value = 1500;
    component.formatType = 'compact';
    expect(component.formattedValue).toBe('1.5K');

    component.value = 2500000;
    expect(component.formattedValue).toBe('2.5M');

    component.value = 500;
    expect(component.formattedValue).toBe('500');
  });

  it('should format percent and ms values correctly', () => {
    component.value = 42.678;
    component.formatType = 'percent';
    expect(component.formattedValue).toBe('42.7%');

    component.value = 128.4;
    component.formatType = 'ms';
    expect(component.formattedValue).toBe('128ms');
  });

  it('should display title, unit, and subtitle in template', () => {
    component.title = 'Requests Per Min';
    component.value = 120;
    component.unit = 'RPM';
    component.subtitle = 'Real-time proxy throughput';
    fixture.detectChanges();

    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.textContent).toContain('Requests Per Min');
    expect(compiled.textContent).toContain('120');
    expect(compiled.textContent).toContain('RPM');
    expect(compiled.textContent).toContain('Real-time proxy throughput');
  });

  it('should render trend delta and apply positive/negative styling', () => {
    component.trendDelta = 12.5;
    fixture.detectChanges();

    expect(component.formattedTrendDelta).toBe('+12.5%');
    expect(component.isTrendPositive).toBeTrue();
    expect(component.trendClasses).toContain('text-emerald-400');

    component.trendDelta = -5.2;
    fixture.detectChanges();

    expect(component.formattedTrendDelta).toBe('-5.2%');
    expect(component.isTrendNegative).toBeTrue();
    expect(component.trendClasses).toContain('text-rose-400');
  });
});
