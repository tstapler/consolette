import { ChangeDetectorRef, Component, DestroyRef, ElementRef, HostListener, Input, OnInit, ViewChild, inject } from '@angular/core';
import { CommonModule } from '@angular/common';
import { ActivatedRoute, Router } from '@angular/router';
import { takeUntilDestroyed } from '@angular/core/rxjs-interop';
import { AttemptTraceData, RequestTraceData } from '../../../core/models/telemetry.models';
import { naReason } from '../../../core/models/na-reason';
import { chipMeaning, errorLabel, fmt, isFailed, isNum, modelSourceText, routeChip, statusParts, statusSubLabel, tokensAfterText, whyText } from '../../../core/models/trace-labels';
import { copyText } from '../../../core/util/copy-text';

const SHEET_MAX_WIDTH_PX = 640;
const MIN_TARGET = 'min-width:24px;min-height:24px';
const EVICTED_BANNER = 'No longer in live buffer - showing the last data received';
const STATUS_CAVEAT =
  'Status shown is the upstream-class status; the client may have received a different code (for example 529 vs 503 depending on the endpoint).';

interface AttemptItem {
  index: number;
  line: string;
  outcome: string;
  failed: boolean;
  barLeft: number;
  barWidth: number;
  attempt: AttemptTraceData;
  target: string;
}

interface Round { heading: string | null; items: AttemptItem[]; }

interface DrawerView {
  trace: RequestTraceData;
  id8: string;
  time: string;
  status: { text: string; glyph: string; cls: string };
  subLabel: string | null;
  total: string | null;
  firstByte: string | null;
  requested: string | null;
  routed: string;
  routedLabel: string;
  noUpstream: boolean;
  modelSource: string | null;
  why: string;
  chip: string | null;
  rounds: Round[];
  items: AttemptItem[];
  summary: string;
  attemptsNote: string | null;
  truncated: boolean;
  notRecorded: string[];
  tokensBefore: string;
  tokensAfter: string;
  streamNote: boolean;
}

const target = (a: AttemptTraceData): string => (a.model ? `${a.upstream} / ${a.model}` : a.upstream);

function glyph(a: AttemptTraceData): string {
  if (a.outcome === 'ok') return 'ok';
  if (a.outcome === 'shed' || a.outcome === 'cancelled') return '-';
  return 'x';
}

function summarize(attempts: AttemptTraceData[]): string {
  const n = attempts.length;
  if (n === 0) return '';
  const plural = n === 1 ? '1 attempt' : `${n} attempts`;
  const failedCount = attempts.filter(isFailed).length;
  if (failedCount === n) return `${plural}; ${n === 1 ? 'it' : 'all'} failed`;
  if (failedCount === 0) return n === 1 ? `${plural}; it succeeded` : `${plural}; all succeeded`;
  const last = attempts[n - 1];
  const leading = attempts.findIndex(a => !isFailed(a));
  const first = leading === 1 ? 'first failed' : leading === 2 ? 'first two failed' : `first ${leading} failed`;
  if (!isFailed(last) && leading === n - 1) {
    const ordinal = n === 2 ? 'second' : n === 3 ? 'third' : `${n}th`;
    return `${plural}; ${first}; ${ordinal} succeeded`;
  }
  return `${plural}; ${failedCount} failed, ${n - failedCount} succeeded`;
}

function buildItems(attempts: AttemptTraceData[]): AttemptItem[] {
  const total = attempts.reduce((sum, a) => sum + (isNum(a.latencyMs) ? a.latencyMs : 0), 0);
  let offset = 0;
  return attempts.map((a, index) => {
    const ms = isNum(a.latencyMs) ? a.latencyMs : 0;
    const left = total > 0 ? (offset / total) * 100 : 0;
    const width = total > 0 ? Math.max((ms / total) * 100, 1) : 0;
    offset += ms;
    const failed = isFailed(a);
    const outcome = [glyph(a), isNum(a.status) ? String(a.status) : '', failed ? errorLabel(a) : ''].filter(Boolean).join(' ');
    return {
      index,
      attempt: a,
      failed,
      target: target(a),
      outcome,
      line: `${index + 1} ${target(a)}  ${outcome}  ${isNum(a.latencyMs) ? `${a.latencyMs}ms` : ''}`.trim(),
      barLeft: left,
      barWidth: width,
    };
  });
}

