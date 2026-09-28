import { Component, input, output } from '@angular/core';
import { CommonModule } from '@angular/common';

@Component({
  selector: 'app-config-diff-modal',
  standalone: true,
  imports: [CommonModule],
  template: `
    @if (isOpen()) {
      <div class="fixed inset-0 z-50 flex items-center justify-center bg-black/70 backdrop-blur-sm p-4" data-testid="diff-modal">
        <div class="bg-slate-900 border border-slate-800 rounded-2xl w-full max-w-3xl shadow-2xl flex flex-col max-h-[85vh]">
          <!-- Header -->
          <div class="flex items-center justify-between p-5 border-b border-slate-800">
            <div>
              <h3 class="text-lg font-semibold text-white">Review Configuration Changes</h3>
              <p class="text-xs text-slate-400">Compare current active config with proposed draft edits before hot reloading.</p>
            </div>
            <button
              type="button"
              (click)="closeModal()"
              class="text-slate-400 hover:text-white text-lg font-bold px-2 py-1"
              data-testid="close-modal"
            >
              ✕
            </button>
          </div>

          <!-- Body / JSON Diff -->
          <div class="p-6 overflow-y-auto space-y-4 font-mono text-xs flex-1">
            <div class="grid grid-cols-1 md:grid-cols-2 gap-4">
              <!-- Current Active Config -->
              <div>
                <span class="block text-slate-400 mb-2 font-sans text-xs font-medium">Active Configuration</span>
                <pre class="bg-slate-950 border border-slate-800 text-slate-300 p-4 rounded-lg overflow-x-auto whitespace-pre-wrap"
                     data-testid="original-json">{{ formatJson(originalConfig()) }}</pre>
              </div>

              <!-- Proposed Draft Config -->
              <div>
                <span class="block text-cyan-400 mb-2 font-sans text-xs font-medium">Proposed Draft</span>
                <pre class="bg-slate-950 border border-cyan-900/50 text-cyan-200 p-4 rounded-lg overflow-x-auto whitespace-pre-wrap"
                     data-testid="draft-json">{{ formatJson(draftConfig()) }}</pre>
              </div>
            </div>
          </div>

          <!-- Footer -->
          <div class="flex items-center justify-end space-x-3 p-5 border-t border-slate-800 bg-slate-950/50 rounded-b-2xl">
            <button
              type="button"
              (click)="closeModal()"
              class="px-4 py-2 text-xs font-medium text-slate-300 hover:text-white bg-slate-800 hover:bg-slate-700 rounded-lg"
              data-testid="cancel-btn"
            >
              Cancel
            </button>
            <button
              type="button"
              (click)="confirmApply()"
              class="px-4 py-2 text-xs font-medium text-white bg-cyan-600 hover:bg-cyan-500 rounded-lg shadow-lg shadow-cyan-500/20"
              data-testid="confirm-apply-btn"
            >
              Apply Config (Hot Reload)
            </button>
          </div>
        </div>
      </div>
    }
  `
})
export class ConfigDiffModalComponent {
  public isOpen = input<boolean>(false);
  public originalConfig = input<any>(null);
  public draftConfig = input<any>(null);

  public close = output<void>();
  public confirm = output<void>();

  public formatJson(obj: any): string {
    if (!obj) return '{}';
    return JSON.stringify(obj, null, 2);
  }

  public closeModal(): void {
    this.close.emit();
  }

  public confirmApply(): void {
    this.confirm.emit();
  }
}
