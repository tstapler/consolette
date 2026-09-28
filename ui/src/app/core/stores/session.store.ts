import { Injectable, inject, signal, computed } from '@angular/core';
import { HttpClient } from '@angular/common/http';
import { SessionData } from '../models/telemetry.models';
import { catchError, of } from 'rxjs';

@Injectable({
  providedIn: 'root'
})
export class SessionStore {
  private readonly http = inject(HttpClient);

  private readonly sessionsSignal = signal<SessionData[]>([]);
  private readonly selectedIdSignal = signal<string | null>(null);
  private readonly isLoadingSignal = signal<boolean>(false);
  private readonly errorSignal = signal<string | null>(null);

  public readonly sessions = this.sessionsSignal.asReadonly();
  public readonly selectedSessionId = this.selectedIdSignal.asReadonly();
  public readonly isLoading = this.isLoadingSignal.asReadonly();
  public readonly error = this.errorSignal.asReadonly();

  public readonly selectedSession = computed(() => {
    const id = this.selectedIdSignal();
    if (!id) return null;
    return this.sessionsSignal().find((s) => s.id === id) ?? null;
  });

  public loadSessions(search?: string, limit = 50, cursor?: string): void {
    this.isLoadingSignal.set(true);
    this.errorSignal.set(null);

    let params: Record<string, string> = { limit: limit.toString() };
    if (search) params['search'] = search;
    if (cursor) params['cursor'] = cursor;

    this.http.get<SessionData[]>('/v1/dashboard/sessions', { params })
      .pipe(
        catchError((err) => {
          this.errorSignal.set(err.message || 'Failed to load sessions');
          this.isLoadingSignal.set(false);
          return of([]);
        })
      )
      .subscribe((data) => {
        this.sessionsSignal.set(data);
        this.isLoadingSignal.set(false);
      });
  }

  public selectSession(id: string | null): void {
    this.selectedIdSignal.set(id);
  }

  public setSessions(sessions: SessionData[]): void {
    this.sessionsSignal.set(sessions);
  }

  public updateSession(updated: SessionData): void {
    this.sessionsSignal.update((existing) =>
      existing.map((s) => (s.id === updated.id ? updated : s))
    );
  }
}
