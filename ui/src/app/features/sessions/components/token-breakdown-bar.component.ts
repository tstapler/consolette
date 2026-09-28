import { Component, Input, computed } from '@angular/core';
import { CommonModule } from '@angular/common';

@Component({
  selector: 'app-token-breakdown-bar',
  standalone: true,
  imports: [CommonModule],
  template: `
    <div class="bg-neutral-900 border border-neutral-800 rounded-lg p-4 space-y-3">
      <div class="flex items-center justify-between text-sm">
        <span class="font-semibold text-white">Token Usage & Breakdown</span>
        <span class="text-xs font-mono text-slate-400">Total: {{ totalTokens() | number }} tokens</span>
      </div>

      <!-- Segmented Bar -->
      <div class="h-4 w-full bg-neutral-950 rounded-full overflow-hidden flex border border-neutral-800 p-0.5">
        <div
          class="h-full bg-cyan-500 rounded-l transition-all duration-300"
          [style.width.%]="promptPercent()"
          [title]="'Prompt Tokens: ' + promptTokens + ' (' + promptPercent().toFixed(1) + '%)'"
        ></div>
        <div
          class="h-full bg-indigo-500 transition-all duration-300"
          [style.width.%]="responsePercent()"
          [title]="'Response Tokens: ' + responseTokens + ' (' + responsePercent().toFixed(1) + '%)'"
        ></div>
        <div
          class="h-full bg-emerald-500 rounded-r transition-all duration-300"
          [style.width.%]="savedPercent()"
          [title]="'Saved Tokens: ' + savedTokens + ' (' + savedPercent().toFixed(1) + '%)'"
        ></div>
      </div>

      <!-- Legend -->
      <div class="grid grid-cols-3 gap-2 text-xs pt-1">
        <div class="flex items-center gap-1.5">
          <span class="w-3 h-3 rounded bg-cyan-500 inline-block"></span>
          <span class="text-slate-300">Prompt: <strong class="text-white">{{ promptTokens | number }}</strong> ({{ promptPercent() | number:'1.1-1' }}%)</span>
        </div>
        <div class="flex items-center gap-1.5">
          <span class="w-3 h-3 rounded bg-indigo-500 inline-block"></span>
          <span class="text-slate-300">Response: <strong class="text-white">{{ responseTokens | number }}</strong> ({{ responsePercent() | number:'1.1-1' }}%)</span>
        </div>
        <div class="flex items-center gap-1.5">
          <span class="w-3 h-3 rounded bg-emerald-500 inline-block"></span>
          <span class="text-slate-300">Saved: <strong class="text-white">{{ savedTokens | number }}</strong> ({{ savedPercent() | number:'1.1-1' }}%)</span>
        </div>
      </div>
    </div>
  `
})
export class TokenBreakdownBarComponent {
  @Input() promptTokens = 0;
  @Input() responseTokens = 0;
  @Input() savedTokens = 0;

  public readonly totalTokens = computed(() => {
    return Math.max(0, this.promptTokens) + Math.max(0, this.responseTokens) + Math.max(0, this.savedTokens);
  });

  public readonly promptPercent = computed(() => {
    const total = this.totalTokens();
    if (total === 0) return 0;
    return (Math.max(0, this.promptTokens) / total) * 100;
  });

  public readonly responsePercent = computed(() => {
    const total = this.totalTokens();
    if (total === 0) return 0;
    return (Math.max(0, this.responseTokens) / total) * 100;
  });

  public readonly savedPercent = computed(() => {
    const total = this.totalTokens();
    if (total === 0) return 0;
    return (Math.max(0, this.savedTokens) / total) * 100;
  });
}
