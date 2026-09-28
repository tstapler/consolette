import { Component, input, output } from '@angular/core';
import { CommonModule } from '@angular/common';

@Component({
  selector: 'app-fallback-cascade-builder',
  standalone: true,
  imports: [CommonModule],
  template: `
    <div class="bg-slate-900 border border-slate-800 rounded-xl p-6 space-y-4">
      <div>
        <h2 class="text-lg font-semibold text-white">Failover Cascade Builder</h2>
        <p class="text-xs text-slate-400">Order upstream providers for automatic failover when 429 or 5xx errors occur.</p>
      </div>

      <div class="space-y-2" data-testid="cascade-list">
        @for (providerName of cascade(); track providerName; let idx = $index) {
          <div class="flex items-center justify-between bg-slate-950 border border-slate-800 rounded-lg p-3"
               [attr.data-testid]="'cascade-item-' + idx">
            <div class="flex items-center space-x-3">
              <span class="px-2 py-0.5 text-xs font-mono rounded bg-slate-800 text-slate-300">
                #{{ idx + 1 }} {{ idx === 0 ? 'Primary' : 'Fallback' }}
              </span>
              <span class="font-medium text-slate-200 capitalize">{{ providerName }}</span>
            </div>

            <div class="flex items-center space-x-1">
              <button
                type="button"
                [disabled]="idx === 0"
                (click)="moveUp(idx)"
                class="px-2 py-1 text-xs rounded bg-slate-800 text-slate-300 hover:bg-slate-700 disabled:opacity-30 disabled:cursor-not-allowed"
                [attr.data-testid]="'move-up-' + idx"
              >
                ▲ Up
              </button>
              <button
                type="button"
                [disabled]="idx === cascade().length - 1"
                (click)="moveDown(idx)"
                class="px-2 py-1 text-xs rounded bg-slate-800 text-slate-300 hover:bg-slate-700 disabled:opacity-30 disabled:cursor-not-allowed"
                [attr.data-testid]="'move-down-' + idx"
              >
                ▼ Down
              </button>
            </div>
          </div>
        }
      </div>
    </div>
  `
})
export class FallbackCascadeBuilderComponent {
  public cascade = input<string[]>([]);
  public cascadeChange = output<string[]>();

  public moveUp(index: number): void {
    if (index <= 0) return;
    const items = [...this.cascade()];
    const temp = items[index - 1];
    items[index - 1] = items[index];
    items[index] = temp;
    this.cascadeChange.emit(items);
  }

  public moveDown(index: number): void {
    if (index >= this.cascade().length - 1) return;
    const items = [...this.cascade()];
    const temp = items[index + 1];
    items[index + 1] = items[index];
    items[index] = temp;
    this.cascadeChange.emit(items);
  }
}
