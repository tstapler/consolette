import { Component } from '@angular/core';
import { CommonModule } from '@angular/common';
import { OverviewDashboardComponent } from './overview-dashboard.component';

@Component({
  selector: 'app-overview',
  standalone: true,
  imports: [CommonModule, OverviewDashboardComponent],
  template: `<app-overview-dashboard></app-overview-dashboard>`
})
export class OverviewComponent {}
