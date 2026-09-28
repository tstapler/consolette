import { Component, Input } from '@angular/core';
import { CommonModule } from '@angular/common';

export interface TimelineStep {
  name: string;
  durationMs: number;
  status: 'ok' | 'warning' | 'error' | 'skipped';
  details?: string;
}

@Component({
  selector: 'app-trace-timeline',
  standalone: true,
  imports: [CommonModule],
  template: `
    <div class="bg-neutral-900 border border-neutral-800 rounded-lg p-4 space-y-4">
      <div class="flex items-center justify-between">
        <h3 class="text-lg font-semibold text-white">Pipeline Execution Timeline</h3>
        <span class="text-xs font-mono text-slate-400">Total Duration: {{ totalDurationMs }}ms</span>
      </div>

      <div class="relative flex items-center justify-between w-full py-4 overflow-x-auto">
        <!-- Connecting Line -->
        <div class="absolute top-1/2 left-0 right-0 h-0.5 bg-neutral-800 -translate-y-1/2 z-0"></div>

        @for (step of steps; track step.name) {
          <div class="relative z-10 flex flex-col items-center group px-2">
            <!-- Node Circle -->
            <div
              class="w-8 h-8 rounded-full flex items-center justify-center font-bold text-xs border-2 transition-all"
              [class.bg-emerald-950]="step.status === 'ok'"
              [class.border-emerald-500]="step.status === 'ok'"
              [class.text-emerald-400]="step.status === 'ok'"
              [class.bg-amber-950]="step.status === 'warning'"
              [class.border-amber-500]="step.status === 'warning'"
              [class.text-amber-400]="step.status === 'warning'"
              [class.bg-red-950]="step.status === 'error'"
              [class.border-red-500]="step.status === 'error'"
              [class.text-red-400]="step.status === 'error'"
              [class.bg-neutral-950]="step.status === 'skipped'"
              [class.border-neutral-700]="step.status === 'skipped'"
              [class.text-slate-500]="step.status === 'skipped'"
            >
              @if (step.status === 'ok') { ✓ }
              @else if (step.status === 'warning') { ! }
              @else if (step.status === 'error') { ✕ }
              @else { - }
            </div>

            <!-- Step Label & Duration -->
            <div class="text-center mt-2">
              <div class="text-xs font-medium text-white whitespace-nowrap">{{ step.name }}</div>
              <div class="text-[10px] font-mono text-cyan-400">{{ step.durationMs }}ms</div>
            </div>
          </div>
        }
      </div>
    </div>
  `
})
export class TraceTimelineComponent {
  @Input() steps: TimelineStep[] = [
    { name: 'Ingress', durationMs: 1, status: 'ok' },
    { name: 'RateLimit', durationMs: 2, status: 'ok' },
    { name: 'Compression', durationMs: 12, status: 'ok' },
    { name: 'Router', durationMs: 3, status: 'ok' },
    { name: 'Provider Dispatch', durationMs: 145, status: 'ok' },
    { name: 'Egress', durationMs: 2, status: 'ok' }
  ];

  public get totalDurationMs(): number {
    return this.steps.reduce((acc, step) => acc + step.durationMs, 0);
  }
}
