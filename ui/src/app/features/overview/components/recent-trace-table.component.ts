import { ChangeDetectorRef, Component, EventEmitter, Input, Output, inject } from '@angular/core';
import { CommonModule } from '@angular/common';
import { RequestTraceData } from '../../../core/models/telemetry.models';
import { NA_REASON_TEXTS, naReason } from '../../../core/models/na-reason';
import { copyText } from '../../../core/util/copy-text';

/** Task 2.1.6f flips this off and deletes the note and its spec. */
export const TRACE_COVERAGE_NOTE_ENABLED = true;
const MAX_ROWS = 100;
const DASH_LEGEND_LINE = '- = not applicable; hover a dash or open the legend for the reason';
const COVERAGE_NOTE =
  'Rows cover requests that reach routing. Requests rejected before routing, cache hits and multi-round search requests are not shown as one row yet.';

const STATUS_WORDS: Record<number, string> = {
  200: 'OK', 400: 'Bad request', 401: 'Unauthorized', 403: 'Forbidden', 404: 'Not found',
  408: 'Timeout', 422: 'Rejected', 429: 'Rate limited', 499: 'Client closed', 500: 'Server error',
  502: 'Failed', 503: 'Unavailable', 504: 'Timed out', 529: 'Overloaded',
};

export const CHIP_LEGEND: ReadonlyArray<{ chip: string; meaning: string }> = [
  { chip: 'pinned', meaning: 'a session pinned this conversation to one upstream' },
  { chip: 'strategy', meaning: 'the routing strategy picked this upstream (name shown in the drawer)' },
  { chip: 'explore', meaning: 'an occasional exploration pick to test another upstream' },
  { chip: 'fallback', meaning: 'an earlier upstream failed, so the next one was tried' },
  { chip: 'held', meaning: 'waited for a rate-limit cooldown, then retried' },
  { chip: 'default', meaning: 'no special rule; the default route' },
  { chip: 'cache', meaning: 'answered from the response cache' },
  { chip: 'local', meaning: 'answered by a local model' },
  { chip: 'cancelled', meaning: 'the client closed the connection' },
];

interface Cell { text: string; reason: string | null; }

interface Row {
  trace: RequestTraceData;
  key: string;
  shortId: string;
  time: string;
  latency: Cell;
  firstByte: Cell;
  tokensBefore: Cell;
  tokensAfter: Cell;
  statusText: string;
  statusGlyph: string;
  statusClass: string;
  statusTitle: string | null;
  subLabel: string | null;
  chip: string | null;
  naDescription: string;
}

const fmt = (n: number): string => n.toLocaleString('en-US');
const isNum = (v: unknown): v is number => typeof v === 'number' && Number.isFinite(v);
const NOT_RECORDED = 'n/a (not recorded)';

/** Sub-label for a successful row that followed a failed attempt; derived, no wire field. */
export function statusSubLabel(t: RequestTraceData): string | null {
  const attempts = t.attempts;
  if (!attempts || attempts.length <= 1) return null;
  if (isNum(t.statusCode) && t.statusCode >= 400) return null;
  return attempts[attempts.length - 1].upstream !== attempts[0].upstream ? 'fell back' : 'retried';
}