function groupRounds(items: AttemptItem[], iterations: number): Round[] {
  const distinct = new Set(items.map(i => i.attempt.iteration ?? 0));
  if (iterations <= 1 && distinct.size <= 1) return [{ heading: null, items }];
  return Array.from(distinct).sort((a, b) => a - b).map(it => ({
    heading: `Round ${it + 1}`,
    items: items.filter(i => (i.attempt.iteration ?? 0) === it),
  }));
}

@Component({
  selector: 'app-inspect-drawer',
  standalone: true,
  imports: [CommonModule],
  template: `
    <aside *ngIf="selectedId" role="dialog" [attr.aria-modal]="narrow ? 'true' : 'false'" [attr.aria-labelledby]="headingId"
      [attr.data-mode]="narrow ? 'sheet' : 'panel'" (keydown)="onKeydown($event)"
      class="fixed top-16 bottom-0 z-40 bg-neutral-900 border-l border-neutral-700 text-xs text-neutral-300 shadow-2xl overflow-y-auto overflow-x-hidden p-5 space-y-4 break-words"
      [ngClass]="narrow ? 'inset-x-0 w-full' : 'right-0 w-[560px] max-w-full'">
      <div class="flex items-start justify-between gap-2">
        <h2 #heading data-drawer-heading [id]="headingId" tabindex="-1" class="text-sm font-semibold text-white focus:outline focus:outline-2 min-w-0"
          [title]="selectedId">Trace {{ idLabel }}</h2>
        <div class="flex gap-2 shrink-0">
          <button type="button" data-drawer-copy (click)="copyId()" style="${MIN_TARGET}" aria-label="Copy request ID"
            class="px-2 py-1 text-[11px] bg-neutral-800 hover:bg-neutral-700 rounded border border-neutral-700 focus-visible:outline focus-visible:outline-2">Copy</button>
          <button type="button" data-drawer-close (click)="close()" style="${MIN_TARGET}" aria-label="Close trace drawer"
            class="px-2 py-1 text-[11px] bg-neutral-800 hover:bg-neutral-700 rounded border border-neutral-700 focus-visible:outline focus-visible:outline-2">Close</button>
        </div>
      </div>

      <div data-drawer-status role="status" aria-live="polite" [class.sr-only]="!evicted"
        class="text-amber-400 border border-amber-800/60 rounded px-3 py-2">{{ statusMessage }}</div>

      <p *ngIf="!view" class="text-neutral-200">This trace is no longer retained (buffer holds last 100).</p>

      <ng-container *ngIf="view as v">
        <div class="flex flex-wrap items-center gap-x-4 gap-y-1">
          <span class="px-2 py-0.5 text-[11px] font-semibold font-mono rounded-full border" [ngClass]="v.status.cls">
            <span aria-hidden="true">{{ v.status.glyph }}</span> {{ v.status.text }}</span>
          <span *ngIf="v.subLabel">{{ v.subLabel }}</span>
          <span class="font-mono">{{ v.time }}</span>
          <span *ngIf="v.total" class="font-mono">Total {{ v.total }}</span>
          <span *ngIf="v.firstByte" class="font-mono">First byte {{ v.firstByte }}</span>
        </div>
        <p data-status-note class="text-neutral-400">${STATUS_CAVEAT}<ng-container *ngIf="v.streamNote"> Streamed: status at response headers.</ng-container></p>

        <section aria-labelledby="route-h" class="space-y-1">
          <h3 id="route-h" class="uppercase tracking-wider text-[11px] text-neutral-400">Route</h3>
          <p *ngIf="v.requested" data-requested>Requested <span class="font-mono text-neutral-100">{{ v.requested }}</span></p>
          <p data-routed>{{ v.routedLabel }} <span class="font-mono text-neutral-100">{{ v.routed }}</span><ng-container *ngIf="v.noUpstream"> (cache hit; no upstream ran)</ng-container></p>
          <p *ngIf="v.modelSource" data-model-source>{{ v.modelSource }}</p>
          <p data-why>Why
            <span *ngIf="v.chip" class="px-1.5 py-0.5 text-[11px] bg-neutral-800 rounded border border-neutral-700" [title]="chipMeaning(v.chip)">{{ v.chip }}</span>
            {{ v.why }}</p>
        </section>

        <section aria-labelledby="attempts-h" class="space-y-2">
          <h3 #attemptsHeading data-attempts-heading id="attempts-h" tabindex="-1" class="uppercase tracking-wider text-[11px] text-neutral-400">Attempts ({{ v.items.length }})</h3>
          <p *ngIf="v.attemptsNote" data-attempts-note>{{ v.attemptsNote }}</p>
          <p *ngIf="v.truncated" data-truncated>showing first 16 attempts</p>
          <p data-attempt-summary class="sr-only">{{ v.summary }}</p>
          <ng-container *ngFor="let round of v.rounds">
            <h4 *ngIf="round.heading" data-round class="text-[11px] text-neutral-400">{{ round.heading }}</h4>
            <ol data-attempt-list class="space-y-1">
              <li *ngFor="let it of round.items">
                <button type="button" data-attempt [attr.aria-pressed]="it.index === selectedAttempt" (click)="selectAttempt(it.index)" style="${MIN_TARGET}"
                  class="w-full text-left px-2 py-1 rounded border font-mono focus-visible:outline focus-visible:outline-2"
                  [ngClass]="it.index === selectedAttempt ? 'border-cyan-600 bg-neutral-800' : 'border-neutral-800 hover:bg-neutral-800/60'">
                  <span class="block break-words">{{ it.line }}</span>
                  <span class="block h-1.5 mt-1 relative bg-neutral-800 rounded" aria-hidden="true" data-bar-track>
                    <span data-bar aria-hidden="true" class="absolute top-0 bottom-0 rounded border" [ngClass]="it.failed ? 'bg-rose-500 border-rose-300' : 'bg-emerald-500 border-emerald-300'"
                      [style.left.%]="it.barLeft" [style.width.%]="it.barWidth"></span>
                  </span>
                </button>
              </li>
            </ol>
          </ng-container>
          <div *ngIf="selected as s" data-attempt-detail aria-live="off" class="border border-neutral-800 rounded p-2 space-y-0.5">
            <p>Attempt {{ s.index + 1 }} of {{ v.items.length }}</p>
            <p class="font-mono">{{ s.target }}</p>
            <p *ngIf="s.attempt.status !== null">Status <span class="font-mono">{{ s.attempt.status }}</span></p>
            <p>Latency <span class="font-mono">{{ s.attempt.latencyMs }}ms</span></p>
            <p *ngIf="s.failed" data-attempt-error>Error {{ errorLabel(s.attempt) }}<ng-container *ngIf="s.attempt.errorMessage"> - {{ s.attempt.errorMessage }}</ng-container></p>
          </div>
        </section>

        <section aria-labelledby="tokens-h" data-tokens class="space-y-1">
          <h3 id="tokens-h" class="uppercase tracking-wider text-[11px] text-neutral-400">Tokens</h3>
          <p>before <span class="font-mono">{{ v.tokensBefore }}</span> &nbsp; after <span class="font-mono">{{ v.tokensAfter }}</span></p>
        </section>

        <section *ngIf="v.notRecorded.length" data-not-recorded aria-labelledby="nr-h" class="space-y-1">
          <h3 id="nr-h" class="uppercase tracking-wider text-[11px] text-neutral-400">Not recorded</h3>
          <p *ngFor="let line of v.notRecorded">{{ line }}</p>
        </section>
      </ng-container>
    </aside>
  `
})
export class InspectDrawerComponent implements OnInit {
  private readonly cdr = inject(ChangeDetectorRef);
  private readonly host = inject<ElementRef<HTMLElement>>(ElementRef);
  private readonly router = inject(Router, { optional: true });
  private readonly route = inject(ActivatedRoute, { optional: true });
  private readonly destroyRef = inject(DestroyRef);

