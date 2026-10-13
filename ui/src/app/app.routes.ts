import { Routes } from '@angular/router';

export const routes: Routes = [
  {
    path: '',
    redirectTo: 'overview',
    pathMatch: 'full'
  },
  {
    path: 'overview',
    loadComponent: () => import('./features/overview/overview.component').then((m) => m.OverviewComponent)
  },
  {
    path: 'sessions',
    loadComponent: () => import('./features/sessions/sessions.component').then((m) => m.SessionsComponent)
  },
  {
    path: 'benchmarks',
    loadComponent: () => import('./features/benchmarks/benchmarks.component').then((m) => m.BenchmarksComponent)
  },
  {
    path: 'config',
    loadComponent: () => import('./features/config/config.component').then((m) => m.ConfigComponent)
  },
  {
    path: 'models',
    loadComponent: () => import('./features/models/upstream-models-view.component').then((m) => m.UpstreamModelsViewComponent)
  },
  {
    path: '**',
    redirectTo: 'overview'
  }
];
