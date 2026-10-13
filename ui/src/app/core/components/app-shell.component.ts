import { Component, ElementRef, inject, OnInit, signal } from '@angular/core';
import { CommonModule } from '@angular/common';
import { RouterModule } from '@angular/router';
import { SseService } from '../services/sse.service';

interface NavItem { path: string; label: string; exact: boolean; icon: string[]; }

const NAV_ITEMS: NavItem[] = [
  { path: '/overview', label: 'Overview', exact: true, icon: ['M9 19v-6a2 2 0 012-2h2a2 2 0 012 2v6m-6 0a2 2 0 002 2h2a2 2 0 002-2m0 0V9a2 2 0 012-2h2a2 2 0 012 2v10m-6 0a2 2 0 002 2h2a2 2 0 002-2'] },
  { path: '/sessions', label: 'Session Replay', exact: false, icon: ['M14.752 11.168l-3.197-2.132A1 1 0 0010 9.87v4.263a1 1 0 001.555.832l3.197-2.132a1 1 0 000-1.664z', 'M21 12a9 9 0 11-18 0 9 9 0 0118 0z'] },
  { path: '/benchmarks', label: 'Model Benchmarks', exact: false, icon: ['M13 10V3L4 14h7v7l9-11h-7z'] },
  { path: '/models', label: 'Models', exact: false, icon: ['M4 6h16M4 10h16M4 14h16M4 18h16'] },
  { path: '/config', label: 'Config Editor', exact: false, icon: ['M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.065 2.572c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.572 1.065c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.065-2.572c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z', 'M15 12a3 3 0 11-6 0 3 3 0 016 0z'] },
];

