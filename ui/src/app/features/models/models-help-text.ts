/** Models-view help text; the exposure line mirrors the verified dashboard bind/auth/CORS stance. */
export const MODELS_HELP_TEXT = {
  mode:
    'Refresh mode comes from model_catalog.refresh in the proxy config (on_demand by default: the catalog refreshes only when you press Refresh; interval refreshes on a timer; off never refreshes). Opening this page never refreshes.',
  credentialFlow:
    "Refreshing runs each upstream's credential flow, which can prompt for SSO or incur API calls.",
  exposure:
    'The dashboard listens on 127.0.0.1 only, has no authentication, and allows requests from any origin, so any local process or web page open in this browser can request a refresh.'
} as const;
