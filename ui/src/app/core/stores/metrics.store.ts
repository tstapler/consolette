import { Injectable, inject, signal, computed, DestroyRef } from '@angular/core';
import { SseService } from '../services/sse.service';
import { MetricsTickData, RequestTraceData, DashboardEvent } from '../models/telemetry.models';

export interface MetricsStoreOptions {
  maxTraces?: number;
  maxTimeSeriesPoints?: number;
}

@Injectable({
  providedIn: 'root'
})
export class MetricsStore {
  private readonly sseService = inject(SseService);
  private readonly destroyRef = inject(DestroyRef, { optional: true });

  private readonly maxTracesLimit: number;
  private readonly maxTimeSeriesLimit: number;

  private readonly metricsSignal = signal<MetricsTickData | null>(null);
  private readonly recentTracesSignal = signal<RequestTraceData[]>([]);
  private readonly timeSeriesPointsSignal = signal<MetricsTickData[]>([]);

  public readonly metrics = this.metricsSignal.asReadonly();
  public readonly recentTraces = this.recentTracesSignal.asReadonly();
  public readonly timeSeriesPoints = this.timeSeriesPointsSignal.asReadonly();
  public readonly connectionStatus = this.sseService.connectionStatus;

  public readonly rpm = computed(() => this.metricsSignal()?.rpm ?? 0);
  public readonly currentLagMs = computed(() => this.metricsSignal()?.currentLagMs ?? 0);
  public readonly isContended = computed(() => (this.metricsSignal()?.currentLagMs ?? 0) > 100);
  public readonly tokensSaved = computed(() => this.metricsSignal()?.tokensSaved ?? 0);
  public readonly tps = computed(() => this.metricsSignal()?.tps ?? 0);
  public readonly medianTtftMs = computed(() => this.metricsSignal()?.medianTtftMs ?? 0);
  public readonly tokenSavingsPercent = computed(() => this.metricsSignal()?.tokenSavingsPercent ?? 0);

  constructor() {
    this.maxTracesLimit = 500;
    this.maxTimeSeriesLimit = 720;

    const sub = this.sseService.events$.subscribe((event: DashboardEvent) => {
      this.handleDashboardEvent(event);
    });

    if (this.destroyRef) {
      this.destroyRef.onDestroy(() => {
        sub.unsubscribe();
      });
    }
  }

  public handleDashboardEvent(event: DashboardEvent): void {
    if (event.type === 'MetricsTick') {
      this.pushTick(event.data);
    } else if (event.type === 'RequestTrace') {
      this.pushTrace(event.data);
    }
  }

  public pushTrace(trace: RequestTraceData): void {
    this.recentTracesSignal.update((existing) => {
      const updated = [trace, ...existing];
      return updated.slice(0, this.maxTracesLimit);
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
  }
}
