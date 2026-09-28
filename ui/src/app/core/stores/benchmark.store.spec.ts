import { TestBed } from '@angular/core/testing';
import { BenchmarkStore } from './benchmark.store';
import { HttpTestingController, provideHttpClientTesting } from '@angular/common/http/testing';
import { provideHttpClient } from '@angular/common/http';
import { BenchmarkData } from '../models/telemetry.models';

describe('BenchmarkStore (TC-UNIT-11)', () => {
  let store: BenchmarkStore;
  let httpMock: HttpTestingController;

  beforeEach(() => {
    TestBed.configureTestingModule({
      providers: [
        BenchmarkStore,
        provideHttpClient(),
        provideHttpClientTesting()
      ]
    });

    store = TestBed.inject(BenchmarkStore);
    httpMock = TestBed.inject(HttpTestingController);
  });

  afterEach(() => {
    httpMock.verify();
  });

  it('TC-UNIT-11: should update benchmarks state on loadBenchmarks HTTP response', () => {
    const mockBenchmarks: BenchmarkData[] = [
      {
        model: 'claude-3-5-sonnet',
        provider: 'anthropic',
        ttftP50: 120,
        ttftP90: 250,
        ttftP95: 310,
        ttftP99: 450,
        speedTokSec: 65,
        successRatePercent: 99.8,
        costPer1k: 0.003,
        tokensSavedPercent: 35
      }
    ];

    store.loadBenchmarks();
    expect(store.isLoading()).toBeTrue();

    const req = httpMock.expectOne('/v1/dashboard/benchmark');
    expect(req.request.method).toBe('GET');
    req.flush(mockBenchmarks);

    expect(store.isLoading()).toBeFalse();
    expect(store.benchmarks()).toEqual(mockBenchmarks);
  });
});
