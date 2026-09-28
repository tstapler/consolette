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
});
