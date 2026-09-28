import { Component, input, output } from '@angular/core';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';

export interface RateLimitValues {
  rpm?: number;
  tpm?: number;
  concurrent?: number;
}

@Component({
  selector: 'app-rate-limit-config',
  standalone: true,
  imports: [CommonModule, FormsModule],
  template: `
    <div class="bg-slate-900 border border-slate-800 rounded-xl p-6 space-y-4">
      <div>
        <h2 class="text-lg font-semibold text-white">Rate Limit Policies</h2>
        <p class="text-xs text-slate-400">Configure global rate limit caps (RPM, TPM, concurrent requests).</p>
      </div>

      <div class="grid grid-cols-1 md:grid-cols-3 gap-4 pt-2">
        <!-- RPM Input -->
        <div>
          <label class="block text-xs font-medium text-slate-400 mb-1">Requests Per Minute (RPM)</label>
          <input
            type="number"
            min="0"
            [ngModel]="rateLimits()?.rpm || 0"
            (ngModelChange)="onRpmChange($event)"
            class="w-full bg-slate-950 border border-slate-800 text-sm font-mono text-slate-200 rounded-lg px-3 py-2 focus:border-cyan-500 focus:outline-none"
            data-testid="input-rpm"
          />
          @if (isInvalid(rateLimits()?.rpm)) {
            <p class="text-[10px] text-rose-400 mt-1">Must be a non-negative integer</p>
          }
        </div>

        <!-- TPM Input -->
        <div>
          <label class="block text-xs font-medium text-slate-400 mb-1">Tokens Per Minute (TPM)</label>
          <input
            type="number"
            min="0"
            [ngModel]="rateLimits()?.tpm || 0"
            (ngModelChange)="onTpmChange($event)"
            class="w-full bg-slate-950 border border-slate-800 text-sm font-mono text-slate-200 rounded-lg px-3 py-2 focus:border-cyan-500 focus:outline-none"
            data-testid="input-tpm"
          />
          @if (isInvalid(rateLimits()?.tpm)) {
            <p class="text-[10px] text-rose-400 mt-1">Must be a non-negative integer</p>
          }
        </div>

        <!-- Concurrent Requests Input -->
        <div>
          <label class="block text-xs font-medium text-slate-400 mb-1">Concurrent Limit</label>
          <input
            type="number"
            min="0"
            [ngModel]="rateLimits()?.concurrent || 0"
            (ngModelChange)="onConcurrentChange($event)"
            class="w-full bg-slate-950 border border-slate-800 text-sm font-mono text-slate-200 rounded-lg px-3 py-2 focus:border-cyan-500 focus:outline-none"
            data-testid="input-concurrent"
          />
          @if (isInvalid(rateLimits()?.concurrent)) {
            <p class="text-[10px] text-rose-400 mt-1">Must be a non-negative integer</p>
          }
        </div>
      </div>
    </div>
  `
})
export class RateLimitConfigComponent {
  public rateLimits = input<RateLimitValues | undefined>();
  public rateLimitsChange = output<RateLimitValues>();

  public isInvalid(val?: number): boolean {
    return val !== undefined && (val < 0 || isNaN(val));
  }

  public onRpmChange(val: number): void {
    const num = Math.max(0, Number(val) || 0);
    this.rateLimitsChange.emit({
      ...this.rateLimits(),
      rpm: num
    });
  }

  public onTpmChange(val: number): void {
    const num = Math.max(0, Number(val) || 0);
    this.rateLimitsChange.emit({
      ...this.rateLimits(),
      tpm: num
    });
  }

  public onConcurrentChange(val: number): void {
    const num = Math.max(0, Number(val) || 0);
    this.rateLimitsChange.emit({
      ...this.rateLimits(),
      concurrent: num
    });
  }
}
