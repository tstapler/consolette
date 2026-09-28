import { Component, Input, OnChanges, SimpleChanges, signal } from '@angular/core';
import { CommonModule } from '@angular/common';

export interface DiffLine {
  type: 'added' | 'removed' | 'unchanged';
  originalLineNumber?: number;
  compressedLineNumber?: number;
  content: string;
}

export interface SideBySidePair {
  left?: { lineNumber: number; content: string; type: 'removed' | 'unchanged' };
  right?: { lineNumber: number; content: string; type: 'added' | 'unchanged' };
}

@Component({
  selector: 'app-payload-diff-viewer',
  standalone: true,
  imports: [CommonModule],
  template: `
    <div class="bg-neutral-900 border border-neutral-800 rounded-lg p-4 space-y-4">
      <div class="flex items-center justify-between">
        <h3 class="text-lg font-semibold text-white">Payload Diff Inspector</h3>
        <div class="flex items-center gap-1 bg-neutral-950 p-1 border border-neutral-800 rounded-md">
          <button
            type="button"
            (click)="setMode('side-by-side')"
            [class.bg-neutral-800]="viewMode() === 'side-by-side'"
            [class.text-white]="viewMode() === 'side-by-side'"
            class="px-3 py-1 text-xs font-medium rounded text-slate-400 hover:text-white transition-colors"
          >
            Side-by-Side
          </button>
          <button
            type="button"
            (click)="setMode('inline')"
            [class.bg-neutral-800]="viewMode() === 'inline'"
            [class.text-white]="viewMode() === 'inline'"
            class="px-3 py-1 text-xs font-medium rounded text-slate-400 hover:text-white transition-colors"
          >
            Inline / Unified
          </button>
        </div>
      </div>

      <!-- Side-by-Side View -->
      @if (viewMode() === 'side-by-side') {
        <div class="grid grid-cols-2 gap-2 font-mono text-xs overflow-x-auto bg-neutral-950 border border-neutral-800 rounded-md p-2">
          <!-- Left: Original -->
          <div class="border-r border-neutral-800 pr-2">
            <div class="text-xs uppercase text-slate-500 font-bold mb-2 pb-1 border-b border-neutral-800">
              Original Payload
            </div>
            <div class="space-y-0.5">
              @for (pair of sideBySidePairs(); track $index) {
                <div
                  class="flex items-start whitespace-pre rounded px-1"
                  [class.bg-red-950]="pair.left && pair.left.type === 'removed'"
                  [class.text-red-400]="pair.left && pair.left.type === 'removed'"
                  [class.text-slate-300]="pair.left && pair.left.type === 'unchanged'"
                >
                  <span class="w-8 text-right text-slate-600 select-none pr-2 shrink-0">
                    {{ pair.left ? pair.left.lineNumber : '' }}
                  </span>
                  <span class="break-all">{{ pair.left ? pair.left.content : '' }}</span>
                </div>
              }
            </div>
          </div>

          <!-- Right: Compressed -->
          <div class="pl-2">
            <div class="text-xs uppercase text-slate-500 font-bold mb-2 pb-1 border-b border-neutral-800">
              Compressed Payload
            </div>
            <div class="space-y-0.5">
              @for (pair of sideBySidePairs(); track $index) {
                <div
                  class="flex items-start whitespace-pre rounded px-1"
                  [class.bg-emerald-950]="pair.right && pair.right.type === 'added'"
                  [class.text-emerald-400]="pair.right && pair.right.type === 'added'"
                  [class.text-slate-300]="pair.right && pair.right.type === 'unchanged'"
                >
                  <span class="w-8 text-right text-slate-600 select-none pr-2 shrink-0">
                    {{ pair.right ? pair.right.lineNumber : '' }}
                  </span>
                  <span class="break-all">{{ pair.right ? pair.right.content : '' }}</span>
                </div>
              }
            </div>
          </div>
        </div>
      }

      <!-- Inline / Unified View -->
      @if (viewMode() === 'inline') {
        <div class="font-mono text-xs overflow-x-auto bg-neutral-950 border border-neutral-800 rounded-md p-2 space-y-0.5">
          <div class="text-xs uppercase text-slate-500 font-bold mb-2 pb-1 border-b border-neutral-800">
            Unified Diff (+ Added / - Removed)
          </div>
          @for (line of unifiedLines(); track $index) {
            <div
              class="flex items-start whitespace-pre rounded px-1"
              [class.bg-red-950]="line.type === 'removed'"
              [class.text-red-400]="line.type === 'removed'"
              [class.bg-emerald-950]="line.type === 'added'"
              [class.text-emerald-400]="line.type === 'added'"
              [class.text-slate-300]="line.type === 'unchanged'"
            >
              <span class="w-8 text-right text-slate-600 select-none pr-1 shrink-0">
                {{ line.originalLineNumber ?? '' }}
              </span>
              <span class="w-8 text-right text-slate-600 select-none pr-2 shrink-0">
                {{ line.compressedLineNumber ?? '' }}
              </span>
              <span class="w-4 text-center select-none font-bold shrink-0">
                {{ line.type === 'added' ? '+' : line.type === 'removed' ? '-' : ' ' }}
              </span>
              <span class="break-all">{{ line.content }}</span>
            </div>
          }
        </div>
      }
    </div>
  `
})
export class PayloadDiffViewerComponent implements OnChanges {
  @Input() originalPayload: any = null;
  @Input() compressedPayload: any = null;