@Component({
  selector: 'app-recent-trace-table',
  standalone: true,
  imports: [CommonModule],
  template: `
    <div class="bg-neutral-900 border border-neutral-800 rounded-xl p-5">
      <div class="flex items-center justify-between mb-4">
        <div>
          <h3 class="text-sm font-semibold text-white tracking-wide uppercase">Live Request Traces</h3>
          <p class="text-xs text-neutral-400 mt-0.5">Real-time SSE proxy request telemetry feed</p>
        </div>
        <span class="text-xs font-mono px-2.5 py-1 bg-neutral-800 text-neutral-300 rounded-full border border-neutral-700">
          {{ rows.length }} Traces
        </span>
      </div>

      <div data-banner role="status" *ngIf="connectionStatus === 'reconnecting'" class="mb-3 text-xs text-amber-400 border border-amber-800/60 rounded px-3 py-2">
        Live feed interrupted - reconnecting (attempt {{ retryAttempt }})
      </div>

      <div class="overflow-x-auto" role="region" aria-label="Live request traces, scroll horizontally" tabindex="0">
        <table class="w-full text-left text-xs text-neutral-300">
          <caption class="sr-only">Live request traces</caption>
          <thead class="bg-neutral-950 text-neutral-400 uppercase text-[11px] tracking-wider border-b border-neutral-800">
            <tr>
              <th scope="col" class="py-3 px-4">Time</th>
              <th scope="col" class="py-3 px-4">Request ID</th>
              <th scope="col" class="py-3 px-4">Provider / Model</th>
              <th scope="col" class="py-3 px-4">Latency (to headers)</th>
              <th scope="col" class="py-3 px-4">First byte</th>
              <th scope="col" class="py-3 px-4">Tokens before</th>
              <th scope="col" class="py-3 px-4">Tokens after (saved)</th>
              <th scope="col" class="py-3 px-4">Status</th>
              <th scope="col" class="py-3 px-4 text-right">Actions</th>
            </tr>
          </thead>
          <tbody class="divide-y divide-neutral-800/60" [attr.data-rows-dimmed]="connectionStatus === 'reconnecting' ? '' : null" [class.opacity-50]="connectionStatus === 'reconnecting'">
            <tr data-row *ngFor="let row of rows; trackBy: trackByKey" class="hover:bg-neutral-800/40 transition-colors">
              <td class="py-3 px-4 font-mono text-neutral-400 whitespace-nowrap">{{ row.time }}</td>
              <th scope="row" class="py-3 px-4 font-mono font-medium text-white whitespace-nowrap text-left">
                <span class="inline-flex items-center gap-1.5">
                  <span data-id-text [title]="row.trace.requestId">{{ failedCopyId === row.key ? row.trace.requestId : row.shortId }}</span>
                  <button type="button" [attr.data-copy-id]="row.key" (click)="copyId(row)"
                    class="px-1.5 py-0.5 text-[11px] font-normal bg-neutral-800 hover:bg-neutral-700 text-neutral-200 rounded border border-neutral-700 focus-visible:outline focus-visible:outline-2"
                    [attr.aria-label]="'Copy request ID ' + row.trace.requestId">Copy</button>
                </span>
              </th>
              <td class="py-3 px-4 whitespace-nowrap">
                <div class="flex items-center gap-1.5">
                  <span class="font-medium text-neutral-200">{{ row.trace.provider }}</span>
                  <span class="px-1.5 py-0.5 text-[11px] font-mono bg-neutral-800 text-cyan-400 rounded border border-neutral-700">{{ row.trace.model }}</span>
                  <span *ngIf="row.chip" class="px-1.5 py-0.5 text-[11px] bg-neutral-800 text-neutral-300 rounded border border-neutral-700" [title]="chipMeaning(row.chip)">{{ row.chip }}</span>
                </div>
              </td>
              <td class="py-3 px-4 font-mono text-neutral-300 whitespace-nowrap">
                <ng-container *ngTemplateOutlet="cell; context: { c: row.latency, col: 'latency' }"></ng-container>
              </td>
              <td class="py-3 px-4 font-mono text-neutral-300 whitespace-nowrap">
                <ng-container *ngTemplateOutlet="cell; context: { c: row.firstByte, col: 'firstByte' }"></ng-container>
              </td>
              <td class="py-3 px-4 font-mono text-neutral-300 whitespace-nowrap">
                <ng-container *ngTemplateOutlet="cell; context: { c: row.tokensBefore, col: 'tokensBefore' }"></ng-container>
              </td>
              <td class="py-3 px-4 font-mono text-neutral-300 whitespace-nowrap">
                <ng-container *ngTemplateOutlet="cell; context: { c: row.tokensAfter, col: 'tokensAfter' }"></ng-container>
              </td>
              <td class="py-3 px-4 whitespace-nowrap" data-status [attr.title]="row.statusTitle">
                <span class="px-2 py-0.5 text-[11px] font-semibold font-mono rounded-full border" [ngClass]="row.statusClass">
                  <span aria-hidden="true">{{ row.statusGlyph }}</span> {{ row.statusText }}
                </span>
                <div *ngIf="row.subLabel" class="text-[11px] text-neutral-400 mt-0.5">{{ row.subLabel }}</div>
              </td>
              <td class="py-3 px-4 text-right whitespace-nowrap">
                <button type="button" data-inspect (click)="onInspect(row.trace)"
                  [attr.aria-label]="'Inspect trace ' + row.shortId"
                  [attr.aria-describedby]="row.naDescription ? 'na-' + row.key : null"
                  class="px-2.5 py-1 text-[11px] font-medium bg-neutral-800 hover:bg-neutral-700 text-neutral-200 rounded border border-neutral-700 transition-colors focus-visible:outline focus-visible:outline-2">
                  Inspect
                </button>
                <span *ngIf="row.naDescription" class="sr-only" [id]="'na-' + row.key">{{ row.naDescription }}</span>
              </td>
            </tr>
            <ng-container *ngIf="rows.length === 0">
              <ng-container *ngIf="showSkeleton; else emptyOrDisconnected">
                <tr data-skeleton *ngFor="let _ of skeletonRows" aria-hidden="true">
                  <td colspan="9" class="py-3 px-4"><div class="h-3 rounded bg-neutral-800 animate-pulse"></div></td>
                </tr>
                <tr><td colspan="9" class="py-3 text-center text-neutral-400">Connecting to live feed...</td></tr>
              </ng-container>
              <ng-template #emptyOrDisconnected>
                <tr *ngIf="connectionStatus === 'connected'">
                  <td colspan="9" class="py-8 text-center text-neutral-400 font-medium">
                    <p>No requests yet. Point a client at this proxy to see traces.</p>
                    <p class="mt-2 flex items-center justify-center gap-2">
                      <code class="font-mono text-neutral-200">{{ proxyUrl }}</code>
                      <button type="button" (click)="copyProxyUrl()"
                        class="px-2 py-0.5 text-[11px] bg-neutral-800 hover:bg-neutral-700 text-neutral-200 rounded border border-neutral-700 focus-visible:outline focus-visible:outline-2">Copy URL</button>
                    </p>
                  </td>
                </tr>
                <tr *ngIf="connectionStatus === 'disconnected'">
                  <td colspan="9" class="py-8 text-center text-neutral-400 font-medium">Live feed not connected.</td>
                </tr>
              </ng-template>
            </ng-container>
          </tbody>
        </table>
      </div>

      <div class="mt-3 space-y-2 text-[11px] text-neutral-400">
        <p *ngIf="rows.length >= ${MAX_ROWS}">Showing latest ${MAX_ROWS}</p>
        <p>${DASH_LEGEND_LINE}</p>
        <button type="button" (click)="legendOpen = !legendOpen" [attr.aria-expanded]="legendOpen" aria-controls="trace-legend"
          class="underline focus-visible:outline focus-visible:outline-2">What do these dashes and chips mean?</button>
        <div id="trace-legend" data-legend *ngIf="legendOpen" class="space-y-2">
          <ul class="space-y-0.5">
            <li *ngFor="let r of reasonLegend"><span class="font-mono text-neutral-200">{{ r.text }}</span> <span>{{ r.meaning }}</span></li>
          </ul>
          <ul class="space-y-0.5">
            <li *ngFor="let c of chipLegend"><span class="font-mono text-neutral-200">{{ c.chip }}</span> <span>{{ c.meaning }}</span></li>
          </ul>
        </div>
        <p *ngIf="coverageNoteEnabled" data-coverage-note>${COVERAGE_NOTE}</p>
      </div>
      <div class="sr-only" role="status" aria-live="polite" data-copy-status>{{ copyMessage }}</div>
    </div>

    <ng-template #cell let-c="c" let-col="col">
      <span *ngIf="c.reason === null">{{ c.text }}</span>
      <span *ngIf="c.reason !== null" [attr.data-na]="col" [attr.title]="c.reason">
        <span aria-hidden="true">—</span><span class="sr-only">{{ c.reason }}</span>
      </span>
    </ng-template>
  `
})
export class RecentTraceTableComponent {
  private readonly cdr = inject(ChangeDetectorRef);

