import { Component, OnInit, inject } from '@angular/core';
import { CommonModule } from '@angular/common';
import { HttpClient } from '@angular/common/http';
import { SessionStore } from '../../core/stores/session.store';
import { SessionData } from '../../core/models/telemetry.models';
import { SessionListComponent } from './components/session-list.component';
import { PayloadDiffViewerComponent } from './components/payload-diff-viewer.component';
import { TokenBreakdownBarComponent } from './components/token-breakdown-bar.component';
import { TraceTimelineComponent } from './components/trace-timeline.component';

@Component({
  selector: 'app-session-replay-view',
  standalone: true,
  imports: [
    CommonModule,
    SessionListComponent,
    PayloadDiffViewerComponent,
    TokenBreakdownBarComponent,
    TraceTimelineComponent
  ],
  template: `
    <div class="space-y-6">
      <div>
        <h1 class="text-2xl font-bold tracking-tight text-white">Session Replay & Payload Inspector</h1>
        <p class="text-slate-400 text-sm">Interactive session timeline, prompt compression diffing, and token metrics breakdown.</p>
      </div>

      <div class="grid grid-cols-1 lg:grid-cols-3 gap-6">
        <!-- Left Column: Session List (1 col) -->
        <div class="lg:col-span-1">
          <app-session-list
            [sessions]="sessionStore.sessions()"
            [selectedSessionId]="sessionStore.selectedSessionId()"
            (sessionSelected)="onSessionSelected($event)"
          ></app-session-list>
        </div>

        <!-- Right Column: Timeline, Token Bar, Diff Inspector (2 cols) -->
        <div class="lg:col-span-2 space-y-6">
          @if (sessionStore.selectedSession()) {
            <!-- Pipeline Timeline -->
            @if (sessionStore.selectedSession()?.timelineSteps?.length) {
              <app-trace-timeline [steps]="sessionStore.selectedSession()!.timelineSteps!"></app-trace-timeline>
            } @else {
              <div class="bg-neutral-900 border border-neutral-800 rounded-lg p-4 text-sm text-slate-400">
                <span class="font-semibold text-white">Pipeline Execution Timeline</span>: Not recorded
              </div>
            }

            <!-- Token Breakdown Bar -->
            @if (hasTokenCounts()) {
              <app-token-breakdown-bar
                [promptTokens]="sessionStore.selectedSession()?.promptTokens ?? 0"
                [responseTokens]="sessionStore.selectedSession()?.responseTokens ?? 0"
                [savedTokens]="sessionStore.selectedSession()?.savedTokens ?? 0"
              ></app-token-breakdown-bar>
            } @else {
              <div class="bg-neutral-900 border border-neutral-800 rounded-lg p-4 text-sm text-slate-400">
                <span class="font-semibold text-white">Token Usage & Breakdown</span>: Not recorded
              </div>
            }

            @if (compressedMissingReason) {
              <p class="text-sm text-slate-400" role="status">
                Compressed payload not captured: {{ compressedMissingReason }}
              </p>
            }

            <!-- Payload Diff Inspector -->
            <app-payload-diff-viewer
              [originalPayload]="originalPayload"
              [compressedPayload]="compressedPayload"
            ></app-payload-diff-viewer>
          } @else {
            <div class="bg-neutral-900 border border-neutral-800 rounded-lg p-12 text-center text-slate-400 space-y-2">
              <div class="text-lg font-medium text-slate-400">No Session Selected</div>
              <p class="text-sm">Select a session from the list on the left to inspect execution traces and prompt diffs.</p>
            </div>
          }
        </div>
      </div>
    </div>
  `
})
export class SessionReplayViewComponent implements OnInit {
  public readonly sessionStore = inject(SessionStore);
  private readonly http = inject(HttpClient);

  public originalPayload: any = null;
  public compressedPayload: any = null;
  public compressedMissingReason = '';

  public hasTokenCounts(): boolean {
    const s = this.sessionStore.selectedSession();
    return s?.promptTokens !== undefined || s?.responseTokens !== undefined;
  }

  public ngOnInit(): void {
    this.sessionStore.loadSessions();
  }

  public onSessionSelected(session: SessionData): void {
    this.sessionStore.selectSession(session.id);
    this.originalPayload = null;
    this.compressedPayload = null;
    this.compressedMissingReason = '';
    const requestId = session.latestRequestId;
    if (!requestId) {
      this.originalPayload = { message: 'Original payload not captured: this session has no recorded request' };
      this.compressedMissingReason = 'this session has no recorded request';
      return;
    }
    this.fetchStagePayloads(requestId, session.compressionStatus ?? 'off');
  }

  private fetchStagePayloads(requestId: string, compressionStatus: 'off' | 'none'): void {
    const id = encodeURIComponent(requestId);
    this.http.get(`/requests/${id}?stage=original`).subscribe({
      next: (data) => (this.originalPayload = data),
      error: () => (this.originalPayload = { message: 'Original payload not captured (request left the ring buffer)' })
    });

    this.http.get(`/requests/${id}?stage=compressed`).subscribe({
      next: (data) => (this.compressedPayload = data),
      error: () => {
        this.compressedMissingReason =
          compressionStatus === 'none' ? 'nothing was compressed' : 'compression is off';
      }
    });
  }
}