  public readonly viewMode = signal<'side-by-side' | 'inline'>('side-by-side');
  public readonly unifiedLines = signal<DiffLine[]>([]);
  public readonly sideBySidePairs = signal<SideBySidePair[]>([]);

  public ngOnChanges(changes: SimpleChanges): void {
    if (changes['originalPayload'] || changes['compressedPayload']) {
      this.computeDiff();
    }
  }

  public setMode(mode: 'side-by-side' | 'inline'): void {
    this.viewMode.set(mode);
  }

  private computeDiff(): void {
    const origStr = this.formatPayload(this.originalPayload);
    const compStr = this.formatPayload(this.compressedPayload);

    const origLines = origStr ? origStr.split('\n') : [];
    const compLines = compStr ? compStr.split('\n') : [];

    const unified: DiffLine[] = [];
    const pairs: SideBySidePair[] = [];

    let i = 0;
    let j = 0;
    let origNum = 1;
    let compNum = 1;

    while (i < origLines.length || j < compLines.length) {
      if (i < origLines.length && j < compLines.length && origLines[i] === compLines[j]) {
        unified.push({
          type: 'unchanged',
          originalLineNumber: origNum,
          compressedLineNumber: compNum,
          content: origLines[i]
        });
        pairs.push({
          left: { lineNumber: origNum, content: origLines[i], type: 'unchanged' },
          right: { lineNumber: compNum, content: compLines[j], type: 'unchanged' }
        });
        i++;
        j++;
        origNum++;
        compNum++;
      } else {
        let foundInComp = -1;
        for (let k = j; k < Math.min(j + 5, compLines.length); k++) {
          if (origLines[i] === compLines[k]) {
            foundInComp = k;
            break;
          }
        }

        if (foundInComp !== -1) {
          while (j < foundInComp) {
            unified.push({
              type: 'added',
              compressedLineNumber: compNum,
              content: compLines[j]
            });
            pairs.push({
              right: { lineNumber: compNum, content: compLines[j], type: 'added' }
            });
            j++;
            compNum++;
          }
        } else if (i < origLines.length) {
          unified.push({
            type: 'removed',
            originalLineNumber: origNum,
            content: origLines[i]
          });
          pairs.push({
            left: { lineNumber: origNum, content: origLines[i], type: 'removed' }
          });
          i++;
          origNum++;
        } else {
          unified.push({
            type: 'added',
            compressedLineNumber: compNum,
            content: compLines[j]
          });
          pairs.push({
            right: { lineNumber: compNum, content: compLines[j], type: 'added' }
          });
          j++;
          compNum++;
        }
      }
    }

    this.unifiedLines.set(unified);
    this.sideBySidePairs.set(pairs);
  }

  private formatPayload(payload: any): string {
    if (!payload) return '';
    if (typeof payload === 'string') {
      try {
        return JSON.stringify(JSON.parse(payload), null, 2);
      } catch {
        return payload;
      }
    }
    return JSON.stringify(payload, null, 2);
  }
}
