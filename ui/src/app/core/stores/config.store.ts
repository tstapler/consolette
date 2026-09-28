import { Injectable, inject, signal } from '@angular/core';
import { HttpClient } from '@angular/common/http';
import { ConfigData } from '../models/telemetry.models';
import { catchError, of, tap } from 'rxjs';

@Injectable({
  providedIn: 'root'
})
export class ConfigStore {
  private readonly http = inject(HttpClient);

  private readonly configSignal = signal<ConfigData | null>(null);
  private readonly isSavingSignal = signal<boolean>(false);
  private readonly lastAppliedSignal = signal<string | null>(null);
  private readonly errorSignal = signal<string | null>(null);

  public readonly config = this.configSignal.asReadonly();
  public readonly isSaving = this.isSavingSignal.asReadonly();
  public readonly lastApplied = this.lastAppliedSignal.asReadonly();
  public readonly error = this.errorSignal.asReadonly();

  public loadConfig(): void {
    this.errorSignal.set(null);

    this.http.get<ConfigData>('/v1/dashboard/config')
      .pipe(
        catchError((err) => {
          this.errorSignal.set(err.message || 'Failed to load configuration');
          return of(null);
        })
      )
      .subscribe((data) => {
        if (data) {
          this.configSignal.set(data);
        }
      });
  }

  public applyConfig(newConfig: ConfigData): void {
    this.isSavingSignal.set(true);
    this.errorSignal.set(null);

    this.http.put<ConfigData>('/v1/dashboard/config', newConfig)
      .pipe(
        catchError((err) => {
          this.errorSignal.set(err.message || 'Failed to apply configuration');
          this.isSavingSignal.set(false);
          return of(null);
        })
      )
      .subscribe((res) => {
        this.isSavingSignal.set(false);
        if (res) {
          this.configSignal.set(res);
          this.lastAppliedSignal.set(new Date().toISOString());
        }
      });
  }

  public updateDraft(draft: ConfigData): void {
    this.configSignal.set(draft);
  }

  public setConfig(config: ConfigData): void {
    this.configSignal.set(config);
  }
}
