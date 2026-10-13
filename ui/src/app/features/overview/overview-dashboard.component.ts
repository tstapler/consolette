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
            <span data-status-dot class="w-2 h-2 rounded-full border" [ngClass]="metricsStore.connectionStatus() === 'connected' ? 'bg-emerald-400 border-emerald-200 motion-safe:animate-pulse' : 'bg-amber-400 border-amber-200'"></span>
            {{ metricsStore.connectionStatus() }}
          </span>
        </div>
      </div>

      <div
        *ngIf="metricsStore.connectionStatus() === 'disconnected'"
        data-disconnected-alert
        role="alert"
        class="flex items-center justify-between gap-3 text-sm text-rose-300 border border-rose-800/60 bg-rose-950/40 rounded px-3 py-2"
      >
        <span>Live feed disconnected. Data below is stale.</span>
        <button
          type="button"
          class="px-3 py-1 text-xs font-semibold rounded border border-rose-700 text-rose-200 hover:bg-rose-900/50 focus-visible:outline-2 focus-visible:outline-offset-2"
          (click)="metricsStore.reconnect()"
        >Reconnect</button>
      </div>

      <div
        *ngIf="metricsStore.lagSkipped() !== null"
        data-lag-chip
        role="status"
        class="text-xs text-amber-300 border border-amber-800/60 bg-amber-950/40 rounded px-3 py-1 w-fit"
      >feed lagging - {{ metricsStore.lagSkipped() }} events skipped</div>

      <!-- KPI Summary Cards Grid -->
      <div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-5 gap-4">
        <app-metric-kpi-card
          title="Upstream Attempts Per Min"
          [value]="metricsStore.rpm()"
          unit="attempts/min"
          formatType="number"
          subtitle="Counts every upstream attempt, not client requests"
        ></app-metric-kpi-card>

        <app-metric-kpi-card
          title="Tokens Per Second"
          [value]="metricsStore.tps() ?? '\u2013'"
          unit="tok/s"
          formatType="number"
          subtitle="Streaming generation rate"
        ></app-metric-kpi-card>

        <app-metric-kpi-card
          title="Median latency (to headers)"
          [value]="metricsStore.medianLatencyMs() ?? '\u2013'"
          unit="ms"
          formatType="ms"
          subtitle="Time until upstream response headers"
        ></app-metric-kpi-card>

        <app-metric-kpi-card
          title="Median first byte"
          [value]="metricsStore.medianFirstByteMs() ?? 'n/a (not measured)'"
          unit="ms"
          formatType="ms"
          subtitle="First body byte latency"
        ></app-metric-kpi-card>

        <app-metric-kpi-card
          title="Token Savings"
          [value]="metricsStore.tokenSavingsPercent() ?? 'n/a (compression off)'"
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

      <section data-recent-errors class="bg-neutral-900 border border-neutral-800 rounded-xl p-5">
        <h3 class="text-sm font-semibold text-white tracking-wide uppercase">Recent errors</h3>
        <p class="text-xs text-neutral-400 mt-1">Upstream errors since this page loaded, newest first.</p>
        <p *ngIf="metricsStore.recentErrors().length === 0" class="mt-3 text-sm text-neutral-400">No errors since this page loaded.</p>
        <ul *ngIf="metricsStore.recentErrors().length > 0" class="mt-3 space-y-1 text-sm text-neutral-200">
          <li *ngFor="let err of metricsStore.recentErrors()" class="flex flex-wrap gap-x-2">
            <span aria-hidden="true" class="text-rose-400">&#9888;</span>
            <span class="font-mono text-xs text-neutral-400">{{ err.timestamp | date:'mediumTime' }}</span>
            <span>{{ err.provider }} {{ err.errorType }}<ng-container *ngIf="err.statusCode !== null"> ({{ err.statusCode }})</ng-container></span>
            <span class="text-neutral-400">{{ err.message }}</span>
          </li>
        </ul>
        <p *ngIf="metricsStore.errorsOverflowed()" class="mt-2 text-xs text-neutral-400">Showing latest 20</p>
      </section>

      <app-inspect-drawer #drawer [traces]="metricsStore.recentTraces()"></app-inspect-drawer>
    </div>
  `
})
export class OverviewDashboardComponent {
  public readonly metricsStore = inject(MetricsStore);
}