  @ViewChild('heading') private headingEl?: ElementRef<HTMLElement>;
  @ViewChild('attemptsHeading') private attemptsHeadingEl?: ElementRef<HTMLElement>;

  public selectedId: string | null = null;
  public selectedAttempt = 0;
  public narrow = false;
  public evicted = false;
  public view: DrawerView | null = null;
  public readonly headingId = 'inspect-drawer-heading';
  public readonly chipMeaning = chipMeaning;
  public readonly errorLabel = errorLabel;
  private announcement = '';
  private snapshot: RequestTraceData | null = null;
  private source: RequestTraceData[] = [];
  private origin: HTMLElement | null = null;
  private inerted: HTMLElement[] = [];

  @Input() set traces(value: RequestTraceData[]) {
    this.source = value ?? [];
    this.refreshSnapshot();
  }

  public get idLabel(): string {
    return (this.selectedId ?? '').substring(0, 8);
  }

  public get statusMessage(): string {
    return this.evicted ? EVICTED_BANNER : this.announcement;
  }

  public get selected(): AttemptItem | null {
    return this.view?.items[this.selectedAttempt] ?? null;
  }

  public ngOnInit(): void {
    this.route?.queryParamMap.pipe(takeUntilDestroyed(this.destroyRef)).subscribe(params => {
      const id = params.get('requestId');
      if (id && id !== this.selectedId) this.selectById(id);
      else if (!id && this.selectedId) this.close(false);
    });
  }

