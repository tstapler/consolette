export type CatalogState = 'loading' | 'ok' | 'stale' | 'error' | 'unsupported';
export type CatalogErrorKind =
  | 'auth' | 'rateLimited' | 'timeout' | 'upstream' | 'unexpectedResponse' | 'other';

export interface CatalogModel {
  id: string;
  ownedBy: string | null;
}

export interface CatalogError {
  kind: CatalogErrorKind;
  status: number | null;
  message: string;
}

export interface UpstreamCatalog {
  state: CatalogState;
  models: CatalogModel[];
  totalCount: number;
  truncated: boolean;
  refreshedAt: string | null;
  lastAttemptAt: string | null;
  error: CatalogError | null;
  refreshing: boolean;
}

export interface ConfigWarning {
  upstream: string;
  model: string;
  referencedBy: string;
}

export interface ModelCatalog {
  upstreams: Record<string, UpstreamCatalog>;
  configWarnings: ConfigWarning[];
}

export interface RefreshResponse extends ModelCatalog {
  refreshing: boolean;
  coalesced: boolean;
  retryAfterSecs: number | null;
}

export interface ModelCatalogChangedData {
  timestamp: string;
  upstream: string;
}
