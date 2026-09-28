import { Component, OnInit, inject, ChangeDetectionStrategy } from '@angular/core';
import { CommonModule } from '@angular/common';
import { BenchmarkStore } from '../../core/stores/benchmark.store';
import { LatencyPercentileChartComponent } from './components/latency-percentile-chart.component';
import { BenchmarkScorecardComponent } from './components/benchmark-scorecard.component';

@Component({
  selector: 'app-model-benchmarks-view',
  standalone: true,
  imports: [
    CommonModule,
    LatencyPercentileChartComponent,
    BenchmarkScorecardComponent
  ],
  changeDetection: ChangeDetectionStrategy.OnPush,
  template: `
    <div class="space-y-6">
      <div class="flex items-center justify-between">
        <div>
          <h1 class="text-2xl font-bold tracking-tight text-white">Model Benchmarks & Telemetry Analytics</h1>
          <p class="text-sm text-neutral-400">Comparative TTFT percentiles, generation speeds, error rates, and aider-polyglot benchmark scores.</p>
        </div>
        <button 
          (click)="refresh()" 
          [disabled]="isLoading()"
          class="px-4 py-2 text-xs font-semibold rounded bg-neutral-800 hover:bg-neutral-700 text-white border border-neutral-700 flex items-center space-x-2 transition-colors disabled:opacity-50 cursor-pointer">
          <svg class="w-3.5 h-3.5" [class.animate-spin]="isLoading()" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15"></path>
          </svg>
          <span>Refresh Metrics</span>
        </button>
      </div>

      @if (error()) {
        <div class="bg-red-950/50 border border-red-800 text-red-300 px-4 py-3 rounded-lg text-sm flex items-center justify-between">
          <span>{{ error() }}</span>
          <button (click)="refresh()" class="underline font-semibold hover:text-red-100">Retry</button>
        </div>
      }

      <div class="grid grid-cols-1 gap-6">
        <app-latency-percentile-chart [data]="benchmarks()"></app-latency-percentile-chart>
        <app-benchmark-scorecard [data]="benchmarks()"></app-benchmark-scorecard>
      </div>
    </div>
  `
})
export class ModelBenchmarksViewComponent implements OnInit {
  public readonly benchmarkStore = inject(BenchmarkStore);
  public readonly benchmarks = this.benchmarkStore.benchmarks;
  public readonly isLoading = this.benchmarkStore.isLoading;
  public readonly error = this.benchmarkStore.error;

  ngOnInit(): void {
    this.benchmarkStore.loadBenchmarks();
  }

  public refresh(): void {
    this.benchmarkStore.loadBenchmarks();
  }
}
