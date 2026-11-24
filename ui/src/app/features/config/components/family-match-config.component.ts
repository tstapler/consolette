import { Component, OnInit, inject, signal } from '@angular/core';
import { CommonModule } from '@angular/common';
import { HttpClient } from '@angular/common/http';

export type FamilyMatch = 'shortest' | 'newest';

const FAMILY_MATCH_URL = '/api/family-match';

export const FAMILY_MATCH_OPTIONS: ReadonlyArray<{ value: FamilyMatch; label: string; hint: string }> = [
  {
    value: 'shortest',
    label: 'Shortest match',
    hint: 'Try the id equal to the family first (gpt-6.1-sol before gpt-6.1-sol-healthcare), then newest.'
  },
  {
    value: 'newest',
    label: 'Newest version',
    hint: 'Prefer the highest version number among every id that starts with the prefix.'
  }
];

/** Saves immediately via `/api/family-match`; independent of the Apply Config draft. */
@Component({
  selector: 'app-family-match-config',
  standalone: true,
  imports: [CommonModule],
  template: `
    <div class="bg-slate-900 border border-slate-800 rounded-xl p-6 space-y-4">
      <div>
        <h2 class="text-lg font-semibold text-white">Model Family Matching</h2>
        <p class="text-xs text-slate-400">
          How a <code>model_family</code> prefix orders the catalog ids it matches. Applies immediately; no Apply Config needed.
        </p>
      </div>
      <div class="flex flex-wrap items-center gap-3">
        <label class="text-xs font-medium text-slate-400" for="family-match-select">Match order</label>
        <select
          id="family-match-select"
          class="bg-slate-800 border border-slate-700 text-slate-200 text-xs rounded-lg px-3 py-2"
          [value]="mode()"
          [disabled]="saving()"
          (change)="onChange($any($event.target).value)"
          data-testid="family-match-select"
        >
          @for (opt of options; track opt.value) {
            <option [value]="opt.value">{{ opt.label }}</option>
          }
        </select>
        @if (saving()) {
          <span class="text-xs text-slate-400">Saving…</span>
        }
      </div>
      <p class="text-xs text-slate-400" data-testid="family-match-hint">{{ hint() }}</p>
      @if (error()) {
        <p class="text-xs text-rose-400" data-testid="family-match-error">⚠️ {{ error() }}</p>
      }
    </div>
  `
})
export class FamilyMatchConfigComponent implements OnInit {
  private readonly http = inject(HttpClient);
  private touched = false;

  public readonly options = FAMILY_MATCH_OPTIONS;
  public readonly mode = signal<FamilyMatch>('shortest');
  public readonly saving = signal(false);
  public readonly error = signal<string | null>(null);

  public hint(): string {
    return this.options.find((o) => o.value === this.mode())?.hint ?? '';
  }

  public ngOnInit(): void {
    this.http.get<{ family_match: FamilyMatch }>(FAMILY_MATCH_URL).subscribe({
      next: (res) => {
        if (!this.touched) {
          this.mode.set(res.family_match);
        }
      },
      error: () => this.error.set('Could not load the current match order')
    });
  }

  public onChange(value: FamilyMatch): void {
    this.touched = true;
    const previous = this.mode();
    this.mode.set(value);
    this.saving.set(true);
    this.error.set(null);
    this.http.post<{ family_match: FamilyMatch }>(FAMILY_MATCH_URL, { family_match: value }).subscribe({
      next: (res) => {
        this.mode.set(res.family_match);
        this.saving.set(false);
      },
      error: (err) => {
        this.mode.set(previous);
        this.saving.set(false);
        this.error.set(err?.error?.error ?? 'Failed to save the match order');
      }
    });
  }
}
