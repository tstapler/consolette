import { Component, inject } from '@angular/core';
import { CommonModule } from '@angular/common';
import { MetricsStore } from '../../core/stores/metrics.store';
import { MetricKpiCardComponent } from './components/metric-kpi-card.component';
import { ProviderHealthGridComponent } from './components/provider-health-grid.component';
import { RpmTrendChartComponent } from './components/rpm-trend-chart.component';
import { RecentTraceTableComponent } from './components/recent-trace-table.component';
import { InspectDrawerComponent } from './components/inspect-drawer.component';

@Component({
  selector: 'app-overview-dashboard',
  standalone: true,
  imports: [
    CommonModule,
    MetricKpiCardComponent,
    ProviderHealthGridComponent,
    RpmTrendChartComponent,
    RecentTraceTableComponent,
    InspectDrawerComponent
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
          title="Upstream Attempts Per Min"
          [value]="metricsStore.rpm()"
          unit="attempts/min"
          formatType="number"
          subtitle="Counts every upstream attempt, not client requests"
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
        [connectionStatus]="metricsStore.connectionStatus()"
        [retryAttempt]="metricsStore.retryCount()"
        (inspect)="drawer.open($event.trace, $event.origin, $event.scrollToAttempts)"
      ></app-recent-trace-table>

      <app-inspect-drawer #drawer [traces]="metricsStore.recentTraces()"></app-inspect-drawer>
    </div>
  `
})
export class OverviewDashboardComponent {
  public readonly metricsStore = inject(MetricsStore);
}
