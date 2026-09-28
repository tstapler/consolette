import { Component } from '@angular/core';
import { CommonModule } from '@angular/common';
import { SessionReplayViewComponent } from './session-replay-view.component';

@Component({
  selector: 'app-sessions',
  standalone: true,
  imports: [CommonModule, SessionReplayViewComponent],
  template: `
    <app-session-replay-view></app-session-replay-view>
  `
})
export class SessionsComponent {}
