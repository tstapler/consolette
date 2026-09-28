import { Component, Input, ChangeDetectionStrategy, signal, computed } from '@angular/core';
import { CommonModule } from '@angular/common';
import { BenchmarkData } from '../../../core/models/telemetry.models';

export type SortField = 'model' | 'provider' | 'ttftP50' | 'durationP50' | 'errorRate' | 'speedTokSec' | 'aiderScore';
export type SortDirection = 'asc' | 'desc';

@Component({
  selector: 'app-benchmark-scorecard',
  standalone: true,
  imports: [CommonModule],
  changeDetection: ChangeDetectionStrategy.OnPush,
  template: `
    <div class="bg-neutral-900 border border-neutral-800 rounded-lg p-5 shadow-sm space-y-4">
      <div class="flex items-center justify-between">
        <div>
          <h3 class="text-lg font-semibold text-white">Comparative Model Scorecard</h3>
          <p class="text-xs text-neutral-400">Sortable metrics, error rates, and aider-polyglot benchmark scores</p>
        </div>
      </div>
      <div class="overflow-x-auto">
        <table class="w-full text-left text-sm text-neutral-300 border-collapse">
          <thead>
            <tr class="border-b border-neutral-800 text-xs font-semibold text-neutral-400 uppercase tracking-wider bg-neutral-950/50">
              <th (click)="sort('model')" class="py-3 px-4 cursor-pointer hover:text-white select-none">
                Model {{ getSortIcon('model') }}
              </th>
              <th (click)="sort('provider')" class="py-3 px-4 cursor-pointer hover:text-white select-none">
                Provider {{ getSortIcon('provider') }}
              </th>
              <th (click)="sort('ttftP50')" class="py-3 px-4 cursor-pointer hover:text-white select-none text-right">
                TTFT (p50/p99) {{ getSortIcon('ttftP50') }}
              </th>
              <th (click)="sort('durationP50')" class="py-3 px-4 cursor-pointer hover:text-white select-none text-right">
                Duration (p50) {{ getSortIcon('durationP50') }}
              </th>
              <th (click)="sort('speedTokSec')" class="py-3 px-4 cursor-pointer hover:text-white select-none text-right">
                Speed (tok/s) {{ getSortIcon('speedTokSec') }}
              </th>
              <th (click)="sort('errorRate')" class="py-3 px-4 cursor-pointer hover:text-white select-none text-right">
                Error Rate {{ getSortIcon('errorRate') }}
              </th>
              <th (click)="sort('aiderScore')" class="py-3 px-4 cursor-pointer hover:text-white select-none text-right">
                Aider Score {{ getSortIcon('aiderScore') }}
              </th>
            </tr>
          </thead>
          <tbody class="divide-y divide-neutral-800/60">
            @for (row of sortedData(); track row.model + row.provider) {
              <tr class="hover:bg-neutral-800/40 transition-colors">
                <td class="py-3 px-4 font-medium text-white">{{ row.model }}</td>
                <td class="py-3 px-4 text-neutral-300">
                  <span class="inline-flex items-center px-2 py-0.5 rounded text-xs font-medium bg-neutral-800 text-cyan-400 border border-neutral-700">
                    {{ row.provider }}
                  </span>
                </td>
                <td class="py-3 px-4 text-right font-mono text-xs text-neutral-200">
                  {{ row.ttftP50 || 0 }}ms <span class="text-neutral-500">/ {{ row.ttftP99 || 0 }}ms</span>
                </td>
                <td class="py-3 px-4 text-right font-mono text-xs text-neutral-200">
                  {{ row.durationP50 || row.ttftP50 || 0 }}ms
                </td>
                <td class="py-3 px-4 text-right font-mono text-xs text-emerald-400">
                  {{ row.speedTokSec || 0 }}
                </td>
                <td class="py-3 px-4 text-right font-mono text-xs">
                  <span [class]="(row.errorRate || (100 - (row.successRatePercent || 100))) > 5 ? 'text-red-400 font-bold' : 'text-neutral-300'">
                    {{ (row.errorRate ?? (100 - (row.successRatePercent || 100))).toFixed(1) }}%
                  </span>
                </td>
                <td class="py-3 px-4 text-right font-mono text-xs font-semibold text-cyan-300">
                  {{ row.aiderScore !== undefined ? row.aiderScore.toFixed(1) + '%' : 'N/A' }}
                </td>
              </tr>
            } @empty {
              <tr>
                <td colspan="7" class="py-8 text-center text-neutral-500 italic">No benchmark metrics available</td>
              </tr>
            }
          </tbody>
        </table>
      </div>
    </div>
  `
})
export class BenchmarkScorecardComponent {
  private readonly rawDataSignal = signal<BenchmarkData[]>([]);
  public readonly sortFieldSignal = signal<SortField>('aiderScore');
  public readonly sortDirectionSignal = signal<SortDirection>('desc');

  @Input() set data(val: BenchmarkData[]) {
    this.rawDataSignal.set(val || []);
  }

  public readonly sortedData = computed(() => {
    const data = [...this.rawDataSignal()];
    const field = this.sortFieldSignal();
    const dir = this.sortDirectionSignal() === 'asc' ? 1 : -1;

    return data.sort((a, b) => {
      let valA: any = a[field as keyof BenchmarkData];
      let valB: any = b[field as keyof BenchmarkData];

      if (field === 'errorRate') {
        valA = a.errorRate ?? (100 - (a.successRatePercent || 100));
        valB = b.errorRate ?? (100 - (b.successRatePercent || 100));
      } else if (field === 'durationP50') {
        valA = a.durationP50 ?? a.ttftP50 ?? 0;
        valB = b.durationP50 ?? b.ttftP50 ?? 0;
      }

      if (valA === undefined || valA === null) return 1;
      if (valB === undefined || valB === null) return -1;

      if (typeof valA === 'string') {
        return valA.localeCompare(valB) * dir;
      }
      return (valA - valB) * dir;
    });
  });

  public sort(field: SortField): void {
    if (this.sortFieldSignal() === field) {
      this.sortDirectionSignal.set(this.sortDirectionSignal() === 'asc' ? 'desc' : 'asc');
    } else {
      this.sortFieldSignal.set(field);
      this.sortDirectionSignal.set('desc');
    }
  }

  public getSortIcon(field: SortField): string {
    if (this.sortFieldSignal() !== field) return '↕';
    return this.sortDirectionSignal() === 'asc' ? '↑' : '↓';
  }
}
