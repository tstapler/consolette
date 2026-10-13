import { ComponentFixture, TestBed } from '@angular/core/testing';
import { provideHttpClient } from '@angular/common/http';
import { provideHttpClientTesting, HttpTestingController } from '@angular/common/http/testing';
import { ModelBenchmarksViewComponent } from './model-benchmarks-view.component';
import { BenchmarkStore } from '../../core/stores/benchmark.store';
import { BenchmarkData } from '../../core/models/telemetry.models';

describe('ModelBenchmarksViewComponent', () => {
  let component: ModelBenchmarksViewComponent;
  let fixture: ComponentFixture<ModelBenchmarksViewComponent>;
  let httpMock: HttpTestingController;
  let store: BenchmarkStore;

  const mockData: BenchmarkData[] = [
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
      tokensSavedPercent: 30,
      aiderScore: 88.0
    }
  ];

  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [ModelBenchmarksViewComponent],
      providers: [
        BenchmarkStore,
        provideHttpClient(),
        provideHttpClientTesting()
      ]
    }).compileComponents();

    fixture = TestBed.createComponent(ModelBenchmarksViewComponent);
    component = fixture.componentInstance;
    httpMock = TestBed.inject(HttpTestingController);
    store = TestBed.inject(BenchmarkStore);
  });

  afterEach(() => {
    httpMock.verify();
  });

  it('should create component and load benchmarks on init', () => {
    fixture.detectChanges();

    const req = httpMock.expectOne('/v1/dashboard/benchmark');
    expect(req.request.method).toBe('GET');
    req.flush(mockData);

    expect(component.benchmarks().length).toBe(1);
  });

  it('should refresh benchmarks on manual refresh call', () => {
    fixture.detectChanges();
    let req = httpMock.expectOne('/v1/dashboard/benchmark');
    req.flush(mockData);

    component.refresh();
    req = httpMock.expectOne('/v1/dashboard/benchmark');
    expect(req.request.method).toBe('GET');
    req.flush(mockData);
  });

  it('benchmarks_should_render_empty_state_and_en_dash_for_null_rates', () => {
    fixture.detectChanges();
    httpMock.expectOne('/v1/dashboard/benchmark').flush([]);
    fixture.detectChanges();
    const empty = (fixture.nativeElement as HTMLElement).textContent ?? '';
    expect(empty).toContain('No traffic yet - benchmarks appear after requests flow');

    store.setBenchmarks([
      {
        model: 'claude-sonnet-4',
        provider: 'anthropic',
        ttftP50: 120,
        ttftP90: 150,
        ttftP95: 180,
        ttftP99: 210,
        speedTokSec: null,
        successRatePercent: null,
        errorRate: null,
        costPer1k: null,
        tokensSavedPercent: 0,
        aiderScore: null
      }
    ]);
    fixture.detectChanges();
    const cells = Array.from(
      (fixture.nativeElement as HTMLElement).querySelectorAll('tbody tr:first-child td')
    ).map((c) => (c.textContent ?? '').trim());
    // speed, error rate, aider score
    expect(cells.slice(4)).toEqual(['\u2013', '\u2013', '\u2013']);
    expect(cells.join(' ')).not.toContain('0.0%');
  });
});
