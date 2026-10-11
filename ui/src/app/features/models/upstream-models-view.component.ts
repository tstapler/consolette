import { Component, OnDestroy, OnInit, computed, inject, signal } from '@angular/core';
import { CommonModule } from '@angular/common';
import { RouterLink } from '@angular/router';
import { ModelCatalogStore } from '../../core/stores/model-catalog.store';
// Injected only so the SSE subscription (and ModelCatalogChanged dispatch) is live on this page.
import { MetricsStore } from '../../core/stores/metrics.store';
import { UpstreamCatalog } from '../../core/models/model-catalog.models';
import {
  catalogErrorLabel, catalogStateGlyph, catalogStateLabel, formatAge, upstreamDisplayState
} from '../../core/models/catalog-labels';
import { MODELS_HELP_TEXT } from './models-help-text';

const PAGE_SIZE = 200;
const COLLAPSE_ABOVE = 50;
const ANNOUNCE_DELAY_MS = 300;

@Component({
  selector: 'app-upstream-models-view',
  standalone: true,
  imports: [CommonModule, RouterLink],
  template: `
    <div class="space-y-4">
      <div class="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h1 class="text-2xl font-bold tracking-tight text-white">Models</h1>
          <p class="text-sm text-neutral-400 mt-1">What each upstream advertises, from the last refresh.</p>
        </div>
        <button
          type="button"
          data-refresh-all
          [attr.data-prominent]="allNeverRefreshed() ? '' : null"
          class="px-4 py-2 rounded-lg text-sm font-semibold border"
          [ngClass]="allNeverRefreshed() ? 'bg-cyan-500 text-slate-950 border-cyan-400' : 'bg-slate-900 text-slate-200 border-slate-700'"
          [disabled]="anyBusy()"
          [attr.aria-busy]="anyBusy() ? 'true' : null"
          (click)="store.refresh()"
        >{{ anyBusy() ? 'Refreshing' : 'Refresh now' }}</button>
      </div>

      <div data-help class="text-xs text-neutral-400 space-y-1 border border-neutral-800 rounded-lg p-3 bg-neutral-900/50">
        <p>{{ help.mode }}</p>
        <p>{{ help.credentialFlow }}</p>
        <p>{{ help.exposure }}</p>
      </div>

      <div class="sr-only" role="status" aria-live="polite" data-search-status>{{ announcement() }}</div>

      <div *ngIf="store.loadError() as err" class="border border-rose-800 bg-rose-950/40 rounded-lg p-4 flex flex-wrap items-center gap-3">
        <span>Could not load the model catalog ({{ err === 'offline' ? 'offline' : err }}).</span>
        <button type="button" data-load-retry class="px-3 py-1 rounded border border-slate-600 text-sm" (click)="store.load()">Retry</button>
      </div>

      <div *ngIf="store.isLoading() && !store.catalog()" data-skeleton role="status" class="space-y-2">
        <div class="h-16 rounded-lg bg-neutral-900 animate-pulse"></div>
        <span class="sr-only">Loading the model catalog</span>
      </div>

      <ng-container *ngIf="store.catalog() as cat">
        <div *ngIf="names().length === 0" class="text-sm text-neutral-300">
          No upstreams configured.
          <a routerLink="/config" class="text-cyan-400 underline">Open Config Editor</a>
        </div>

        <div *ngIf="names().length > 0" class="flex flex-wrap items-center gap-2">
          <input
            type="search"
            aria-label="Search models"
            placeholder="Search models"
            class="w-full sm:w-80 px-3 py-2 rounded-lg bg-neutral-900 border border-neutral-700 text-sm text-white"
            [value]="query()"
            (input)="onSearch($any($event.target).value)"
          />
        </div>

        <div *ngIf="query() && totalMatches() === 0 && names().length > 0" class="text-sm text-neutral-300 flex flex-wrap items-center gap-3">
          <span>No models match '{{ query() }}'.</span>
          <button type="button" class="px-3 py-1 rounded border border-slate-600" (click)="clearSearch()">Clear search</button>
        </div>

        <section
          *ngFor="let name of names(); trackBy: trackByName"
          [attr.data-upstream]="name"
          class="border border-neutral-800 rounded-xl bg-neutral-900 p-4"
          [hidden]="query() && matches(name).length === 0"
        >
          <div data-upstream-header class="flex flex-wrap items-center gap-x-3 gap-y-2">
            <h2 class="font-semibold text-white">{{ name }}</h2>
            <span class="px-2 py-0.5 text-xs font-semibold rounded-full border border-neutral-600 inline-flex items-center gap-1">
              <span aria-hidden="true">{{ glyph(cat.upstreams[name]) }}</span>{{ stateLabel(cat.upstreams[name]) }}
            </span>
            <span *ngIf="cat.upstreams[name].totalCount > 0" class="text-sm text-neutral-300">{{ cat.upstreams[name].totalCount }} models</span>
            <span *ngIf="cat.upstreams[name].refreshedAt as at" class="text-sm text-neutral-400">
              refreshed <time [attr.datetime]="at">{{ age(at) }}</time>
            </span>
            <span *ngIf="cat.upstreams[name].error as e" class="text-sm text-rose-300" [attr.title]="e.message">{{ errorLabel(e) }}</span>
            <span *ngIf="cat.upstreams[name].refreshing" data-spinner class="text-xs text-cyan-300 animate-pulse">Refreshing...</span>
            <span *ngIf="store.notice(name) as n" class="text-xs text-amber-300">{{ n }}</span>
            <button
              type="button"
              data-refresh
              class="ml-auto px-3 py-1 rounded border border-slate-600 text-sm min-h-6"
              [attr.aria-label]="(cat.upstreams[name].state === 'error' ? 'Retry ' : 'Refresh ') + name"
              [disabled]="store.isBusy(name)"
              [attr.aria-busy]="store.isBusy(name) ? 'true' : null"
              (click)="store.refresh(name)"
            >{{ store.isBusy(name) ? 'Refreshing' : (cat.upstreams[name].state === 'error' ? 'Retry' : 'Refresh') }}</button>
          </div>

          <p *ngIf="display(cat.upstreams[name]) === 'not_loaded'" class="mt-2 text-sm text-neutral-300">Not loaded yet - press Refresh now</p>
          <p *ngIf="cat.upstreams[name].state === 'ok' && cat.upstreams[name].totalCount === 0" class="mt-2 text-sm text-neutral-300">Upstream reports 0 models.</p>

          <div *ngIf="cat.upstreams[name].models.length > 0" class="mt-2">
            <button
              type="button"
              data-toggle
              class="text-sm text-cyan-400 underline min-h-6"
              [attr.aria-expanded]="isExpanded(name, cat.upstreams[name]) ? 'true' : 'false'"
              (click)="toggle(name, cat.upstreams[name])"
            >{{ isExpanded(name, cat.upstreams[name]) ? 'Hide models' : 'Show models' }}</button>
            <ul *ngIf="isExpanded(name, cat.upstreams[name])" data-model-list class="mt-2 grid grid-cols-1 md:grid-cols-2 gap-x-4 gap-y-1 text-sm font-mono text-neutral-200">
              <li *ngFor="let m of visible(name); trackBy: trackById" data-model-id class="break-all">{{ m.id }}</li>
            </ul>
            <button
              *ngIf="isExpanded(name, cat.upstreams[name]) && matches(name).length > visible(name).length"
              type="button"
              data-show-more
              class="mt-2 px-3 py-1 rounded border border-slate-600 text-sm"
              (click)="showMore(name)"
            >Show more ({{ matches(name).length - visible(name).length }} remaining)</button>
            <p *ngIf="cat.upstreams[name].truncated" class="mt-2 text-xs text-neutral-400">
              Showing the first {{ cat.upstreams[name].models.length }} of {{ cat.upstreams[name].totalCount }} models.
            </p>
          </div>
        </section>

        <section data-config-warnings class="border border-neutral-800 rounded-xl bg-neutral-900 p-4 space-y-2">
          <h2 class="font-semibold text-white">
            <span aria-hidden="true">{{ cat.configWarnings.length > 0 ? '!' : '✓' }}</span>
            Referenced by config but not listed upstream ({{ cat.configWarnings.length }})
          </h2>
          <ul *ngIf="cat.configWarnings.length > 0" class="text-sm font-mono space-y-1">
            <li *ngFor="let w of cat.configWarnings">{{ w.upstream }}: {{ w.model }} ({{ w.referencedBy }})</li>
          </ul>
          <p class="text-xs text-neutral-400">{{ caveat }}</p>
        </section>
      </ng-container>
    </div>
  `
})
export class UpstreamModelsViewComponent implements OnInit, OnDestroy {
  protected readonly store = inject(ModelCatalogStore);
  protected readonly help = MODELS_HELP_TEXT;
  protected readonly caveat =
    'Only models pinned on the first route are checked. Models reached through other routes (ignored by the router, which warns and uses the first), model-family routes and fallback targets that live outside this route are not checked, so no warning here is not proof that every target exists.';