  /** Open from a table row; `origin` regains focus when the drawer closes. */
  public open(trace: RequestTraceData, origin: HTMLElement | null, scrollToAttempts: boolean): void {
    const id = this.idOf(trace);
    const swapping = this.selectedId !== null && this.selectedId !== id;
    this.selectedId = id;
    this.snapshot = trace;
    this.evicted = false;
    this.origin = origin;
    this.announcement = swapping ? `Showing trace ${id.substring(0, 8)}` : '';
    this.rebuild(true);
    this.syncUrl(id);
    this.present(scrollToAttempts);
  }

  /** Open from a deep link: no originating button. */
  public selectById(id: string): void {
    this.selectedId = id;
    this.snapshot = this.source.find(t => this.idOf(t) === id) ?? null;
    this.evicted = false;
    this.origin = null;
    this.announcement = '';
    this.rebuild(true);
    this.present(false);
  }

  public close(restoreFocus = true): void {
    if (!this.selectedId) return;
    this.selectedId = null;
    this.snapshot = null;
    this.view = null;
    this.evicted = false;
    this.refreshLayout();
    this.syncUrl(null);
    this.cdr.detectChanges();
    if (restoreFocus) this.restoreFocus();
  }

  public selectAttempt(index: number): void {
    this.selectedAttempt = index;
  }

  public async copyId(): Promise<void> {
    if (!this.selectedId) return;
    const ok = await copyText(this.selectedId);
    this.announcement = ok ? 'Copied request ID' : 'Copy failed - select the ID and copy it manually';
    this.cdr.detectChanges();
  }

  @HostListener('window:resize')
  public refreshLayout(): void {
    this.narrow = !!this.selectedId && window.innerWidth <= SHEET_MAX_WIDTH_PX;
    this.narrow ? this.applyInert() : this.clearInert();
  }

  public onKeydown(event: KeyboardEvent): void {
    if (event.key === 'Escape') {
      event.preventDefault();
      this.close();
    } else if (event.key === 'Tab' && this.narrow) {
      this.cycleFocus(event);
    }
  }

