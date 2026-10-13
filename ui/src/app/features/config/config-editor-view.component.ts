import { Component, OnInit, inject, signal, effect } from '@angular/core';
import { CommonModule } from '@angular/common';
import { ConfigStore } from '../../core/stores/config.store';
import { ConfigData } from '../../core/models/telemetry.models';
import { RoutingConfigComponent, ProviderConfigItem } from './components/routing-config.component';
import { FallbackCascadeBuilderComponent } from './components/fallback-cascade-builder.component';
import { RateLimitConfigComponent, RateLimitValues } from './components/rate-limit-config.component';
import { ConfigDiffModalComponent } from './components/config-diff-modal.component';

@Component({
  selector: 'app-config-editor-view',
  standalone: true,
  imports: [
    CommonModule,
    RoutingConfigComponent,
    FallbackCascadeBuilderComponent,
    RateLimitConfigComponent,
    ConfigDiffModalComponent
  ],
  template: `
    <div class="space-y-6">
      <!-- Header Bar -->
      <div class="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 class="text-2xl font-bold tracking-tight text-white">Dynamic Proxy Configuration</h1>
          <p class="text-xs text-slate-400">Manage upstreams, weights, failover cascades, and rate limit policies with zero-downtime hot reloading.</p>
        </div>

        <div class="flex flex-wrap items-center gap-3">
          <button
            type="button"
            (click)="openDiffModal()"
            class="px-4 py-2 text-xs font-medium text-slate-200 bg-slate-800 hover:bg-slate-700 rounded-lg border border-slate-700 transition"
            data-testid="preview-diff-btn"
          >
            Preview Diff
          </button>
          <button
            type="button"
            (click)="onApplyConfig()"
            [disabled]="configStore.isSaving()"
            class="px-4 py-2 text-xs font-medium text-white bg-cyan-700 hover:bg-cyan-800 disabled:opacity-50 rounded-lg shadow-lg shadow-cyan-500/20 transition flex items-center space-x-2"
            data-testid="apply-config-btn"
          >
            @if (configStore.isSaving()) {
              <span class="motion-safe:animate-spin text-xs">⏳</span>
              <span>Applying...</span>
            } @else {
              <span>Apply Config (Hot Reload)</span>
            }
          </button>
        </div>
      </div>

      <!-- Success Toast / Notification Banner -->
      @if (showToast()) {
        <div class="bg-emerald-500/10 border border-emerald-500/30 text-emerald-400 px-4 py-3 rounded-xl text-xs flex items-center justify-between" data-testid="success-toast">
          <span>✓ Configuration successfully applied and hot-reloaded!</span>
          <button type="button" (click)="showToast.set(false)" class="text-emerald-400 hover:text-white">✕</button>
        </div>
      }

      <!-- Error Banner -->
      @if (configStore.error()) {
        <div class="bg-rose-500/10 border border-rose-500/30 text-rose-400 px-4 py-3 rounded-xl text-xs flex items-center justify-between" data-testid="error-banner">
          <span>⚠️ {{ configStore.error() }}</span>
        </div>
      }

      <!-- Routing & Load Balancing -->
      <app-routing-config
        [providers]="providerItems()"
        (providersChange)="onProvidersChange($event)"
      />

      <!-- Fallback Cascade -->
      <app-fallback-cascade-builder
        [cascade]="cascadeItems()"
        (cascadeChange)="onCascadeChange($event)"
      />

      <!-- Rate Limits -->
      <app-rate-limit-config
        [rateLimits]="draftRateLimits()"
      />

      <!-- Diff Preview Modal -->
      <app-config-diff-modal
        [isOpen]="isDiffModalOpen()"
        [originalConfig]="configStore.config()"
        [draftConfig]="currentDraft()"
        (close)="isDiffModalOpen.set(false)"
        (confirm)="onConfirmApply()"
      />
    </div>
  `
})
export class ConfigEditorViewComponent implements OnInit {
  public readonly configStore = inject(ConfigStore);

  public isDiffModalOpen = signal<boolean>(false);
  public showToast = signal<boolean>(false);

  public providerItems = signal<ProviderConfigItem[]>([]);
  public cascadeItems = signal<string[]>([]);
  public draftRateLimits = signal<RateLimitValues>({});

  constructor() {
    effect(() => {
      const active = this.configStore.config();
      if (active) {
        this.syncDraftFromActive(active);
      }
    });

    effect(() => {
      const applied = this.configStore.lastApplied();
      if (applied) {
        this.showToast.set(true);
        setTimeout(() => this.showToast.set(false), 4000);
      }
    });
  }

  public ngOnInit(): void {
    this.configStore.loadConfig();
  }

  private syncDraftFromActive(active: ConfigData): void {
    if (active.providers) {
      const items: ProviderConfigItem[] = Object.entries(active.providers).map(([name, p]) => ({
        name,
        apiKey: p.apiKey,
        baseUrl: p.baseUrl,
        weight: p.weight
      }));
      this.providerItems.set(items);
    } else {
      this.providerItems.set([]);
    }

    this.cascadeItems.set(active.fallbackCascade ?? []);

    if (active.rateLimits) {
      this.draftRateLimits.set(active.rateLimits);
    }
  }

  public currentDraft(): ConfigData {
    const providersMap: Record<string, { apiKey?: string; baseUrl?: string; weight?: number }> = {};
    for (const p of this.providerItems()) {
      providersMap[p.name] = {
        apiKey: p.newApiKey ?? p.apiKey,
        baseUrl: p.baseUrl,
        weight: p.weight
      };
    }

    return {
      webUi: this.configStore.config()?.webUi || 'angular',
      providers: providersMap,
      fallbackCascade: this.cascadeItems(),
      rateLimits: this.draftRateLimits()
    };
  }

  public onProvidersChange(newProviders: ProviderConfigItem[]): void {
    this.providerItems.set(newProviders);
  }

  public onCascadeChange(newCascade: string[]): void {
    this.cascadeItems.set(newCascade);
  }

  public openDiffModal(): void {
    this.isDiffModalOpen.set(true);
  }

  /** The PUT body: only fields the operator changed. Rate limits are read-only and never sent. */
  public changesPayload(): ConfigData {
    const active = this.configStore.config()?.providers ?? {};
    const providers: NonNullable<ConfigData['providers']> = {};
    for (const p of this.providerItems()) {
      const original = active[p.name];
      const entry: { apiKey?: string; baseUrl?: string; weight?: number } = {};
      if (p.newApiKey) entry.apiKey = p.newApiKey;
      if (p.baseUrl && p.baseUrl !== original?.baseUrl) entry.baseUrl = p.baseUrl;
      if (p.weight !== undefined && p.weight !== original?.weight) entry.weight = p.weight;
      providers[p.name] = entry;
    }
    const payload: ConfigData = {
      webUi: this.configStore.config()?.webUi || 'angular',
      providers
    };
    if (this.cascadeItems().length > 0) payload.fallbackCascade = this.cascadeItems();
    return payload;
  }

  public onApplyConfig(): void {
    this.configStore.applyConfig(this.changesPayload());
  }

  public onConfirmApply(): void {
    this.isDiffModalOpen.set(false);
    this.onApplyConfig();
  }
}
