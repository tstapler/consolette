import { Component, EventEmitter, Input, Output } from '@angular/core';
import { CommonModule } from '@angular/common';
import { RequestTraceData } from '../../../core/models/telemetry.models';

@Component({
  selector: 'app-recent-trace-table',
  standalone: true,
  imports: [CommonModule],
  template: `
    <div class="bg-neutral-900 border border-neutral-800 rounded-xl p-5">
      <div class="flex items-center justify-between mb-4">
        <div>
          <h3 class="text-sm font-semibold text-white tracking-wide uppercase">Live Request Traces</h3>
          <p class="text-xs text-neutral-400 mt-0.5">Real-time SSE proxy request telemetry feed</p>
        </div>
        <span class="text-xs font-mono px-2.5 py-1 bg-neutral-800 text-neutral-300 rounded-full border border-neutral-700">
          {{ traces.length }} Traces
        </span>
      </div>

      <div class="overflow-x-auto">
        <table class="w-full text-left text-xs text-neutral-300">
          <thead class="bg-neutral-950 text-neutral-400 uppercase text-[10px] tracking-wider border-b border-neutral-800">
            <tr>
              <th scope="col" class="py-3 px-4">Time</th>
              <th scope="col" class="py-3 px-4">Request ID</th>
              <th scope="col" class="py-3 px-4">Provider / Model</th>
              <th scope="col" class="py-3 px-4">Duration</th>
              <th scope="col" class="py-3 px-4">TTFT</th>
              <th scope="col" class="py-3 px-4">Tokens (Saved)</th>
              <th scope="col" class="py-3 px-4">Status</th>
              <th scope="col" class="py-3 px-4 text-right">Actions</th>
            </tr>
          </thead>
          <tbody class="divide-y divide-neutral-800/60">
            <tr *ngFor="let trace of traces; trackBy: trackById" class="hover:bg-neutral-800/40 transition-colors">
              <td class="py-3 px-4 font-mono text-neutral-400 whitespace-nowrap">
                {{ formatTime(trace.timestamp) }}
              </td>
              <td class="py-3 px-4 font-mono font-medium text-white whitespace-nowrap">
                {{ getShortId(trace) }}
              </td>
              <td class="py-3 px-4 whitespace-nowrap">
                <div class="flex items-center gap-1.5">
                  <span class="font-medium text-neutral-200">{{ trace.provider }}</span>
                  <span class="px-1.5 py-0.5 text-[10px] font-mono bg-neutral-800 text-cyan-400 rounded border border-neutral-700">
                    {{ trace.model }}
                  </span>
                </div>
              </td>
              <td class="py-3 px-4 font-mono text-neutral-300 whitespace-nowrap">
                {{ trace.durationMs }}ms
              </td>
              <td class="py-3 px-4 font-mono text-neutral-400 whitespace-nowrap">
                {{ trace.firstByteMs }}ms
              </td>
              <td class="py-3 px-4 font-mono whitespace-nowrap">
                <span class="text-neutral-300">{{ trace.tokensAfter }}</span>
                <span *ngIf="getSavedPercent(trace) > 0" class="ml-1.5 text-emerald-400 font-semibold text-[10px]">
                  (-{{ getSavedPercent(trace) }}%)
                </span>
              </td>
              <td class="py-3 px-4 whitespace-nowrap">
                <span
                  class="px-2 py-0.5 text-[10px] font-semibold font-mono rounded-full border"
                  [ngClass]="getStatusBadgeClass(trace.statusCode)"
                >
                  {{ trace.statusCode }}
                </span>
              </td>
              <td class="py-3 px-4 text-right whitespace-nowrap">
                <button
                  (click)="onInspect(trace)"
                  class="px-2.5 py-1 text-[11px] font-medium bg-neutral-800 hover:bg-neutral-700 text-neutral-200 rounded border border-neutral-700 transition-colors"
                >
                  Inspect
                </button>
              </td>
            </tr>
            <tr *ngIf="traces.length === 0">
              <td colspan="8" class="py-8 text-center text-neutral-500 font-medium">
                No request traces recorded yet. Waiting for live telemetry stream...
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>
  `
})
export class RecentTraceTableComponent {
  @Input() traces: RequestTraceData[] = [];
  @Output() inspectPayload = new EventEmitter<RequestTraceData>();
  @Output() selectTrace = new EventEmitter<RequestTraceData>();

  public onInspect(trace: RequestTraceData): void {
    this.inspectPayload.emit(trace);
    this.selectTrace.emit(trace);
  }

  public getShortId(trace: RequestTraceData): string {
    const rawId = trace.requestId || trace.id || 'unknown';
    return rawId.length > 8 ? rawId.substring(0, 8) + '...' : rawId;
  }

  public formatTime(timestamp: string): string {
    if (!timestamp) return '';
    try {
      const d = new Date(timestamp);
      return d.toLocaleTimeString([], { hour12: false, hour: '2-digit', minute: '2-digit', second: '2-digit' });
    } catch {
      return timestamp;
    }
  }

  public getSavedPercent(trace: RequestTraceData): number {
    if (!trace.tokensBefore || trace.tokensBefore <= 0) return 0;
    const diff = trace.tokensBefore - trace.tokensAfter;
    if (diff <= 0) return 0;
    return Math.round((diff / trace.tokensBefore) * 100);
  }

  public getStatusBadgeClass(code: number): string {
    if (code >= 200 && code < 300) {
      return 'bg-emerald-950/80 text-emerald-400 border-emerald-800/60';
    }
    if (code === 429) {
      return 'bg-amber-950/80 text-amber-400 border-amber-800/60';
    }
    if (code >= 400 && code < 500) {
      return 'bg-yellow-950/80 text-yellow-400 border-yellow-800/60';
    }
    if (code >= 500) {
      return 'bg-rose-950/80 text-rose-400 border-rose-800/60';
    }
    return 'bg-neutral-800 text-neutral-300 border-neutral-700';
  }

  public trackById(_: number, item: RequestTraceData): string {
    return item.id || item.requestId || `${item.timestamp}-${item.provider}`;
  }
}
