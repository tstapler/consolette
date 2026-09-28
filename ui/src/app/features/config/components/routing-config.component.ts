import { Component, input, output } from '@angular/core';
import { CommonModule } from '@angular/common';
import { FormsModule } from '@angular/forms';

export interface ProviderConfigItem {
  name: string;
  apiKey?: string;
  baseUrl?: string;
  weight?: number;
}

@Component({
  selector: 'app-routing-config',
  standalone: true,
  imports: [CommonModule, FormsModule],
  template: `
    <div class="bg-slate-900 border border-slate-800 rounded-xl p-6 space-y-6">
      <div class="flex items-center justify-between">
        <div>
          <h2 class="text-lg font-semibold text-white">Routing & Load Balancing</h2>
          <p class="text-xs text-slate-400">Configure provider weights (total 100%) and credential endpoints.</p>
        </div>
        <div class="px-3 py-1 text-xs font-mono rounded-full border"
             [ngClass]="totalWeight === 100 ? 'bg-emerald-500/10 text-emerald-400 border-emerald-500/30' : 'bg-amber-500/10 text-amber-400 border-amber-500/30'">
          Total Weight: {{ totalWeight }}%
        </div>
      </div>

      <div class="space-y-4">
        @for (provider of providers(); track provider.name) {
          <div class="bg-slate-950 border border-slate-800 rounded-lg p-4 space-y-3">
            <div class="flex items-center justify-between">
              <span class="font-medium text-slate-200 capitalize">{{ provider.name }}</span>
              <span class="text-xs font-mono text-cyan-400 font-bold">{{ provider.weight || 0 }}%</span>
            </div>

            <!-- Weight Slider -->
            <div class="space-y-1">
              <input
                type="range"
                min="0"
                max="100"
                step="5"
                [ngModel]="provider.weight || 0"
                (ngModelChange)="onWeightChange(provider.name, $event)"
                class="w-full accent-cyan-500 bg-slate-800 rounded-lg h-2 cursor-pointer"
                [attr.data-testid]="'slider-' + provider.name"
              />
            </div>

            <div class="grid grid-cols-1 md:grid-cols-2 gap-3 pt-2">
              <!-- API Key Input -->
              <div>
                <label class="block text-xs font-medium text-slate-400 mb-1">API Key</label>
                <input
                  type="text"
                  [ngModel]="provider.apiKey || ''"
                  (ngModelChange)="onApiKeyChange(provider.name, $event)"
                  placeholder="sk-...****"
                  class="w-full bg-slate-900 border border-slate-800 text-xs font-mono text-slate-200 rounded px-3 py-1.5 focus:border-cyan-500 focus:outline-none"
                  [attr.data-testid]="'apikey-' + provider.name"
                />
              </div>

              <!-- Base URL Input -->
              <div>
                <label class="block text-xs font-medium text-slate-400 mb-1">Base URL (HTTPS)</label>
                <input
                  type="text"
                  [ngModel]="provider.baseUrl || ''"
                  (ngModelChange)="onBaseUrlChange(provider.name, $event)"
                  placeholder="https://api.example.com/v1"
                  class="w-full bg-slate-900 border border-slate-800 text-xs font-mono text-slate-200 rounded px-3 py-1.5 focus:border-cyan-500 focus:outline-none"
                  [attr.data-testid]="'baseurl-' + provider.name"
                />
              </div>
            </div>
          </div>
        }
      </div>
    </div>
  `
})
export class RoutingConfigComponent {
  public providers = input<ProviderConfigItem[]>([]);
  public providersChange = output<ProviderConfigItem[]>();

  get totalWeight(): number {
    return this.providers().reduce((sum, p) => sum + (p.weight || 0), 0);
  }

  public onWeightChange(name: string, newWeight: number): void {
    const updated = this.providers().map((p) =>
      p.name === name ? { ...p, weight: Number(newWeight) } : p
    );
    this.providersChange.emit(updated);
  }

  public onApiKeyChange(name: string, newKey: string): void {
    const updated = this.providers().map((p) =>
      p.name === name ? { ...p, apiKey: newKey } : p
    );
    this.providersChange.emit(updated);
  }

  public onBaseUrlChange(name: string, newUrl: string): void {
    const updated = this.providers().map((p) =>
      p.name === name ? { ...p, baseUrl: newUrl } : p
    );
    this.providersChange.emit(updated);
  }
}
