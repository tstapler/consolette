import { Injectable, inject, signal } from '@angular/core';
import { HttpClient, HttpErrorResponse, HttpParams } from '@angular/common/http';
import { ModelCatalog, RefreshResponse } from '../models/model-catalog.models';

const CATALOG_URL = '/api/model-catalog';
/** Above the backend's 10 s per-upstream timeout. */
const BUSY_TIMEOUT_MS = 15_000;

interface Notice {
  text: string;
  /** Cleared together with the busy state instead of persisting. */
  whileBusy: boolean;
}

@Injectable({ providedIn: 'root' })
export class ModelCatalogStore {
  private readonly http = inject(HttpClient);

  private readonly catalogSignal = signal<ModelCatalog | null>(null);
  private readonly isLoadingSignal = signal<boolean>(false);
  private readonly loadErrorSignal = signal<string | null>(null);
  private readonly busySignal = signal<ReadonlySet<string>>(new Set());
  private readonly noticesSignal = signal<Readonly<Record<string, Notice>>>({});
  private readonly timers = new Map<string, ReturnType<typeof setTimeout>>();
  private opened = false;

  public readonly catalog = this.catalogSignal.asReadonly();
  public readonly isLoading = this.isLoadingSignal.asReadonly();
  /** HTTP status or `offline` when the last GET failed; null otherwise. */
  public readonly loadError = this.loadErrorSignal.asReadonly();

  public isBusy(upstream: string): boolean {
    return this.busySignal().has(upstream);
  }

  public notice(upstream: string): string | null {
    return this.noticesSignal()[upstream]?.text ?? null;
  }

  /** Single GET; never refreshes anything upstream. */
  public load(): void {
    this.opened = true;
    this.fetch();
  }

  /** Called on `ModelCatalogChanged`; ignored until the Models page was opened once. */
  public onCatalogChanged(): void {
    if (this.opened) {
      this.fetch();
    }
  }

  /** Explicit refresh of one upstream, or all when `upstream` is omitted. */
  public refresh(upstream?: string): void {
    const targets = upstream === undefined
      ? Object.keys(this.catalogSignal()?.upstreams ?? {})
      : [upstream];
    targets.forEach((name) => this.markBusy(name));
    const params = upstream === undefined ? new HttpParams() : new HttpParams().set('upstream', upstream);

    this.http.post<RefreshResponse>(`${CATALOG_URL}/refresh`, null, { params }).subscribe({
      next: (res) => this.onRefreshResponse(res, targets),
      error: (err: HttpErrorResponse) => {
        targets.forEach((name) => {
          this.clearBusy(name);
          this.setNotice(name, { text: `Refresh failed (${statusText(err)})`, whileBusy: false });
        });
      }
    });
  }

  private onRefreshResponse(res: RefreshResponse, targets: string[]): void {
    if (res.retryAfterSecs !== null) {
      targets.forEach((name) => {
        this.clearBusy(name);
        this.setNotice(name, { text: `Try again in ${res.retryAfterSecs}s`, whileBusy: false });
      });
      this.applyCatalog(res);
      return;
    }
    if (res.coalesced) {
      targets.forEach((name) => this.setNotice(name, { text: 'Refresh already running', whileBusy: true }));
    }
    this.applyCatalog(res);
  }

  private fetch(): void {
    this.isLoadingSignal.set(true);
    this.loadErrorSignal.set(null);
    this.http.get<ModelCatalog>(CATALOG_URL).subscribe({
      next: (data) => {
        this.isLoadingSignal.set(false);
        this.applyCatalog(data);
      },
      error: (err: HttpErrorResponse) => {
        this.isLoadingSignal.set(false);
        this.loadErrorSignal.set(statusText(err));
      }
    });
  }

  /** Busy ends once the snapshot shows that upstream no longer refreshing. */
  private applyCatalog(data: ModelCatalog): void {
    this.catalogSignal.set({ upstreams: data.upstreams, configWarnings: data.configWarnings });
    for (const name of this.busySignal()) {
      if (data.upstreams[name]?.refreshing === false) {
        this.clearBusy(name);
      }
    }
  }

  private markBusy(name: string): void {
    this.clearTimer(name);
    this.busySignal.update((s) => new Set(s).add(name));
    this.noticesSignal.update(({ [name]: _gone, ...rest }) => rest);
    this.timers.set(name, setTimeout(() => {
      if (this.busySignal().has(name)) {
        this.clearBusy(name);
        this.setNotice(name, { text: 'Refresh status unknown', whileBusy: false });
      }
    }, BUSY_TIMEOUT_MS));
  }

  private clearBusy(name: string): void {
    this.clearTimer(name);
    this.busySignal.update((s) => {
      const next = new Set(s);
      next.delete(name);
      return next;
    });
    if (this.noticesSignal()[name]?.whileBusy) {
      this.noticesSignal.update(({ [name]: _gone, ...rest }) => rest);
    }
  }

  private clearTimer(name: string): void {
    const timer = this.timers.get(name);
    if (timer !== undefined) {
      clearTimeout(timer);
      this.timers.delete(name);
    }
  }

  private setNotice(name: string, notice: Notice): void {
    this.noticesSignal.update((n) => ({ ...n, [name]: notice }));
  }
}

function statusText(err: HttpErrorResponse): string {
  return err.status === 0 ? 'offline' : String(err.status);
}
