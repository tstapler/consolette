import { Injectable, inject, signal } from '@angular/core';
import { HttpClient } from '@angular/common/http';
import { BenchmarkData } from '../models/telemetry.models';
import { catchError, of } from 'rxjs';

@Injectable({
  providedIn: 'root'
})
export class BenchmarkStore {
  private readonly http = inject(HttpClient);

  private readonly benchmarksSignal = signal<BenchmarkData[]>([]);
  private readonly isLoadingSignal = signal<boolean>(false);
  private readonly errorSignal = signal<string | null>(null);

  public readonly benchmarks = this.benchmarksSignal.asReadonly();
  public readonly isLoading = this.isLoadingSignal.asReadonly();
  public readonly error = this.errorSignal.asReadonly();

  public loadBenchmarks(): void {
    this.isLoadingSignal.set(true);
    this.errorSignal.set(null);

    this.http.get<BenchmarkData[]>('/v1/dashboard/benchmark')
      .pipe(
        catchError((err) => {
          this.errorSignal.set(err.message || 'Failed to load benchmarks');
          this.isLoadingSignal.set(false);
          return of([]);
        })
      )
      .subscribe((data) => {
        this.benchmarksSignal.set(data);
        this.isLoadingSignal.set(false);
      });
  }

  public setBenchmarks(data: BenchmarkData[]): void {
    this.benchmarksSignal.set(data);
  }
}