  @Input() connectionStatus: 'connecting' | 'connected' | 'reconnecting' | 'disconnected' = 'connected';
  @Input() retryAttempt = 0;
  @Output() inspectPayload = new EventEmitter<RequestTraceData>();
  @Output() selectTrace = new EventEmitter<RequestTraceData>();

  public rows: Row[] = [];
  public legendOpen = false;
  public copyMessage = '';
  public failedCopyId: string | null = null;
  public readonly skeletonRows = [0, 1, 2];
  public readonly reasonLegend = NA_REASON_TEXTS;
  public readonly chipLegend = CHIP_LEGEND;
  public readonly coverageNoteEnabled = TRACE_COVERAGE_NOTE_ENABLED;
  public readonly proxyUrl = window.location.origin;
  private readonly warned = new Set<string>();
  private source: RequestTraceData[] = [];

  @Input() set traces(value: RequestTraceData[]) {
    this.source = value ?? [];
    this.rows = this.source.slice(0, MAX_ROWS).map(t => this.toRow(t));
  }
  get traces(): RequestTraceData[] {
    return this.source;
  }

  public get showSkeleton(): boolean {
    return this.connectionStatus === 'connecting' || this.connectionStatus === 'reconnecting';
  }

  public onInspect(trace: RequestTraceData): void {
    this.inspectPayload.emit(trace);
    this.selectTrace.emit(trace);
  }

