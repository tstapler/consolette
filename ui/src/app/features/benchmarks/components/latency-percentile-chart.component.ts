import { Component, ElementRef, Input, OnDestroy, OnInit, ViewChild, ChangeDetectionStrategy, SimpleChanges, OnChanges } from '@angular/core';
import { CommonModule } from '@angular/common';
import Chart from 'chart.js/auto';
import { BenchmarkData } from '../../../core/models/telemetry.models';

@Component({
  selector: 'app-latency-percentile-chart',
  standalone: true,
  imports: [CommonModule],
  changeDetection: ChangeDetectionStrategy.OnPush,
  template: `
    <div class="bg-neutral-900 border border-neutral-800 rounded-lg p-5 shadow-sm space-y-4">
      <div class="flex items-center justify-between">
        <div>
          <h3 class="text-lg font-semibold text-white">Latency Percentiles & TTFT</h3>
          <p class="text-xs text-neutral-400">p50, p90, p99 distribution across upstreams</p>
        </div>
        <div class="flex items-center space-x-3">
          <label class="flex items-center cursor-pointer space-x-2 text-xs text-neutral-300">
            <span>CDF Mode</span>
            <input type="checkbox" [checked]="isCdfMode" (change)="toggleCdfMode()" class="toggle-checkbox rounded bg-neutral-800 border-neutral-700 text-cyan-500 focus:ring-cyan-500" />
          </label>
        </div>
      </div>
      <div class="relative h-64 w-full">
        <canvas #chartCanvas></canvas>
      </div>
    </div>
  `
})
export class LatencyPercentileChartComponent implements OnInit, OnDestroy, OnChanges {
  @ViewChild('chartCanvas', { static: true }) chartCanvas!: ElementRef<HTMLCanvasElement>;
  @Input() data: BenchmarkData[] = [];

  public chartInstance: Chart | null = null;
  public isCdfMode = false;

  ngOnInit(): void {
    this.initChart();
    if (this.data && this.data.length > 0) {
      this.updateChart(this.data);
    }
  }

  ngOnChanges(changes: SimpleChanges): void {
    if (changes['data'] && !changes['data'].firstChange && this.chartInstance) {
      this.updateChart(this.data);
    }
  }

  private initChart(): void {
    if (!this.chartCanvas) return;

    this.chartInstance = new Chart(this.chartCanvas.nativeElement, {
      type: 'line',
      data: {
        labels: [],
        datasets: [
          {
            label: 'p50 TTFT (ms)',
            data: [],
            borderColor: '#06b6d4',
            backgroundColor: 'rgba(6, 182, 212, 0.1)',
            fill: true,
            tension: 0.3
          },
          {
            label: 'p90 TTFT (ms)',
            data: [],
            borderColor: '#f59e0b',
            backgroundColor: 'rgba(245, 158, 11, 0.1)',
            fill: true,
            tension: 0.3
          },
          {
            label: 'p99 TTFT (ms)',
            data: [],
            borderColor: '#ef4444',
            backgroundColor: 'rgba(239, 68, 68, 0.1)',
            fill: true,
            tension: 0.3
          }
        ]
      },
      options: {
        responsive: true,
        maintainAspectRatio: false,
        scales: {
          x: {
            grid: { color: 'rgba(255, 255, 255, 0.05)' },
            ticks: { color: '#9ca3af' }
          },
          y: {
            grid: { color: 'rgba(255, 255, 255, 0.05)' },
            ticks: { color: '#9ca3af' },
            beginAtZero: true
          }
        },
        plugins: {
          legend: {
            labels: { color: '#e5e7eb' }
          }
        }
      }
    });
  }

  public updateChart(benchmarkData: BenchmarkData[]): void {
    if (!this.chartInstance) return;

    if (this.isCdfMode) {
      this.renderCdf(benchmarkData);
    } else {
      this.renderPercentiles(benchmarkData);
    }
  }

  private renderPercentiles(benchmarkData: BenchmarkData[]): void {
    if (!this.chartInstance) return;

    const timestamp = new Date().toLocaleTimeString();
    const p50Avg = benchmarkData.length > 0 ? benchmarkData.reduce((acc, d) => acc + (d.ttftP50 || 0), 0) / benchmarkData.length : 0;
    const p90Avg = benchmarkData.length > 0 ? benchmarkData.reduce((acc, d) => acc + (d.ttftP90 || 0), 0) / benchmarkData.length : 0;
    const p99Avg = benchmarkData.length > 0 ? benchmarkData.reduce((acc, d) => acc + (d.ttftP99 || 0), 0) / benchmarkData.length : 0;

    this.chartInstance.data.labels?.push(timestamp);
    this.chartInstance.data.datasets[0].data.push(Math.round(p50Avg));
    this.chartInstance.data.datasets[1].data.push(Math.round(p90Avg));
    this.chartInstance.data.datasets[2].data.push(Math.round(p99Avg));

    this.applySlidingWindowCap();
    this.chartInstance.update();
  }

  private renderCdf(benchmarkData: BenchmarkData[]): void {
    if (!this.chartInstance) return;

    const allTtfts = benchmarkData.map(d => d.ttftP50 || 0).sort((a, b) => a - b);
    const labels: string[] = [];
    const probabilities: number[] = [];

    for (let i = 0; i < allTtfts.length; i++) {
      labels.push(`${allTtfts[i]}ms`);
      probabilities.push(Math.round(((i + 1) / allTtfts.length) * 100));
    }

    this.chartInstance.data.labels = labels;
    this.chartInstance.data.datasets[0].label = 'Cumulative Probability (%)';
    this.chartInstance.data.datasets[0].data = probabilities;
    this.chartInstance.data.datasets[1].data = [];
    this.chartInstance.data.datasets[2].data = [];

    this.applySlidingWindowCap();
    this.chartInstance.update();
  }

  public applySlidingWindowCap(): void {
    if (!this.chartInstance || !this.chartInstance.data.labels) return;

    while (this.chartInstance.data.labels.length > 60) {
      this.chartInstance.data.labels.shift();
      this.chartInstance.data.datasets.forEach(dataset => {
        dataset.data.shift();
      });
    }
  }

  public toggleCdfMode(): void {
    this.isCdfMode = !this.isCdfMode;
    if (this.chartInstance) {
      if (this.isCdfMode) {
        this.chartInstance.data.datasets[0].label = 'CDF Probability (%)';
      } else {
        this.chartInstance.data.datasets[0].label = 'p50 TTFT (ms)';
        this.chartInstance.data.datasets[1].label = 'p90 TTFT (ms)';
        this.chartInstance.data.datasets[2].label = 'p99 TTFT (ms)';
      }
      this.updateChart(this.data);
    }
  }

  ngOnDestroy(): void {
    if (this.chartInstance) {
      this.chartInstance.destroy();
      this.chartInstance = null;
    }
  }
}
