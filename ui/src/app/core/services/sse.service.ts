import { Injectable, DestroyRef, inject, signal, computed } from '@angular/core';
import { Observable, Subject } from 'rxjs';
import { DashboardEvent } from '../models/telemetry.models';

export type SseConnectionStatus = 'connected' | 'reconnecting' | 'disconnected';

export interface SseServiceOptions {
  url?: string;
  initialBackoffMs?: number;
  maxBackoffMs?: number;
  maxRetryAttempts?: number;
}

@Injectable({
  providedIn: 'root'
})
export class SseService {
  private readonly destroyRef = inject(DestroyRef, { optional: true });
  private eventSource: EventSource | null = null;
  private readonly eventsSubject = new Subject<DashboardEvent>();
  private reconnectTimer: any = null;

  private readonly statusSignal = signal<SseConnectionStatus>('disconnected');
  private readonly retryCountSignal = signal<number>(0);

  public readonly connectionStatus = this.statusSignal.asReadonly();
  public readonly retryCount = this.retryCountSignal.asReadonly();
  public readonly isConnected = computed(() => this.statusSignal() === 'connected');

  public readonly events$: Observable<DashboardEvent> = this.eventsSubject.asObservable();

  private url = '/v1/dashboard/events';
  private initialBackoffMs = 1000;
  private maxBackoffMs = 30000;
  private maxRetryAttempts = 20;

  constructor() {
    if (this.destroyRef) {
      this.destroyRef.onDestroy(() => {
        this.disconnect();
      });
    }
  }

  public connect(options?: SseServiceOptions): void {
    if (options?.url) this.url = options.url;
    if (options?.initialBackoffMs) this.initialBackoffMs = options.initialBackoffMs;
    if (options?.maxBackoffMs) this.maxBackoffMs = options.maxBackoffMs;
    if (options?.maxRetryAttempts) this.maxRetryAttempts = options.maxRetryAttempts;

    this.initEventSource();
  }

  private initEventSource(): void {
    this.closeExistingSource();

    try {
      this.eventSource = new EventSource(this.url);

      this.eventSource.onopen = () => {
        this.statusSignal.set('connected');
        this.retryCountSignal.set(0);
      };

      this.eventSource.onmessage = (event: MessageEvent) => {
        try {
          const parsed = JSON.parse(event.data) as DashboardEvent;
          this.eventsSubject.next(parsed);
        } catch {
          // If message isn't standard DashboardEvent JSON, emit raw if valid type
        }
      };

      // Listen to named custom events if emitted by SSE server
      ['MetricsTick', 'RequestTrace', 'ErrorLogged', 'ConfigChanged', 'system_lag'].forEach((eventType) => {
        this.eventSource?.addEventListener(eventType, (event: MessageEvent) => {
          try {
            const data = JSON.parse(event.data);
            this.eventsSubject.next({ type: eventType as any, data });
          } catch {
            // ignore JSON parse error
          }
        });
      });

      this.eventSource.onerror = () => {
        this.handleErrorAndReconnect();
      };
    } catch {
      this.handleErrorAndReconnect();
    }
  }

  public handleErrorAndReconnect(): void {
    this.closeExistingSource();

    const currentRetry = this.retryCountSignal();
    if (currentRetry >= this.maxRetryAttempts) {
      this.statusSignal.set('disconnected');
      return;
    }

    this.statusSignal.set('reconnecting');
    const nextRetry = currentRetry + 1;
    this.retryCountSignal.set(nextRetry);

    const backoffMs = Math.min(
      this.initialBackoffMs * Math.pow(2, currentRetry),
      this.maxBackoffMs
    );

    this.clearReconnectTimer();
    this.reconnectTimer = setTimeout(() => {
      if (this.statusSignal() === 'reconnecting') {
        this.initEventSource();
      }
    }, backoffMs);
  }

  public disconnect(): void {
    this.clearReconnectTimer();
    this.closeExistingSource();
    this.statusSignal.set('disconnected');
  }

  private closeExistingSource(): void {
    if (this.eventSource) {
      this.eventSource.close();
      this.eventSource = null;
    }
  }

  private clearReconnectTimer(): void {
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }
}
