import { Component } from '@angular/core';
import { CommonModule } from '@angular/common';
import { ConfigEditorViewComponent } from './config-editor-view.component';

@Component({
  selector: 'app-config',
  standalone: true,
  imports: [CommonModule, ConfigEditorViewComponent],
  template: `
    <app-config-editor-view />
  `
})
export class ConfigComponent {}
