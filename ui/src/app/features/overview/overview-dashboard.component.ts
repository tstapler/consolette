import { Component, inject } from '@angular/core';
import { CommonModule } from '@angular/common';
import { Router } from '@angular/router';
import { MetricsStore } from '../../core/stores/metrics.store';
import { MetricKpiCardComponent } from './components/metric-kpi-card.component';
import { ProviderHealthGridComponent } from './components/provider-health-grid.component';
import { RpmTrendChartComponent } from './components/rpm-trend-chart.component';
import { RecentTraceTableComponent } from './components/recent-trace-table.component';
import { RequestTraceData } from '../../core/models/telemetry.models';

@Component({
  selector: 'app-overview-dashboard',
  standalone: true,
  imports: [
    CommonModule,
    MetricKpiCardComponent,
    ProviderHealthGridComponent,
    RpmTrendChartComponent,
    RecentTraceTableComponent
  ],
  template: `
    <div class="space-y-6">
      <!-- Header Bar -->
      <div class="flex items-center justify-between">
        <div>
          <h1 class="text-2xl font-bold tracking-tight text-white">System Overview</h1>
          <p class="text-sm text-neutral-400 mt-1">Real-time metrics, provider health status, and live telemetry feed</p>
        </div>
        <div class="flex items-center gap-2">
          <span
            class="px-3 py-1 text-xs font-semibold rounded-full border flex items-center gap-2"
            [ngClass]="metricsStore.connectionStatus() === 'connected' ? 'bg-emerald-950/80 text-emerald-400 border-emerald-800/60' : 'bg-amber-950/80 text-amber-400 border-amber-800/60'"
          >
            <span class="w-2 h-2 rounded-full" [ngClass]="metricsStore.connectionStatus() === 'connected' ? 'bg-emerald-400 animate-pulse' : 'bg-amber-400'"></span>
            {{ metricsStore.connectionStatus() }}
          </span>
        </div>
      </div>

      <!-- KPI Summary Cards Grid -->
      <div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-4 gap-4">
        <app-metric-kpi-card
          title="Requests Per Min"
          [value]="metricsStore.rpm()"
          unit="RPM"
          formatType="number"
          subtitle="Real-time proxy throughput"
        ></app-metric-kpi-card>

        <app-metric-kpi-card
          title="Tokens Per Second"
          [value]="metricsStore.tps()"
          unit="tok/s"
          formatType="number"
          subtitle="Streaming generation rate"
        ></app-metric-kpi-card>

        <app-metric-kpi-card
          title="Median TTFT"
          [value]="metricsStore.medianTtftMs()"
          unit="ms"
          formatType="ms"
          subtitle="First byte latency"
        ></app-metric-kpi-card>

        <app-metric-kpi-card
          title="Token Savings"
          [value]="metricsStore.tokenSavingsPercent()"
          unit="%"
          formatType="percent"
          subtitle="Prompt compression efficiency"
        ></app-metric-kpi-card>
      </div>

      <!-- Provider Health Grid -->
      <app-provider-health-grid
        [providerHealth]="metricsStore.metrics()?.providerHealth ?? null"
      ></app-provider-health-grid>

      <!-- Telemetry Trend Chart -->
      <app-rpm-trend-chart
        [metricsTick]="metricsStore.metrics()"
        [initialHistory]="metricsStore.timeSeriesPoints()"
      ></app-rpm-trend-chart>

      <!-- Live Recent Traces Feed Table -->
      <app-recent-trace-table
        [traces]="metricsStore.recentTraces()"
        (inspectPayload)="onInspectTrace($event)"
      ></app-recent-trace-table>
    </div>
  `
})
export class OverviewDashboardComponent {
  public readonly metricsStore = inject(MetricsStore);
  private readonly router = inject(Router, { optional: true });

  public onInspectTrace(trace: RequestTraceData): void {
    if (this.router) {
      this.router.navigate(['/sessions'], { queryParams: { requestId: trace.requestId || trace.id } });
    }
  }
}