  private readonly metricsStore = inject(MetricsStore);

  protected readonly query = signal('');
  protected readonly announcement = signal('');
  private readonly expandedOverride = signal<Readonly<Record<string, boolean>>>({});
  private readonly limits = signal<Readonly<Record<string, number>>>({});
  private announceTimer: ReturnType<typeof setTimeout> | null = null;

  protected readonly names = computed(() => Object.keys(this.store.catalog()?.upstreams ?? {}));
  protected readonly anyBusy = computed(() => this.names().some((n) => this.store.isBusy(n)));
  protected readonly allNeverRefreshed = computed(() => {
    const cat = this.store.catalog();
    const names = this.names();
    return names.length > 0 && names.every((n) => upstreamDisplayState(cat!.upstreams[n]) === 'not_loaded');
  });
  protected readonly totalMatches = computed(() =>
    this.names().reduce((sum, n) => sum + this.matches(n).length, 0));

  ngOnInit(): void {
    this.store.load();
  }

  ngOnDestroy(): void {
    if (this.announceTimer !== null) {
      clearTimeout(this.announceTimer);
    }
  }

  protected display(entry: UpstreamCatalog) { return upstreamDisplayState(entry); }
  protected stateLabel(entry: UpstreamCatalog): string { return catalogStateLabel(upstreamDisplayState(entry)); }
  protected glyph(entry: UpstreamCatalog): string { return catalogStateGlyph(upstreamDisplayState(entry)); }
  protected errorLabel = catalogErrorLabel;
  protected age(iso: string): string { return formatAge(iso, Date.now()); }
  protected trackByName(_: number, name: string): string { return name; }
  protected trackById(_: number, m: { id: string }): string { return m.id; }