  public trackByKey(_: number, row: Row): string {
    return row.key;
  }

  public chipMeaning(chip: string): string {
    return CHIP_LEGEND.find(c => c.chip === chip)?.meaning ?? chip;
  }

  public async copyId(row: Row): Promise<void> {
    this.copyMessage = '';
    this.failedCopyId = null;
    const ok = await copyText(row.trace.requestId);
    if (ok) {
      this.copyMessage = 'Copied request ID';
      this.cdr.detectChanges();
      return;
    }
    this.copyMessage = 'Copy failed - select the ID and copy it manually';
    this.failedCopyId = row.key;
    this.cdr.detectChanges();
    this.selectIdText(row.key);
  }

  public async copyProxyUrl(): Promise<void> {
    const ok = await copyText(this.proxyUrl);
    this.copyMessage = ok ? 'Copied URL' : 'Copy failed - select the URL and copy it manually';
    this.cdr.detectChanges();
  }

  private selectIdText(key: string): void {
    const rowEl = Array.from(document.querySelectorAll('[data-copy-id]')).find(b => b.getAttribute('data-copy-id') === key);
    const span = rowEl?.parentElement?.querySelector('[data-id-text]');
    if (!span) return;
    const range = document.createRange();
    range.selectNodeContents(span);
    const sel = window.getSelection();
    sel?.removeAllRanges();
    sel?.addRange(range);
  }

