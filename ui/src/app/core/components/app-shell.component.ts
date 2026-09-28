import { Component, inject, OnInit } from '@angular/core';
import { CommonModule } from '@angular/common';
import { RouterModule } from '@angular/router';
import { SseService } from '../services/sse.service';

@Component({
  selector: 'app-shell',
  standalone: true,
  imports: [CommonModule, RouterModule],
  template: `
    <div class="min-h-screen flex flex-col bg-slate-950 text-slate-100 font-sans">
      <!-- Top Navigation Header -->
      <header class="h-16 border-b border-slate-800 bg-slate-900/80 backdrop-blur px-6 flex items-center justify-between sticky top-0 z-50">
        <div class="flex items-center space-x-4">
          <div class="flex items-center space-x-2">
            <span class="w-8 h-8 rounded-lg bg-cyan-500 flex items-center justify-center font-bold text-slate-950 text-lg shadow-lg shadow-cyan-500/20">
              C
            </span>
            <span class="text-xl font-bold tracking-tight bg-gradient-to-r from-white to-slate-400 bg-clip-text text-transparent">
              Consolette
            </span>
          </div>
          <span class="px-2 py-0.5 text-xs font-mono rounded-full bg-slate-800 text-cyan-400 border border-slate-700">
            v1.0.0
          </span>
        </div>

        <!-- Connection Health Badge -->
        <div class="flex items-center space-x-6">
          <div class="flex items-center space-x-2 text-sm font-medium">
            <span class="text-xs text-slate-400 font-mono uppercase tracking-wider">SSE Telemetry:</span>
            
            <div [ngClass]="{
              'bg-emerald-500/10 text-emerald-400 border-emerald-500/30': connectionStatus() === 'connected',
              'bg-amber-500/10 text-amber-400 border-amber-500/30': connectionStatus() === 'reconnecting',
              'bg-rose-500/10 text-rose-400 border-rose-500/30': connectionStatus() === 'disconnected'
            }" class="px-3 py-1 rounded-full border text-xs font-semibold flex items-center space-x-2 transition-all duration-300">
              <span [ngClass]="{
                'bg-emerald-500 animate-pulse': connectionStatus() === 'connected',
                'bg-amber-500 animate-ping': connectionStatus() === 'reconnecting',
                'bg-rose-500': connectionStatus() === 'disconnected'
              }" class="w-2 h-2 rounded-full inline-block"></span>
              
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

      <!-- Body Layout: Sidebar + Main Content -->
      <div class="flex flex-1">
        <!-- Sidebar Navigation -->
        <aside class="w-64 border-r border-slate-800 bg-slate-900/40 p-4 flex flex-col justify-between hidden md:flex">
          <nav class="space-y-1">
            <a
              routerLink="/overview"
              routerLinkActive="bg-cyan-500/10 text-cyan-400 border-cyan-500/30"
              [routerLinkActiveOptions]="{ exact: true }"
              class="flex items-center space-x-3 px-4 py-3 rounded-lg text-sm font-medium text-slate-300 hover:bg-slate-800/60 hover:text-white border border-transparent transition-all"
            >
              <svg class="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 19v-6a2 2 0 012-2h2a2 2 0 012 2v6m-6 0a2 2 0 002 2h2a2 2 0 002-2m0 0V9a2 2 0 012-2h2a2 2 0 012 2v10m-6 0a2 2 0 002 2h2a2 2 0 002-2"></path>
              </svg>
              <span>Overview</span>
            </a>

            <a
              routerLink="/sessions"
              routerLinkActive="bg-cyan-500/10 text-cyan-400 border-cyan-500/30"
              class="flex items-center space-x-3 px-4 py-3 rounded-lg text-sm font-medium text-slate-300 hover:bg-slate-800/60 hover:text-white border border-transparent transition-all"
            >
              <svg class="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M14.752 11.168l-3.197-2.132A1 1 0 0010 9.87v4.263a1 1 0 001.555.832l3.197-2.132a1 1 0 000-1.664z"></path>
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M21 12a9 9 0 11-18 0 9 9 0 0118 0z"></path>
              </svg>
              <span>Session Replay</span>
            </a>

            <a
              routerLink="/benchmarks"
              routerLinkActive="bg-cyan-500/10 text-cyan-400 border-cyan-500/30"
              class="flex items-center space-x-3 px-4 py-3 rounded-lg text-sm font-medium text-slate-300 hover:bg-slate-800/60 hover:text-white border border-transparent transition-all"
            >
              <svg class="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 10V3L4 14h7v7l9-11h-7z"></path>
              </svg>
              <span>Model Benchmarks</span>
            </a>

            <a
              routerLink="/config"
              routerLinkActive="bg-cyan-500/10 text-cyan-400 border-cyan-500/30"
              class="flex items-center space-x-3 px-4 py-3 rounded-lg text-sm font-medium text-slate-300 hover:bg-slate-800/60 hover:text-white border border-transparent transition-all"
            >
              <svg class="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.065 2.572c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.572 1.065c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.065-2.572c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z"></path>
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15 12a3 3 0 11-6 0 3 3 0 016 0z"></path>
              </svg>
              <span>Config Editor</span>
            </a>
          </nav>

          <div class="p-3 bg-slate-900 border border-slate-800 rounded-lg text-xs text-slate-400">
            <p class="font-semibold text-slate-300 mb-1">Axum 0.8 Engine</p>
            <p>Air-gapped single-binary WebUI with real-time SSE push.</p>
          </div>
        </aside>

        <!-- Main Workspace Area -->
        <main class="flex-1 p-6 bg-slate-950 overflow-y-auto">
          <router-outlet></router-outlet>
        </main>
      </div>
    </div>
  `
})
export class AppShellComponent implements OnInit {
  protected readonly sseService = inject(SseService);
  public readonly connectionStatus = this.sseService.connectionStatus;

  ngOnInit(): void {
    if (this.connectionStatus() === 'disconnected') {
      this.sseService.connect();
    }
  }
}
