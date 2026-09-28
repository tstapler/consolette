import { Component, Input, OnChanges, OnDestroy, SimpleChanges } from '@angular/core';
import { CommonModule } from '@angular/common';

export interface ProviderHealthItem {
  name: string;
  status: 'Healthy' | 'Degraded' | 'Cooldown' | 'AuthError' | 'Active' | string;
  cooldownSeconds?: number;
  lastError?: string;
}

@Component({
  selector: 'app-provider-health-grid',
  standalone: true,
  imports: [CommonModule],
  template: `
    <div class="bg-neutral-900 border border-neutral-800 rounded-xl p-5">
      <div class="flex items-center justify-between mb-4">
        <h3 class="text-sm font-semibold text-white tracking-wide uppercase">Provider Health</h3>
        <span class="text-xs text-neutral-400">{{ providers.length }} Upstream Providers</span>
      </div>

      <div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
        <div
          *ngFor="let provider of providers; trackBy: trackByName"
          class="bg-neutral-950/70 border rounded-lg p-4 transition-all duration-300 relative overflow-hidden"
          [ngClass]="getCardBorderClass(provider.status)"
        >
          <div class="flex items-center justify-between mb-2">
            <span class="font-medium text-white text-sm">{{ provider.name }}</span>
            <span
              class="px-2.5 py-0.5 text-xs font-semibold rounded-full flex items-center gap-1.5"
              [ngClass]="getBadgeClass(provider.status)"
            >
              <span class="w-1.5 h-1.5 rounded-full" [ngClass]="getDotClass(provider.status)"></span>
              {{ getStatusLabel(provider.status) }}
            </span>
          </div>

          <div *ngIf="isCooldown(provider.status)" class="mt-2 text-xs font-mono text-amber-400 flex items-center gap-1">
            <svg class="w-3.5 h-3.5 animate-spin" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 8v4l3 3m6-3a9 9 0 11-18 0 9 9 0 0118 0z" />
            </svg>
            <span>Cooldown: {{ getCooldownRemaining(provider) }}s remaining</span>
          </div>

          <div *ngIf="provider.lastError" class="mt-2 text-xs text-rose-400 font-mono truncate" [title]="provider.lastError">
            {{ provider.lastError }}
          </div>
        </div>
      </div>
    </div>
  `
})
export class ProviderHealthGridComponent implements OnChanges, OnDestroy {
  @Input() providerHealth: Record<string, string> | null = null;
  @Input() providersList: ProviderHealthItem[] = [];

  public providers: ProviderHealthItem[] = [];
  private cooldownTimers: Map<string, number> = new Map();
  private intervalId: any = null;

  ngOnChanges(_changes: SimpleChanges): void {
    this.updateProviders();
  }

  ngOnDestroy(): void {
    this.stopTimer();
  }

  private updateProviders(): void {
    if (this.providersList && this.providersList.length > 0) {
      this.providers = this.providersList.map(p => ({ ...p }));
    } else if (this.providerHealth) {
      this.providers = Object.entries(this.providerHealth).map(([name, status]) => {
        const existingCooldown = this.cooldownTimers.get(name) ?? (this.isCooldown(status) ? 30 : 0);
        return {
          name,
          status,
          cooldownSeconds: existingCooldown
        };
      });
    } else {
      this.providers = [
        { name: 'Anthropic', status: 'Healthy' },
        { name: 'AWS Bedrock', status: 'Healthy' },
        { name: 'OpenAI', status: 'Healthy' }
      ];
    }

    let hasCooldown = false;
    for (const p of this.providers) {
      if (this.isCooldown(p.status)) {
        hasCooldown = true;
        if (!this.cooldownTimers.has(p.name)) {
          this.cooldownTimers.set(p.name, p.cooldownSeconds ?? 30);
        }
      } else {
        this.cooldownTimers.delete(p.name);
      }
    }

    if (hasCooldown && !this.intervalId) {
      this.startTimer();
    } else if (!hasCooldown && this.intervalId) {
      this.stopTimer();
    }
  }

  private startTimer(): void {
    this.intervalId = setInterval(() => {
      let activeCooldowns = false;
      this.providers.forEach(p => {
        if (this.isCooldown(p.status)) {
          const current = this.cooldownTimers.get(p.name) ?? (p.cooldownSeconds ?? 30);
          if (current > 0) {
            const next = current - 1;
            this.cooldownTimers.set(p.name, next);
            p.cooldownSeconds = next;
            activeCooldowns = true;
          }
        }
      });
      if (!activeCooldowns) {
        this.stopTimer();
      }
    }, 1000);
  }

  private stopTimer(): void {
    if (this.intervalId) {
      clearInterval(this.intervalId);
      this.intervalId = null;
    }
  }

  public isCooldown(status: string): boolean {
    const s = status.toLowerCase();
    return s === 'cooldown' || s === 'in cooldown' || s === 'in_cooldown';
  }

  public getCooldownRemaining(provider: ProviderHealthItem): number {
    return this.cooldownTimers.get(provider.name) ?? provider.cooldownSeconds ?? 0;
  }

  public getStatusLabel(status: string): string {
    const s = status.toLowerCase();
    if (s === 'healthy' || s === 'active') return 'Active';
    if (s === 'degraded') return 'Degraded';
    if (this.isCooldown(status)) return 'In Cooldown';
    if (s === 'autherror' || s === 'auth error' || s === 'auth_error') return 'Auth Error';
    return status;
  }

  public getBadgeClass(status: string): string {
    const s = status.toLowerCase();
    if (s === 'healthy' || s === 'active') {
      return 'bg-emerald-950/80 text-emerald-400 border border-emerald-800/60';
    }
    if (s === 'degraded') {
      return 'bg-yellow-950/80 text-yellow-400 border border-yellow-800/60';
    }
    if (this.isCooldown(status)) {
      return 'bg-amber-950/80 text-amber-400 border border-amber-800/60';
    }
    if (s === 'autherror' || s === 'auth error' || s === 'auth_error') {
      return 'bg-rose-950/80 text-rose-400 border border-rose-800/60';
    }
    return 'bg-neutral-800 text-neutral-300 border border-neutral-700';
  }

  public getDotClass(status: string): string {
    const s = status.toLowerCase();
    if (s === 'healthy' || s === 'active') return 'bg-emerald-400 animate-pulse';
    if (s === 'degraded') return 'bg-yellow-400';
    if (this.isCooldown(status)) return 'bg-amber-400 animate-ping';
    if (s === 'autherror' || s === 'auth error' || s === 'auth_error') return 'bg-rose-400';
    return 'bg-neutral-400';
  }

  public getCardBorderClass(status: string): string {
    const s = status.toLowerCase();
    if (s === 'healthy' || s === 'active') return 'border-neutral-800 hover:border-emerald-800/50';
    if (s === 'degraded') return 'border-yellow-800/50';
    if (this.isCooldown(status)) return 'border-amber-800/60 bg-amber-950/10';
    if (s === 'autherror' || s === 'auth error' || s === 'auth_error') return 'border-rose-800/60 bg-rose-950/10';
    return 'border-neutral-800';
  }

  public trackByName(_: number, item: ProviderHealthItem): string {
    return item.name;
  }
}
