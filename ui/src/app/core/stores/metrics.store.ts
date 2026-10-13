import { HttpClient } from '@angular/common/http';
import { Injectable, inject, signal, computed, DestroyRef, Injector } from '@angular/core';
import { ModelCatalogStore } from './model-catalog.store';
import { ConfigStore } from './config.store';
import { SseService } from '../services/sse.service';
import { MetricsTickData, RequestTraceData, DashboardEvent, ErrorLoggedData } from '../models/telemetry.models';

const MAX_RECENT_ERRORS = 20;
const BACKFILL_LIMIT = 50;

export interface MetricsStoreOptions {
  maxTraces?: number;
  maxTimeSeriesPoints?: number;
}

/**
 * Higher revision wins; at equal revision an in-flight placeholder (status 0) never replaces a
 * row that already has a status, so a slow backfill cannot undo a finished trace.
 */
function supersedes(incoming: RequestTraceData, existing: RequestTraceData): boolean {
  if (incoming.traceRevision !== existing.traceRevision) {
    return incoming.traceRevision > existing.traceRevision;
  }
  return !(incoming.statusCode === 0 && existing.statusCode !== 0);
}

@Injectable({
  providedIn: 'root'
})
export class MetricsStore {
  private readonly sseService = inject(SseService);
  private readonly injector = inject(Injector);
  // Optional so stores built without HTTP wiring (isolated specs) simply skip backfill.
  private readonly http = inject(HttpClient, { optional: true });
  private readonly destroyRef = inject(DestroyRef, { optional: true });

  private readonly maxTracesLimit: number;
  private readonly maxTimeSeriesLimit: number;

  private readonly metricsSignal = signal<MetricsTickData | null>(null);
  private readonly recentTracesSignal = signal<RequestTraceData[]>([]);
  private readonly timeSeriesPointsSignal = signal<MetricsTickData[]>([]);

  public readonly metrics = this.metricsSignal.asReadonly();
  public readonly recentTraces = this.recentTracesSignal.asReadonly();
  public readonly timeSeriesPoints = this.timeSeriesPointsSignal.asReadonly();
  private readonly lagSkippedSignal = signal<number | null>(null);
  private readonly recentErrorsSignal = signal<ErrorLoggedData[]>([]);
  private readonly errorsOverflowedSignal = signal<boolean>(false);
  public readonly lagSkipped = this.lagSkippedSignal.asReadonly();
  public readonly recentErrors = this.recentErrorsSignal.asReadonly();
  public readonly errorsOverflowed = this.errorsOverflowedSignal.asReadonly();
  public readonly connectionStatus = this.sseService.connectionStatus;
  public readonly retryCount = this.sseService.retryCount;

  public readonly rpm = computed(() => this.metricsSignal()?.rpm ?? 0);
  public readonly currentLagMs = computed(() => this.metricsSignal()?.currentLagMs ?? 0);
  public readonly isContended = computed(() => (this.metricsSignal()?.currentLagMs ?? 0) > 100);
  public readonly totalRequests = computed(() => this.metricsSignal()?.totalRequests ?? 0);
  public readonly tokensSaved = computed(() => this.metricsSignal()?.totalTokensSaved ?? 0);
  public readonly tps = computed(() => this.metricsSignal()?.tps ?? null);
  public readonly medianLatencyMs = computed(() => this.metricsSignal()?.medianLatencyMs ?? null);
  public readonly medianFirstByteMs = computed(() => this.metricsSignal()?.medianFirstByteMs ?? null);
  public readonly tokenSavingsPercent = computed(() => this.metricsSignal()?.tokenSavingsPercent ?? null);

  constructor() {
    this.maxTracesLimit = 500;
    this.maxTimeSeriesLimit = 720;

    const sub = this.sseService.events$.subscribe((event: DashboardEvent) => {
      this.handleDashboardEvent(event);
    });

    const reconnectSub = this.sseService.reconnected$.subscribe(() => this.backfillTraces());
    this.backfillTraces();
    if (this.destroyRef) {
      this.destroyRef.onDestroy(() => {
        reconnectSub.unsubscribe();
        sub.unsubscribe();
      });
    }
  }

  public reconnect(): void {
    this.sseService.reconnect();
  }

  public handleDashboardEvent(event: DashboardEvent): void {
    if (event.type === 'system_lag') {
      this.lagSkippedSignal.set(event.data.skipped);
      this.backfillTraces();
      return;
    }
    this.lagSkippedSignal.set(null);
    if (event.type === 'MetricsTick') {
      this.pushTick(event.data);
    } else if (event.type === 'RequestTrace') {
      this.pushTrace(event.data);
    } else if (event.type === 'ErrorLogged') {
      this.pushError(event.data);
    } else if (event.type === 'ConfigChanged') {
      this.injector.get(ConfigStore).loadConfig();
    } else if (event.type === 'ModelCatalogChanged') {
      // Resolved lazily so stores that never see this event need no HttpClient.
      this.injector.get(ModelCatalogStore).onCatalogChanged();
    }
  }

  /** Recovers history after a page load, lag or reconnect; see `mergeBackfill` for the merge rule. */
  public backfillTraces(): void {
    this.http
      ?.get<RequestTraceData[]>(`/v1/dashboard/traces?limit=${BACKFILL_LIMIT}`)
      .subscribe({
        next: (rows) => this.mergeBackfill(rows),
        error: () => undefined, // live events still flow; the next reconnect retries
      });
  }

  // A row replaces the existing one only if `supersedes` allows it (so an older snapshot never
  // overwrites a newer live row); unseen rows are added and the result is ordered newest first.
  private mergeBackfill(rows: RequestTraceData[]): void {
    this.recentTracesSignal.update((existing) => {
      const merged = existing.slice();
      for (const row of rows) {
        const idx = merged.findIndex((t) => t.requestId === row.requestId);
        if (idx === -1) {
          merged.push(row);
        } else if (supersedes(row, merged[idx])) {
          merged[idx] = row;
        }
      }
      merged.sort((a, b) => (a.timestamp < b.timestamp ? 1 : a.timestamp > b.timestamp ? -1 : 0));
      return merged.slice(0, this.maxTracesLimit);
    });
  }

  private pushError(error: ErrorLoggedData): void {
    const next = [error, ...this.recentErrorsSignal()];
    if (next.length > MAX_RECENT_ERRORS) {
      this.errorsOverflowedSignal.set(true);
    }
    this.recentErrorsSignal.set(next.slice(0, MAX_RECENT_ERRORS));
  }

  public pushTrace(trace: RequestTraceData): void {
    this.recentTracesSignal.update((existing) => {
      const idx = existing.findIndex((t) => t.requestId === trace.requestId);
      if (idx === -1) {
        return [trace, ...existing].slice(0, this.maxTracesLimit);
      }
      if (!supersedes(trace, existing[idx])) {
        return existing;
      }
      const updated = existing.slice();
      updated[idx] = trace;
      return updated;
    });
  }

  public pushTick(tick: MetricsTickData): void {
    this.metricsSignal.set(tick);
    this.timeSeriesPointsSignal.update((existing) => {
      const updated = [...existing, tick];
      if (updated.length > this.maxTimeSeriesLimit) {
        return updated.slice(updated.length - this.maxTimeSeriesLimit);
      }
      return updated;
    });
  }

  public clear(): void {
    this.metricsSignal.set(null);
    this.recentTracesSignal.set([]);
    this.timeSeriesPointsSignal.set([]);
    this.lagSkippedSignal.set(null);
    this.recentErrorsSignal.set([]);
    this.errorsOverflowedSignal.set(false);
  }
}
