import { Component, EventEmitter, Input, Output, signal, computed } from '@angular/core';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';
import { SessionData } from '../../../core/models/telemetry.models';

@Component({
  selector: 'app-session-list',
  standalone: true,
  imports: [CommonModule, FormsModule],
  template: `
    <div class="bg-neutral-900 border border-neutral-800 rounded-lg p-4 space-y-4">
      <div class="flex flex-col sm:flex-row items-center justify-between gap-3">
        <h3 class="text-lg font-semibold text-white">Active Sessions</h3>
        <div class="flex items-center gap-2 w-full sm:w-auto">
          <input
            type="text"
            [ngModel]="searchQuery()"
            (ngModelChange)="onSearchChange($event)"
            placeholder="Filter by session ID, provider, model..."
            class="w-full sm:w-64 px-3 py-1.5 text-sm bg-neutral-950 border border-neutral-800 rounded-md text-slate-200 placeholder-slate-500 focus:outline-none focus:border-cyan-500"
          />
        </div>
      </div>

      <div class="overflow-x-auto">
        <table class="w-full text-left text-sm text-slate-300">
          <thead class="text-xs uppercase bg-neutral-950 text-slate-400 border-b border-neutral-800">
            <tr>
              <th class="px-4 py-3">Session ID</th>
              <th class="px-4 py-3">Provider / Model</th>
              <th class="px-4 py-3 text-center">Turns</th>
              <th class="px-4 py-3 text-right">Saved %</th>
              <th class="px-4 py-3">Last Active</th>
              <th class="px-4 py-3 text-center">Status</th>
            </tr>
          </thead>
          <tbody class="divide-y divide-neutral-800">
            @for (session of filteredSessions(); track session.id) {
              <tr
                (click)="selectSession(session)"
                class="hover:bg-neutral-800 cursor-pointer transition-colors"
                [class.bg-cyan-950]="selectedSessionId === session.id"
                [class.border-l-2]="selectedSessionId === session.id"
                [class.border-cyan-500]="selectedSessionId === session.id"
              >
                <td class="px-4 py-3 font-mono text-xs font-medium text-cyan-400">
                  {{ session.id }}
                </td>
                <td class="px-4 py-3">
                  <div class="font-medium text-white">{{ session.model }}</div>
                  <div class="text-xs text-slate-500 uppercase">{{ session.provider }}</div>
                </td>
                <td class="px-4 py-3 text-center font-mono">
                  {{ session.turnCount }}
                </td>
                <td class="px-4 py-3 text-right font-mono font-medium text-emerald-400">
                  {{ session.tokenSavingsPercent | number:'1.1-1' }}%
                </td>
                <td class="px-4 py-3 text-xs text-slate-400">
                  {{ session.lastActiveTimestamp }}
                </td>
                <td class="px-4 py-3 text-center">
                  @if (session.pinned) {
                    <span class="inline-flex items-center px-2 py-0.5 rounded text-xs font-medium bg-emerald-950 text-emerald-400 border border-emerald-800">
                      Pinned
                    </span>
                  } @else {
                    <span class="inline-flex items-center px-2 py-0.5 rounded text-xs font-medium bg-neutral-800 text-slate-400 border border-neutral-700">
                      Unpinned
                    </span>
                  }
                </td>
              </tr>
            }
            @if (filteredSessions().length === 0) {
              <tr>
                <td colspan="6" class="px-4 py-8 text-center text-slate-500">
                  No sessions match the search query.
                </td>
              </tr>
            }
          </tbody>
        </table>
      </div>
    </div>
  `
})
export class SessionListComponent {
  @Input() sessions: SessionData[] = [];
  @Input() selectedSessionId: string | null = null;
  @Output() sessionSelected = new EventEmitter<SessionData>();

  public readonly searchQuery = signal<string>('');

  public readonly filteredSessions = computed(() => {
    const q = this.searchQuery().trim().toLowerCase();
    if (!q) return this.sessions;
    return this.sessions.filter(
      (s) =>
        s.id.toLowerCase().includes(q) ||
        (s.model && s.model.toLowerCase().includes(q)) ||
        (s.provider && s.provider.toLowerCase().includes(q))
    );
  });

  public onSearchChange(query: string): void {
    this.searchQuery.set(query);
  }

  public selectSession(session: SessionData): void {
    this.sessionSelected.emit(session);
  }
}