  protected matches(name: string) {
    const models = this.store.catalog()?.upstreams[name]?.models ?? [];
    const q = this.query().trim().toLowerCase();
    return q ? models.filter((m) => m.id.toLowerCase().includes(q)) : models;
  }

  protected visible(name: string) {
    return this.matches(name).slice(0, this.limits()[name] ?? PAGE_SIZE);
  }

  protected showMore(name: string): void {
    this.limits.update((l) => ({ ...l, [name]: (l[name] ?? PAGE_SIZE) + PAGE_SIZE }));
  }

  protected isExpanded(name: string, entry: UpstreamCatalog): boolean {
    if (this.query().trim()) {
      return this.matches(name).length > 0;
    }
    return this.expandedOverride()[name] ?? entry.models.length <= COLLAPSE_ABOVE;
  }

  protected toggle(name: string, entry: UpstreamCatalog): void {
    const next = !this.isExpanded(name, entry);
    this.expandedOverride.update((o) => ({ ...o, [name]: next }));
  }

  protected onSearch(value: string): void {
    this.query.set(value);
    if (this.announceTimer !== null) {
      clearTimeout(this.announceTimer);
    }
    this.announceTimer = setTimeout(() => {
      this.announceTimer = null;
      this.announcement.set(value.trim() ? `${this.totalMatches()} matches` : '');
    }, ANNOUNCE_DELAY_MS);
  }

  protected clearSearch(): void {
    this.query.set('');
    this.announcement.set('');
  }
}
