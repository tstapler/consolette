import { Component } from '@angular/core';
import { CommonModule } from '@angular/common';
import { ModelBenchmarksViewComponent } from './model-benchmarks-view.component';

@Component({
  selector: 'app-benchmarks',
  standalone: true,
  imports: [CommonModule, ModelBenchmarksViewComponent],
  template: `<app-model-benchmarks-view></app-model-benchmarks-view>`
})
export class BenchmarksComponent {}
