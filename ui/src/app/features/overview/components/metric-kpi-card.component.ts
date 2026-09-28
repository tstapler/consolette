import { Component, Input } from '@angular/core';
import { CommonModule } from '@angular/common';

@Component({
  selector: 'app-metric-kpi-card',
  standalone: true,
  imports: [CommonModule],
  template: `
    <div class="bg-neutral-900 border border-neutral-800 rounded-xl p-5 shadow-sm">
      <div class="flex items-center justify-between">
        <span class="text-xs font-semibold uppercase tracking-wider text-neutral-400">{{ title }}</span>
        <span *ngIf="trendDelta !== undefined && trendDelta !== null" [ngClass]="trendClasses" class="text-xs font-medium px-2 py-0.5 rounded-full flex items-center gap-1">
          <span *ngIf="isTrendPositive">↑</span>
          <span *ngIf="isTrendNegative">↓</span>
          {{ formattedTrendDelta }}
        </span>
      </div>
      <div class="mt-3 flex items-baseline gap-2">
        <span class="text-2xl font-bold text-white tracking-tight">{{ formattedValue }}</span>
        <span *ngIf="unit" class="text-xs font-medium text-neutral-400">{{ unit }}</span>
      </div>
      <div *ngIf="subtitle" class="mt-1 text-xs text-neutral-500">
        {{ subtitle }}
      </div>
    </div>
  `
})
export class MetricKpiCardComponent {
  @Input() title: string = '';
  @Input() value: number | string = 0;
  @Input() unit?: string;
  @Input() trendDelta?: number | string;
  @Input() subtitle?: string;
  @Input() formatType: 'number' | 'compact' | 'percent' | 'ms' | 'raw' = 'number';
  @Input() isPositiveGood: boolean = true;

  get formattedValue(): string {
    if (typeof this.value === 'string') return this.value;
    if (this.value === null || this.value === undefined) return '0';

    switch (this.formatType) {
      case 'compact':
        return this.formatCompact(this.value);
      case 'percent':
        return `${this.value.toFixed(1)}%`;
      case 'ms':
        return `${Math.round(this.value)}ms`;
      case 'raw':
        return `${this.value}`;
      case 'number':
      default:
        return this.value.toLocaleString();
    }
  }

  get formattedTrendDelta(): string {
    if (this.trendDelta === undefined || this.trendDelta === null) return '';
    if (typeof this.trendDelta === 'string') return this.trendDelta;
    const sign = this.trendDelta > 0 ? '+' : '';
    return `${sign}${this.trendDelta.toFixed(1)}%`;
  }

  get isTrendPositive(): boolean {
    if (typeof this.trendDelta === 'number') return this.trendDelta > 0;
    if (typeof this.trendDelta === 'string') return this.trendDelta.startsWith('+');
    return false;
  }

  get isTrendNegative(): boolean {
    if (typeof this.trendDelta === 'number') return this.trendDelta < 0;
    if (typeof this.trendDelta === 'string') return this.trendDelta.startsWith('-');
    return false;
  }

  get trendClasses(): string {
    if (!this.trendDelta) return 'bg-neutral-800 text-neutral-400';
    const positive = this.isTrendPositive;
    const isGood = (positive && this.isPositiveGood) || (!positive && !this.isPositiveGood);
    return isGood
      ? 'bg-emerald-950/60 text-emerald-400 border border-emerald-800/50'
      : 'bg-rose-950/60 text-rose-400 border border-rose-800/50';
  }

  private formatCompact(num: number): string {
    if (num >= 1000000) {
      return (num / 1000000).toFixed(1).replace(/\.0$/, '') + 'M';
    }
    if (num >= 1000) {
      return (num / 1000).toFixed(1).replace(/\.0$/, '') + 'K';
    }
    return num.toString();
  }
}