  private cycleFocus(event: KeyboardEvent): void {
    const dialog = this.host.nativeElement.querySelector('[role="dialog"]');
    const controls = Array.from(dialog?.querySelectorAll<HTMLElement>('button:not([disabled])') ?? []);
    if (controls.length === 0) return;
    const first = controls[0];
    const last = controls[controls.length - 1];
    const active = document.activeElement;
    if (event.shiftKey && (active === first || active === this.headingEl?.nativeElement)) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && active === last) {
      event.preventDefault();
      first.focus();
    }
  }

  private present(scrollToAttempts: boolean): void {
    this.refreshLayout();
    this.cdr.detectChanges();
    this.headingEl?.nativeElement.focus();
    if (scrollToAttempts) this.attemptsHeadingEl?.nativeElement.scrollIntoView({ block: 'start' });
  }

  private restoreFocus(): void {
    const origin = this.origin;
    this.origin = null;
    if (origin?.isConnected) {
      origin.focus();
      return;
    }
    document.querySelector<HTMLElement>('table[data-trace-table]')?.focus();
  }

  private syncUrl(id: string | null): void {
    void this.router?.navigate([], {
      relativeTo: this.route ?? undefined,
      queryParams: { requestId: id },
      queryParamsHandling: 'merge',
      replaceUrl: true,
    }).catch(() => undefined);
  }

  private idOf(t: RequestTraceData): string {
    return String(t.requestId ?? t.id);
  }

  private refreshSnapshot(): void {
    if (!this.selectedId) return;
    const found = this.source.find(t => this.idOf(t) === this.selectedId);
    if (found) {
      this.snapshot = found;
      this.evicted = false;
    } else {
      this.evicted = this.snapshot !== null;
    }
    this.rebuild(false);
  }

  private rebuild(resetSelection: boolean): void {
    const t = this.snapshot;
    this.view = t ? this.toView(t) : null;
    if (!this.view) return;
    const items = this.view.items;
    if (resetSelection || this.selectedAttempt >= items.length) {
      const firstFailed = items.findIndex(i => i.failed);
      this.selectedAttempt = firstFailed >= 0 ? firstFailed : 0;
    }
  }

  private toView(t: RequestTraceData): DrawerView {
    const attempts = t.attempts ?? [];
    const items = buildItems(attempts);
    const fb = naReason(t, 'firstByte');
    const ta = naReason(t, 'tokensAfter');
    const notRecorded: string[] = [];
    if (fb) notRecorded.push(`First byte: ${fb}`);
    if (ta) notRecorded.push(`Tokens after: ${ta}`);
    const requested = t.requestedModel ?? null;
    const routedModel = t.routedModel ?? t.model;
    const provider = t.routedProvider ?? t.provider;
    const noUpstream = t.traceKind === 'cache_hit';
    return {
      trace: t,
      id8: this.idOf(t).substring(0, 8),
      time: this.formatTime(t.timestamp),
      status: statusParts(t),
      subLabel: statusSubLabel(t),
      total: isNum(t.totalMs) ? `${t.totalMs}ms` : null,
      firstByte: fb === null && isNum(t.firstByteMs) ? `${t.firstByteMs}ms` : null,
      requested,
      routed: noUpstream ? routedModel : `${provider} / ${routedModel}`,
      routedLabel: noUpstream ? 'Answered for' : 'Routed',
      noUpstream,
      modelSource: modelSourceText(t),
      why: whyText(t),
      chip: routeChip(t),
      rounds: groupRounds(items, t.iterations ?? 1),
      items,
      summary: summarize(attempts),
      attemptsNote: attempts.length > 0 ? null : noUpstream ? 'Served from the response cache; no upstream attempt' : 'Attempt detail not captured',
      truncated: t.attemptsTruncated === true,
      notRecorded,
      tokensBefore: isNum(t.tokensBefore) ? `${fmt(t.tokensBefore)} est.` : 'n/a (not recorded)',
      tokensAfter: ta ?? tokensAfterText(t),
      streamNote: t.stream === true,
    };
  }

  private formatTime(timestamp: string): string {
    const d = new Date(timestamp);
    if (!timestamp || Number.isNaN(d.getTime())) return timestamp ?? '';
    return d.toLocaleTimeString([], { hour12: false, hour: '2-digit', minute: '2-digit', second: '2-digit' });
  }

  private applyInert(): void {
    this.clearInert();
    let node: HTMLElement | null = this.host.nativeElement;
    while (node && node !== document.body) {
      const parent: HTMLElement | null = node.parentElement;
      for (const sibling of Array.from(parent?.children ?? [])) {
        if (sibling !== node && sibling instanceof HTMLElement && !sibling.hasAttribute('inert')) {
          sibling.setAttribute('inert', '');
          this.inerted.push(sibling);
        }
      }
      node = parent;
    }
  }

  private clearInert(): void {
    this.inerted.forEach(n => n.removeAttribute('inert'));
    this.inerted = [];
  }
}
