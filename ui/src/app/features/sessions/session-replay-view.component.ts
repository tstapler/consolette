import { Component, OnInit, inject } from '@angular/core';
import { CommonModule } from '@angular/common';
import { HttpClient } from '@angular/common/http';
import { SessionStore } from '../../core/stores/session.store';
import { SessionData } from '../../core/models/telemetry.models';
import { SessionListComponent } from './components/session-list.component';
import { PayloadDiffViewerComponent } from './components/payload-diff-viewer.component';
import { TokenBreakdownBarComponent } from './components/token-breakdown-bar.component';
import { TraceTimelineComponent, TimelineStep } from './components/trace-timeline.component';

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
            <app-trace-timeline [steps]="currentTimelineSteps"></app-trace-timeline>

            <!-- Token Breakdown Bar -->
            <app-token-breakdown-bar
              [promptTokens]="promptTokens"
              [responseTokens]="responseTokens"
              [savedTokens]="savedTokens"
            ></app-token-breakdown-bar>

            <!-- Payload Diff Inspector -->
            <app-payload-diff-viewer
              [originalPayload]="originalPayload"
              [compressedPayload]="compressedPayload"
            ></app-payload-diff-viewer>
          } @else {
            <div class="bg-neutral-900 border border-neutral-800 rounded-lg p-12 text-center text-slate-500 space-y-2">
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
  public promptTokens = 1250;
  public responseTokens = 450;
  public savedTokens = 320;

  public currentTimelineSteps: TimelineStep[] = [
    { name: 'Ingress', durationMs: 2, status: 'ok' },
    { name: 'RateLimit', durationMs: 1, status: 'ok' },
    { name: 'Compression', durationMs: 14, status: 'ok' },
    { name: 'Router', durationMs: 4, status: 'ok' },
    { name: 'Provider Dispatch', durationMs: 180, status: 'ok' },
    { name: 'Egress', durationMs: 2, status: 'ok' }
  ];

  public ngOnInit(): void {
    this.sessionStore.loadSessions();
  }

  public onSessionSelected(session: SessionData): void {
    this.sessionStore.selectSession(session.id);
    this.fetchStagePayloads(session.id);
  }

  private fetchStagePayloads(requestId: string): void {
    this.http.get(`/requests/${requestId}?stage=original`).subscribe({
      next: (data) => (this.originalPayload = data),
      error: () => (this.originalPayload = { message: 'Original payload snapshot not found' })
    });

    this.http.get(`/requests/${requestId}?stage=compressed`).subscribe({
      next: (data) => (this.compressedPayload = data),
      error: () => (this.compressedPayload = { message: 'Compressed payload snapshot not found (compression skipped or unrecorded)' })
    });
  }
}