  private toRow(t: RequestTraceData): Row {
    const key = String(t.requestId ?? t.id ?? `${t.timestamp}-${t.provider}`);
    this.warnIfMalformed(t, key);
    const latency: Cell = isNum(t.timeToHeadersMs)
      ? { text: `${t.timeToHeadersMs}ms`, reason: null }
      : { text: '', reason: NOT_RECORDED };
    const fbReason = naReason(t, 'firstByte');
    const firstByte: Cell = { text: fbReason === null ? `${t.firstByteMs}ms` : '', reason: fbReason };
    const tokensBefore: Cell = isNum(t.tokensBefore)
      ? { text: `${fmt(t.tokensBefore)} est.`, reason: null }
      : { text: '', reason: NOT_RECORDED };
    const taReason = naReason(t, 'tokensAfter');
    const tokensAfter: Cell = { text: taReason === null ? this.tokensAfterText(t) : '', reason: taReason };
    const { text: statusText, glyph, cls } = this.statusParts(t);
    const naParts: string[] = [];
    if (firstByte.reason) naParts.push(`First byte: ${firstByte.reason}.`);
    if (tokensAfter.reason) naParts.push(`Tokens after: ${tokensAfter.reason}.`);
    return {
      trace: t,
      key,
      shortId: key.length > 8 ? key.substring(0, 8) : key,
      time: this.formatTime(t.timestamp),
      latency,
      firstByte,
      tokensBefore,
      tokensAfter,
      statusText,
      statusGlyph: glyph,
      statusClass: cls,
      statusTitle: t.stream ? 'status at response headers' : null,
      subLabel: statusSubLabel(t),
      chip: t.traceKind === 'cache_hit' ? 'cache' : t.traceKind === 'local' ? 'local' : t.statusCode === 499 ? 'cancelled' : null,
      naDescription: naParts.join(' '),
    };
  }

  private tokensAfterText(t: RequestTraceData): string {
    const after = t.tokensAfter as number;
    if (t.compressionStatus !== 'compressed') return `${fmt(after)} est.`;
    if (!isNum(t.tokensBefore) || t.tokensBefore <= 0 || after >= t.tokensBefore) return `${fmt(after)} est.`;
    const pct = Math.round(((t.tokensBefore - after) / t.tokensBefore) * 100);
    return `${fmt(t.tokensBefore)} -> ${fmt(after)} (-${pct}%)`;
  }

  private statusParts(t: RequestTraceData): { text: string; glyph: string; cls: string } {
    const code = t.statusCode;
    if (!isNum(code)) return { text: '—', glyph: '-', cls: 'bg-neutral-800 text-neutral-300 border-neutral-700' };
    if (t.traceKind === 'cache_hit' && code === 200) {
      return { text: '200 (cache)', glyph: '✓', cls: 'bg-emerald-950/80 text-emerald-400 border-emerald-800/60' };
    }
    const word = STATUS_WORDS[code] ?? (code >= 500 ? 'Server error' : code >= 400 ? 'Client error' : '');
    const text = word ? `${code} ${word}` : String(code);
    if (code === 499) return { text, glyph: '-', cls: 'bg-neutral-800 text-neutral-300 border-neutral-700' };
    if (code >= 200 && code < 300) return { text, glyph: '✓', cls: 'bg-emerald-950/80 text-emerald-400 border-emerald-800/60' };
    if (code === 429) return { text, glyph: '!', cls: 'bg-amber-950/80 text-amber-400 border-amber-800/60' };
    if (code >= 400 && code < 500) return { text, glyph: '!', cls: 'bg-yellow-950/80 text-yellow-400 border-yellow-800/60' };
    if (code >= 500) return { text, glyph: '✕', cls: 'bg-rose-950/80 text-rose-400 border-rose-800/60' };
    return { text, glyph: '-', cls: 'bg-neutral-800 text-neutral-300 border-neutral-700' };
  }

  private warnIfMalformed(t: RequestTraceData, key: string): void {
    if (this.warned.has(key)) return;
    if (isNum(t.timeToHeadersMs) && isNum(t.statusCode) && isNum(t.tokensBefore)) return;
    this.warned.add(key);
    console.warn(`Malformed RequestTrace event ${key}: rendering missing fields as n/a`);
  }

  public formatTime(timestamp: string): string {
    const d = new Date(timestamp);
    if (!timestamp || Number.isNaN(d.getTime())) return timestamp ?? '';
    return d.toLocaleTimeString([], { hour12: false, hour: '2-digit', minute: '2-digit', second: '2-digit' });
  }
}