@Component({
  selector: 'app-shell',
  standalone: true,
  imports: [CommonModule, RouterModule],
  template: `
    <div class="min-h-screen flex flex-col bg-slate-950 text-slate-100 font-sans">
      <a href="#main-content" data-skip-link (click)="skipToContent($event)"
        class="sr-only focus:not-sr-only focus:fixed focus:top-2 focus:left-2 focus:z-[60] focus:px-4 focus:py-2 focus:rounded-lg focus:bg-sky-300 focus:text-slate-950 focus:font-semibold">Skip to content</a>

      <!-- Top Navigation Header -->
      <header class="h-16 border-b border-slate-800 bg-slate-900/80 backdrop-blur px-3 sm:px-6 flex items-center justify-between gap-2 sticky top-0 z-50">
        <div class="flex items-center gap-2 sm:gap-4 min-w-0">
          <button type="button" data-menu-toggle class="md:hidden shrink-0 w-10 h-10 inline-flex items-center justify-center rounded-lg border border-slate-700 text-slate-200"
            aria-controls="mobile-nav" [attr.aria-expanded]="menuOpen() ? 'true' : 'false'" [attr.aria-label]="menuOpen() ? 'Close navigation menu' : 'Open navigation menu'"
            (click)="menuOpen.set(!menuOpen())">
            <svg class="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24" aria-hidden="true">
              <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" [attr.d]="menuOpen() ? 'M6 18L18 6M6 6l12 12' : 'M4 6h16M4 12h16M4 18h16'"></path>
            </svg>
          </button>
          <div class="flex items-center space-x-2 min-w-0">
            <span class="w-8 h-8 shrink-0 rounded-lg bg-cyan-500 flex items-center justify-center font-bold text-slate-950 text-lg shadow-lg shadow-cyan-500/20">
              C
            </span>
            <span class="hidden min-[400px]:inline text-xl font-bold tracking-tight bg-gradient-to-r from-white to-slate-400 bg-clip-text text-transparent">
              Consolette
            </span>
          </div>
          <span class="hidden sm:inline px-2 py-0.5 text-xs font-mono rounded-full bg-slate-800 text-cyan-400 border border-slate-700">
            v1.0.0
          </span>
        </div>

        <!-- Connection Health Badge -->
        <div class="flex items-center space-x-6 shrink-0">
          <div class="flex items-center space-x-2 text-sm font-medium">
            <span class="hidden sm:inline text-xs text-slate-400 font-mono uppercase tracking-wider">SSE Telemetry:</span>

            <div [ngClass]="{
              'bg-emerald-500/10 text-emerald-400 border-emerald-500/30': connectionStatus() === 'connected',
              'bg-amber-500/10 text-amber-400 border-amber-500/30': connectionStatus() === 'reconnecting',
              'bg-rose-500/10 text-rose-400 border-rose-500/30': connectionStatus() === 'disconnected'
            }" class="px-3 py-1 rounded-full border text-xs font-semibold flex items-center space-x-2 whitespace-nowrap transition-all duration-300">
              <span data-status-dot [ngClass]="{
                'bg-emerald-500 border-emerald-300 motion-safe:animate-pulse': connectionStatus() === 'connected',
                'bg-amber-500 border-amber-300 motion-safe:animate-ping': connectionStatus() === 'reconnecting',
                'bg-rose-500 border-rose-300': connectionStatus() === 'disconnected'
              }" class="w-2 h-2 rounded-full inline-block border"></span>

              <span class="capitalize" data-testid="connection-badge-text">
                {{ connectionStatus() }}
                <ng-container *ngIf="connectionStatus() === 'reconnecting'">
                  (retry #{{ sseService.retryCount() }})
                </ng-container>
              </span>
            </div>
          </div>
        </div>
      </header>

      <nav *ngIf="menuOpen()" id="mobile-nav" data-mobile-nav aria-label="Main" class="md:hidden border-b border-slate-800 bg-slate-900 p-3 space-y-1">
        <a *ngFor="let item of navItems" [routerLink]="item.path" routerLinkActive="bg-cyan-500/10 text-cyan-400 border-cyan-500/30"
          [routerLinkActiveOptions]="{ exact: item.exact }" (click)="menuOpen.set(false)"
          class="flex items-center space-x-3 px-4 py-3 rounded-lg text-sm font-medium text-slate-300 hover:bg-slate-800/60 hover:text-white border border-transparent">
          <svg class="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24" aria-hidden="true">
            <path *ngFor="let d of item.icon" stroke-linecap="round" stroke-linejoin="round" stroke-width="2" [attr.d]="d"></path>
          </svg>
          <span>{{ item.label }}</span>
        </a>
      </nav>

      <!-- Body Layout: Sidebar + Main Content -->
      <div class="flex flex-1">
        <!-- Sidebar Navigation: sticky so it stays in view on long pages (E-25) -->
        <aside class="sticky top-16 self-start h-[calc(100vh-4rem)] w-64 shrink-0 border-r border-slate-800 bg-slate-900/40 p-4 flex-col justify-between hidden md:flex">
          <nav aria-label="Main" class="space-y-1">
            <a *ngFor="let item of navItems" [routerLink]="item.path" routerLinkActive="bg-cyan-500/10 text-cyan-400 border-cyan-500/30"
              [routerLinkActiveOptions]="{ exact: item.exact }"
              class="flex items-center space-x-3 px-4 py-3 rounded-lg text-sm font-medium text-slate-300 hover:bg-slate-800/60 hover:text-white border border-transparent transition-all">
              <svg class="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24" aria-hidden="true">
                <path *ngFor="let d of item.icon" stroke-linecap="round" stroke-linejoin="round" stroke-width="2" [attr.d]="d"></path>
              </svg>
              <span>{{ item.label }}</span>
            </a>
          </nav>

          <div class="p-3 bg-slate-900 border border-slate-800 rounded-lg text-xs text-slate-400">
            <p class="font-semibold text-slate-300 mb-1">Axum 0.8 Engine</p>
            <p>Air-gapped single-binary WebUI with real-time SSE push.</p>
          </div>
        </aside>

        <!-- Main Workspace Area -->
        <main id="main-content" tabindex="-1" class="flex-1 min-w-0 p-3 sm:p-6 bg-slate-950 overflow-y-auto">
          <router-outlet></router-outlet>
        </main>
      </div>
    </div>
  `
})
export class AppShellComponent implements OnInit {
  private readonly host = inject<ElementRef<HTMLElement>>(ElementRef);
  protected readonly navItems = NAV_ITEMS;
  protected readonly menuOpen = signal(false);
  protected readonly sseService = inject(SseService);
  public readonly connectionStatus = this.sseService.connectionStatus;

  /** One skip link for every page: lands on the page's `[data-skip-target]` (the live traces on Overview), else on `main`. */
  protected skipToContent(event: Event): void {
    event.preventDefault();
    const root = this.host.nativeElement;
    const target = root.querySelector<HTMLElement>('[data-skip-target]') ?? root.querySelector<HTMLElement>('main');
    target?.focus();
  }

  ngOnInit(): void {
    if (this.connectionStatus() === 'disconnected') {
      this.sseService.connect();
    }
  }
}
