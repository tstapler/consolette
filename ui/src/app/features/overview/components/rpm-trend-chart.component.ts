import {
  Component,
  ElementRef,
  Input,
  OnChanges,
  OnDestroy,
  AfterViewInit,
  ViewChild,
  SimpleChanges
} from '@angular/core';
import { CommonModule } from '@angular/common';
import Chart from 'chart.js/auto';
import { MetricsTickData } from '../../../core/models/telemetry.models';

@Component({
  selector: 'app-rpm-trend-chart',
  standalone: true,
  imports: [CommonModule],
  template: `
    <div class="bg-neutral-900 border border-neutral-800 rounded-xl p-5">
      <div class="flex items-center justify-between mb-4">
        <div>
          <h3 class="text-sm font-semibold text-white tracking-wide uppercase">RPM & Telemetry Trend</h3>
          <p class="text-xs text-neutral-400 mt-0.5">Real-time requests per minute & event loop lag (60s window)</p>
        </div>
        <div class="flex items-center gap-4 text-xs">
          <div class="flex items-center gap-1.5">
            <span class="w-3 h-0.5 bg-cyan-400 rounded"></span>
            <span class="text-neutral-300 font-medium">RPM</span>
          </div>
          <div class="flex items-center gap-1.5">
            <span class="w-3 h-0.5 bg-amber-400 rounded"></span>
            <span class="text-neutral-300 font-medium">Loop Lag (ms)</span>
          </div>
        </div>
      </div>
      <div class="relative h-64 w-full">
        <canvas #chartCanvas></canvas>
      </div>
    </div>
  `
})
export class RpmTrendChartComponent implements AfterViewInit, OnChanges, OnDestroy {
  @ViewChild('chartCanvas') chartCanvas!: ElementRef<HTMLCanvasElement>;

  @Input() metricsTick?: MetricsTickData | null;
  @Input() initialHistory: MetricsTickData[] = [];

  public chartInstance: Chart | null = null;
  public readonly MAX_POINTS = 60;

  ngAfterViewInit(): void {
    this.initChart();
    if (this.initialHistory && this.initialHistory.length > 0) {
      this.loadHistory(this.initialHistory);
    }
  }

  ngOnChanges(changes: SimpleChanges): void {
    if (changes['metricsTick'] && this.metricsTick && this.chartInstance) {
      this.pushDataPoint(this.metricsTick);
    }
    if (changes['initialHistory'] && changes['initialHistory'].currentValue && this.chartInstance) {
      this.loadHistory(changes['initialHistory'].currentValue);
    }
  }

  ngOnDestroy(): void {
    if (this.chartInstance) {
      this.chartInstance.destroy();
      this.chartInstance = null;
    }
  }

  public initChart(): void {
    if (!this.chartCanvas) return;

    const ctx = this.chartCanvas.nativeElement.getContext('2d');
    if (!ctx) return;

    this.chartInstance = new Chart(ctx, {
      type: 'line',
      data: {
        labels: [],
        datasets: [
          {
            label: 'RPM',
            data: [],
            borderColor: '#22d3ee',
            backgroundColor: 'rgba(34, 211, 238, 0.1)',
            borderWidth: 2,
            tension: 0.3,
            fill: true,
            pointRadius: 0,
            pointHoverRadius: 4,
            yAxisID: 'y'
          },
          {
            label: 'Loop Lag (ms)',
            data: [],
            borderColor: '#fbbf24',
            backgroundColor: 'rgba(251, 191, 36, 0.05)',
            borderWidth: 1.5,
            borderDash: [4, 4],
            tension: 0.3,
            fill: false,
            pointRadius: 0,
            pointHoverRadius: 4,
            yAxisID: 'y1'
          }
        ]
      },
      options: {
        responsive: true,
        maintainAspectRatio: false,
        animation: false,
        plugins: {
          legend: { display: false },
          tooltip: {
            mode: 'index',
            intersect: false,
            backgroundColor: '#171717',
            titleColor: '#f5f5f5',
            bodyColor: '#d4d4d4',
            borderColor: '#262626',
            borderWidth: 1
          }
        },
        scales: {
          x: {
            grid: { color: '#262626' },
            ticks: { color: '#a3a3a3', maxRotation: 0 }
          },
          y: {
            type: 'linear',
            display: true,
            position: 'left',
            beginAtZero: true,
            grid: { color: '#262626' },
            ticks: { color: '#a3a3a3' },
            title: { display: false }
          },
          y1: {
            type: 'linear',
            display: true,
            position: 'right',
            beginAtZero: true,
            grid: { drawOnChartArea: false },
            ticks: { color: '#737373' }
          }
        }
      }
    });
  }

  public pushDataPoint(tick: MetricsTickData): void {
    if (!this.chartInstance) return;

    const timeLabel = new Date().toLocaleTimeString([], { hour12: false, hour: '2-digit', minute: '2-digit', second: '2-digit' });

    this.chartInstance.data.labels?.push(timeLabel);
    this.chartInstance.data.datasets[0].data.push(tick.rpm);
    this.chartInstance.data.datasets[1].data.push(tick.currentLagMs);

    if ((this.chartInstance.data.labels?.length ?? 0) > this.MAX_POINTS) {
      this.chartInstance.data.labels?.shift();
      this.chartInstance.data.datasets.forEach((d) => d.data.shift());
    }

    this.chartInstance.update('none');
  }

  public loadHistory(ticks: MetricsTickData[]): void {
    if (!this.chartInstance) return;

    const sliced = ticks.slice(-this.MAX_POINTS);
    const labels = sliced.map((_, i) => `-${sliced.length - i}s`);
    const rpmData = sliced.map((t) => t.rpm);
    const lagData = sliced.map((t) => t.currentLagMs);

    this.chartInstance.data.labels = labels;
    this.chartInstance.data.datasets[0].data = rpmData;
    this.chartInstance.data.datasets[1].data = lagData;
    this.chartInstance.update();
  }
}
