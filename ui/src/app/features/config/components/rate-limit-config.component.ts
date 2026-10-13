import { Component, input } from '@angular/core';
import { CommonModule } from '@angular/common';

export interface RateLimitValues {
  rpm?: number;
  tpm?: number;
  concurrent?: number;
}

export const RATE_LIMITS_READ_ONLY_NOTE =
  'Rate limits are configured in the route file; editing is not supported here';

@Component({
  selector: 'app-rate-limit-config',
  standalone: true,
  imports: [CommonModule],
  template: `
    <div class="bg-slate-900 border border-slate-800 rounded-xl p-6 space-y-4">
      <div>
        <h2 class="text-lg font-semibold text-white">Rate Limit Policies</h2>
        <p class="text-xs text-slate-400">{{ note }}</p>
      </div>

      <div class="grid grid-cols-1 md:grid-cols-3 gap-4 pt-2">
        @for (field of fields; track field.key) {
          <div>
            <label class="block text-xs font-medium text-slate-400 mb-1" [attr.for]="'rl-' + field.key">{{ field.label }}</label>
            <input
              type="number"
              disabled
              [id]="'rl-' + field.key"
              [value]="rateLimits()?.[field.key] ?? ''"
              placeholder="Not set here"
              class="w-full bg-slate-950 border border-slate-800 text-sm font-mono text-slate-400 rounded-lg px-3 py-2 opacity-60 cursor-not-allowed"
              [attr.data-testid]="'input-' + field.testId"
            />
          </div>
        }
      </div>
    </div>
  `
})
export class RateLimitConfigComponent {
  public rateLimits = input<RateLimitValues | undefined>();
  public readonly note = RATE_LIMITS_READ_ONLY_NOTE;
  public readonly fields: { key: keyof RateLimitValues; testId: string; label: string }[] = [
    { key: 'rpm', testId: 'rpm', label: 'Requests Per Minute (RPM)' },
    { key: 'tpm', testId: 'tpm', label: 'Tokens Per Minute (TPM)' },
    { key: 'concurrent', testId: 'concurrent', label: 'Concurrent Limit' }
  ];
}
