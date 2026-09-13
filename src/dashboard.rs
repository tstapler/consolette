//! Dashboard HTML endpoint (`GET /dashboard`).
//!
//! The HTML is ported from the Python proxy's `DASHBOARD_HTML` constant in
//! `stapler-scripts/claude-proxy/main.py`. The JS polls `/metrics` and
//! `/errors/summary` automatically every 30/60 seconds.

use axum::response::{Html, IntoResponse};

/// The full dashboard HTML, inlined as a compile-time constant.
///
/// Auto-refreshes meta tag removed in favour of the JS setInterval polling
/// (30s for metrics, 60s for error types) which is less disruptive to the
/// Chart.js animation state.
const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Claude Proxy Dashboard</title>
    <script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.0/dist/chart.umd.min.js"></script>
    <style>
        * { margin: 0; padding: 0; box-sizing: border-box; }
        body {
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Oxygen, Ubuntu, Cantarell, sans-serif;
            background: #0a0a0a;
            color: #e0e0e0;
            padding: 20px;
        }
        .header {
            display: flex;
            justify-content: space-between;
            align-items: center;
            margin-bottom: 24px;
            padding-bottom: 16px;
            border-bottom: 1px solid #333;
        }
        h1 { font-size: 28px; font-weight: 600; color: #fff; }
        .status-bar { display: flex; gap: 16px; align-items: center; }
        .provider-status { display: flex; align-items: center; gap: 8px; font-size: 14px; }
        .status-indicator { width: 10px; height: 10px; border-radius: 50%; display: inline-block; }
        .status-active { background: #10b981; }
        .status-cooldown { background: #f59e0b; }
        .status-cold { background: #6b7280; }
        .status-auth-required { background: #ef4444; }
        .status-schema-drift { background: #8b5cf6; }
        .refresh-time { color: #888; font-size: 14px; }
        .stats-grid {
            display: grid;
            grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
            gap: 16px;
            margin-bottom: 24px;
        }
        .stat-card {
            background: #1a1a1a;
            border: 1px solid #2a2a2a;
            border-radius: 8px;
            padding: 16px;
        }
        .stat-label { font-size: 12px; color: #888; text-transform: uppercase; margin-bottom: 8px; }
        .stat-value { font-size: 32px; font-weight: 600; color: #fff; }
        .stat-subtitle { font-size: 14px; color: #666; margin-top: 4px; }
        .charts-grid {
            display: grid;
            grid-template-columns: 2fr 1fr 1fr;
            gap: 16px;
            margin-bottom: 24px;
        }
        .chart-container {
            background: #1a1a1a;
            border: 1px solid #2a2a2a;
            border-radius: 8px;
            padding: 16px;
        }
        .chart-title { font-size: 14px; font-weight: 600; color: #fff; margin-bottom: 12px; }
        .errors-section {
            background: #1a1a1a;
            border: 1px solid #2a2a2a;
            border-radius: 8px;
            padding: 16px;
        }
        .errors-title { font-size: 14px; font-weight: 600; color: #fff; margin-bottom: 12px; }
        .errors-table { width: 100%; border-collapse: collapse; }
        .errors-table th {
            text-align: left; font-size: 12px; color: #888;
            padding: 8px 12px; border-bottom: 1px solid #2a2a2a;
        }
        .errors-table td { font-size: 13px; padding: 8px 12px; border-bottom: 1px solid #2a2a2a; }
        .error-type {
            display: inline-block; padding: 2px 8px;
            background: #7c2d12; color: #fca5a5;
            border-radius: 4px; font-size: 11px; font-weight: 500;
        }
        .no-errors { color: #666; font-size: 14px; padding: 16px; text-align: center; }
        /* Epic 5b (Story 5.2): family card — server-rendered HTML above the
           stat-cards. Status is never color-only: every dot pairs with a
           text label. Paid card is visually distinct (amber); the
           safety-net banner + bypassed border are red WITH a text label. */
        .family-section { margin-bottom: 24px; }
        .family-card {
            background: #1a1a1a;
            border: 1px solid #2a2a2a;
            border-radius: 8px;
            padding: 16px;
            margin-bottom: 16px;
        }
        .family-card.paid-card { border: 1px solid #f59e0b; }
        .family-card.bypassed { border: 1px solid #ef4444; }
        .family-banner {
            border: 1px solid #ef4444;
            border-radius: 6px;
            padding: 12px;
            margin-bottom: 12px;
            font-size: 13px;
            line-height: 1.6;
            color: #e0e0e0;
        }
        .family-banner code {
            display: block;
            margin-top: 6px;
            background: #111;
            border: 1px solid #2a2a2a;
            border-radius: 4px;
            padding: 8px;
            font-size: 12px;
            white-space: pre-wrap;
            word-break: break-all;
            user-select: all;
        }
        .family-label {
            display: inline-block; padding: 2px 8px;
            border-radius: 4px; font-size: 11px; font-weight: 600;
            margin-left: 8px; vertical-align: middle;
        }
        .family-label.paid { background: #3a2a1a; color: #fbbf24; }
        .family-label.bypassed-label { background: #3a1a1a; color: #fca5a5; }
        .family-pick { font-size: 15px; color: #fff; margin: 8px 0; }
        .family-pick #family-pick, .family-pick #family-paid-pick {
            font-family: monospace; user-select: all;
        }
        .family-meta { font-size: 13px; color: #aaa; margin: 4px 0; }
        .family-meta a { color: #3b82f6; }
        .member-id { font-family: monospace; font-size: 12px; user-select: all; }
        tr.member-excluded td { color: #666; }
        .family-note { font-size: 12px; color: #666; margin-top: 12px; line-height: 1.5; }
        @media (max-width: 1024px) { .charts-grid { grid-template-columns: 1fr; } }
    </style>
</head>
<body>
    <div class="header">
        <h1>Claude Proxy</h1>
        <div class="status-bar">
            <div class="provider-status" id="provider-status-bar" style="gap: 16px;"></div>
            <div class="refresh-time" id="refresh-time">Loading...</div>
        </div>
    </div>

    <!-- Epic 5b (Story 5.2) rollout note — Epic 3 task C11 perf budget:
         family resolution overhead must stay p99 <= 1ms at family sizes <= 8.
         Measured 2026-09-12 (dev profile, `cargo test --test family_perf`
         `-- --nocapture`; see tests/family_perf.rs which prints these live):
         rank micro-bench p99 ~= 3.6us, full dispatch-seam resolve_family
         p99 ~= 54us, static-pin baseline p99 ~= 190ns — the seam sits ~20x
         inside the 1ms budget. Gate is green; the family route may be
         enabled. (Epic 6 owns the user-facing rollout doc — coordinate the
         numbers there; this comment is the dashboard-side record.) -->
     <!-- Family card: server-rendered cold-start skeleton. Readable with JS
          disabled or CDN/Chart.js blocked; renderFamily() (called from
          loadMetrics()) only swaps text values in place every 30s
          (no animation reset). -->
    <section class="family-section" id="family-section" aria-label="Model family status">
        <div class="family-card" id="family-card">
            <div class="family-banner" id="family-banner" hidden>
                <span class="family-label bypassed-label">BYPASSED</span>
                <span id="family-banner-text">All auto-coding members unhealthy — bypassed cooldown and served &lt;model-id&gt; at &lt;time&gt;.</span>
                <span> Rollback (copy-paste):</span>
                <code id="family-rollback-curl">curl -X POST http://<span id="rollback-host">localhost:PORT</span>/api/route -H 'Content-Type: application/json' -d '{"name":"default-pinned"}'</code>
                <span>Next retry on healthy member immediately; bypass clears on next healthy resolution.</span>
            </div>
            <div class="chart-title">FAMILY: <span id="family-alias-name">auto-coding</span> (free pool, least-errors first)<span class="family-label bypassed-label" id="family-bypassed-tag" hidden>BYPASSED</span> <a href="/api/route" style="font-weight:normal;font-size:12px;">via GET /api/route</a></div>
            <div class="family-pick" id="family-pick-line" aria-live="polite">Cold start — serving config-order default (<span id="family-pick">loading…</span>) until 20 requests accumulate.</div>
            <div class="family-meta" id="family-why">why: collecting stats — err — · p50 —</div>
            <div class="family-meta" id="family-last-change">last change: — (no previous pick yet)</div>
            <div class="family-meta">stable: challenger needs err &gt;2pp AND p50 &gt;10% to dethrone (hysteresis)</div>
            <div class="family-meta" id="family-window">stats window: last 500 reqs, age 0s / since restart</div>
            <div class="family-meta" id="family-pinned">pinned sessions: 0 (see <a href="/api/sessions">GET /api/sessions</a>)</div>
            <table class="errors-table" style="margin-top:12px;">
                <thead>
                    <tr><th>Member (ranked)</th><th>Err %</th><th>P50</th><th>Status</th></tr>
                </thead>
                <tbody id="family-members-body">
                    <tr><td colspan="4" class="no-errors">Cold start — member stats accumulate after 20 requests (members with no data show —, never 0%)</td></tr>
                </tbody>
            </table>
            <div class="family-note">Session pins take precedence; pick sticks per session, re-evaluates on cooldown/exclusion event or every 50 family resolutions.</div>
        </div>
        <div class="family-card paid-card" id="family-card-paid" hidden>
            <div class="chart-title">FAMILY: auto-coding-paid — PAID, may spend<span class="family-label paid">PAID — may spend</span> <a href="/api/route" style="font-weight:normal;font-size:12px;">via GET /api/route</a></div>
            <div class="family-pick" id="family-paid-pick-line" aria-live="polite">NOW SERVING (copy-pasteable): <span id="family-paid-pick">—</span></div>
            <div class="family-meta" id="family-paid-why">why: —</div>
            <div class="family-meta">paid resolutions: <span id="family-paid-count">0</span> (switch opencode model back to auto-coding or a pin; counter confirms spend stopped)</div>
        </div>
    </section>

    <div class="stats-grid">
        <div class="stat-card">
            <div class="stat-label">Total Requests</div>
            <div class="stat-value" id="total-requests">0</div>
        </div>
        <div class="stat-card">
            <div class="stat-label">Success Rate</div>
            <div class="stat-value" id="success-rate">0%</div>
            <div class="stat-subtitle" id="success-count">0 successful</div>
        </div>
        <div class="stat-card">
            <div class="stat-label">Error Rate</div>
            <div class="stat-value" id="error-rate">0%</div>
            <div class="stat-subtitle" id="error-count">0 errors</div>
        </div>
        <div class="stat-card">
            <div class="stat-label">Fallbacks</div>
            <div class="stat-value" id="fallback-count">0</div>
        </div>
        <div class="stat-card">
            <div class="stat-label">Loop Lag (current)</div>
            <div class="stat-value" id="loop-lag">0ms</div>
            <div class="stat-subtitle" id="loop-lag-status">healthy</div>
        </div>
        <div class="stat-card">
            <div class="stat-label">Tokens Saved</div>
            <div class="stat-value" id="tokens-saved">0</div>
            <div class="stat-subtitle" id="compression-ratio">— compression</div>
        </div>
    </div>

    <div class="charts-grid">
        <div class="chart-container">
            <div class="chart-title">Requests Per Minute (15 min)</div>
            <canvas id="rpm-chart"></canvas>
        </div>
        <div class="chart-container">
            <div class="chart-title">Providers</div>
            <canvas id="provider-chart"></canvas>
        </div>
        <div class="chart-container">
            <div class="chart-title">Duration</div>
            <canvas id="duration-chart"></canvas>
        </div>
    </div>

    <div class="chart-container" style="margin-bottom: 24px;">
        <div class="chart-title">Event Loop Lag — max ms per minute (15 min)</div>
        <canvas id="lag-chart"></canvas>
    </div>

    <div class="chart-container" style="margin-bottom: 24px;">
        <div class="chart-title">Latency by Upstream</div>
        <div class="stats-grid" id="latency-cards" style="margin-top: 12px; margin-bottom: 0;">
            <div class="stat-card"><div class="stat-label">No upstream traffic yet</div></div>
        </div>
    </div>

    <div class="chart-container" style="margin-bottom: 24px;">
        <div class="chart-title">Compression</div>
        <div class="stats-grid" style="margin-top: 12px; margin-bottom: 0;">
            <div class="stat-card">
                <div class="stat-label">Requests Compressed</div>
                <div class="stat-value" id="comp-requests">0</div>
            </div>
            <div class="stat-card">
                <div class="stat-label">Avg Compression Ratio</div>
                <div class="stat-value" id="comp-ratio">0%</div>
            </div>
            <div class="stat-card">
                <div class="stat-label">Total Tokens Before</div>
                <div class="stat-value" id="comp-before">0</div>
            </div>
            <div class="stat-card">
                <div class="stat-label">Total Tokens After</div>
                <div class="stat-value" id="comp-after">0</div>
            </div>
        </div>
        <div id="compression-disabled-notice" style="display:none; color:#888; font-size:13px; padding:8px 0; text-align:center;">
            Compression inactive — no requests compressed yet (or STAPLER_COMPRESS=0)
        </div>
    </div>

    <div class="errors-section" style="margin-bottom: 24px;">
        <div class="errors-title">Recent Requests</div>
        <table class="errors-table">
            <thead>
                <tr>
                    <th>Time</th><th>ID</th><th>Provider</th><th>Model</th>
                    <th>Duration</th><th>TTFT</th><th>Tokens Before → After</th>
                    <th>Saved</th><th>Msgs</th><th>Content Types</th><th>Type</th>
                </tr>
            </thead>
            <tbody id="requests-body">
                <tr><td colspan="11" class="no-errors">No requests yet</td></tr>
            </tbody>
        </table>
    </div>

    <div class="errors-section" style="margin-bottom: 24px;">
        <div class="errors-title">count_tokens Health</div>
        <div class="stats-grid" style="margin-top: 12px; margin-bottom: 0;">
            <div class="stat-card">
                <div class="stat-label">Total Calls</div>
                <div class="stat-value" id="ct-total">0</div>
            </div>
            <div class="stat-card">
                <div class="stat-label">Failures</div>
                <div class="stat-value" id="ct-failures">0</div>
                <div class="stat-subtitle" id="ct-failure-rate">0% failure rate</div>
            </div>
            <div class="stat-card">
                <div class="stat-label">Last Token Count</div>
                <div class="stat-value" id="ct-last-count">—</div>
                <div class="stat-subtitle" id="ct-last-model">—</div>
            </div>
        </div>
        <div style="color:#888;font-size:12px;padding:8px 0;">
            count_tokens drives Claude Code auto-compaction. Failures here prevent compaction from triggering.
        </div>
    </div>

    <div class="errors-section" style="margin-bottom: 24px;">
        <div class="errors-title">Unique Error Types (persistent)</div>
        <table class="errors-table">
            <thead>
                <tr>
                    <th>Fingerprint</th><th>Provider</th><th>Type</th>
                    <th>Count</th><th>First Seen</th><th>Last Seen</th><th>Message</th>
                </tr>
            </thead>
            <tbody id="error-types-body">
                <tr><td colspan="7" class="no-errors">Loading...</td></tr>
            </tbody>
        </table>
    </div>

    <div class="errors-section">
        <div class="errors-title">Recent Errors (in-memory)</div>
        <table class="errors-table">
            <thead>
                <tr><th>Time</th><th>Type</th><th>Provider</th><th>Model</th></tr>
            </thead>
            <tbody id="errors-body">
                <tr><td colspan="4" class="no-errors">No errors yet</td></tr>
            </tbody>
        </table>
    </div>

    <!-- Request body inspection modal -->
    <div id="body-modal" style="display:none;position:fixed;inset:0;background:rgba(0,0,0,0.7);z-index:1000;overflow:auto;" onclick="if(event.target===this)closeModal()">
        <div style="background:#1a1a1a;border:1px solid #333;border-radius:8px;max-width:900px;margin:40px auto;padding:24px;position:relative;">
            <div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:16px;">
                <span id="modal-title" style="font-weight:600;color:#e5e5e5;font-size:14px;"></span>
                <div style="display:flex;gap:8px;align-items:center;">
                    <button id="modal-stage-orig" onclick="switchStage('original')" style="background:#2a2a2a;border:1px solid #444;color:#ccc;cursor:pointer;padding:3px 10px;border-radius:4px;font-size:12px;font-weight:bold;">Original</button>
                    <button id="modal-stage-comp" onclick="switchStage('compressed')" style="background:#2a2a2a;border:1px solid #444;color:#ccc;cursor:pointer;padding:3px 10px;border-radius:4px;font-size:12px;font-weight:normal;">Compressed</button>
                    <button onclick="closeModal()" style="background:#333;border:none;color:#aaa;cursor:pointer;padding:4px 10px;border-radius:4px;font-size:14px;">X</button>
                </div>
            </div>
            <pre id="modal-body" style="background:#111;border:1px solid #2a2a2a;border-radius:6px;padding:16px;overflow:auto;max-height:70vh;font-size:12px;line-height:1.5;color:#d4d4d4;white-space:pre-wrap;word-break:break-all;margin:0;"></pre>
        </div>
    </div>

    <script>
        let rpmChart, providerChart, durationChart, lagChart;

        function initCharts() {
            const chartDefaults = {
                responsive: true,
                maintainAspectRatio: true,
                plugins: { legend: { display: false } }
            };

            rpmChart = new Chart(document.getElementById('rpm-chart'), {
                type: 'line',
                data: { labels: [], datasets: [{ data: [], borderColor: '#3b82f6', tension: 0.4 }] },
                options: {
                    ...chartDefaults,
                    scales: {
                        y: { beginAtZero: true, grid: { color: '#2a2a2a' }, ticks: { color: '#888' } },
                        x: { grid: { display: false }, ticks: { color: '#888' } }
                    }
                }
            });

            providerChart = new Chart(document.getElementById('provider-chart'), {
                type: 'doughnut',
                data: {
                    labels: [],
                    datasets: [{ data: [], backgroundColor: [] }]
                },
                options: {
                    ...chartDefaults,
                    plugins: { legend: { display: true, position: 'bottom', labels: { color: '#888' } } }
                }
            });

            durationChart = new Chart(document.getElementById('duration-chart'), {
                type: 'bar',
                data: {
                    labels: ['< 1s', '1-5s', '5-30s', '30-60s', '> 60s'],
                    datasets: [{ data: [0, 0, 0, 0, 0], backgroundColor: '#3b82f6' }]
                },
                options: {
                    ...chartDefaults,
                    scales: {
                        y: { beginAtZero: true, grid: { color: '#2a2a2a' }, ticks: { color: '#888' } },
                        x: { grid: { display: false }, ticks: { color: '#888' } }
                    }
                }
            });

            lagChart = new Chart(document.getElementById('lag-chart'), {
                type: 'line',
                data: {
                    labels: [],
                    datasets: [
                        { label: 'max', data: [], borderColor: '#ef4444', backgroundColor: 'rgba(239,68,68,0.1)', fill: true, tension: 0.4 },
                        { label: 'avg', data: [], borderColor: '#f59e0b', borderDash: [4, 4], tension: 0.4 }
                    ]
                },
                options: {
                    responsive: true,
                    maintainAspectRatio: true,
                    plugins: {
                        legend: { display: true, position: 'top', labels: { color: '#888' } },
                        tooltip: { callbacks: { label: ctx => ctx.dataset.label + ': ' + ctx.parsed.y.toFixed(2) + 'ms' } }
                    },
                    scales: {
                        y: { beginAtZero: true, grid: { color: '#2a2a2a' }, ticks: { color: '#888', callback: v => v + 'ms' } },
                        x: { grid: { display: false }, ticks: { color: '#888' } }
                    }
                }
            });
        }

        // Epic 5b (Story 5.2): family card text-swap polling. Reads the
        // /metrics `family` section (Epic 5a shape: current_pick,
        // previous_pick, last_change_at, window_age_s, resolutions_total,
        // fallback_to_default_total, paid_resolutions, members[{model,
        // error_rate (fraction), latency_p50_ms, samples, status}]) and swaps
        // text values in place — no animation reset, no chart dependency.
        function fmtP50(ms) {
            if (ms == null) return '—';
            return ms >= 1000 ? (ms / 1000).toFixed(1) + 's' : ms + 'ms';
        }
        function fmtErr(frac) { return (frac * 100).toFixed(1) + '%'; }
        // HTML-escape for the innerHTML builders below (pick line, member
        // table): model IDs come from /metrics and must never break out of
        // markup. Missing values render as '—', never "undefined".
        function esc(s) {
            if (s === undefined || s === null || s === '') return '—';
            return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;')
                .replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&#39;');
        }
        // Edge-detect state for the safety-net banner: the cumulative
        // fallback_to_default_total never resets, so visibility latches on
        // fallback-count growth and clears on the next healthy resolution
        // (resolutions_total advancing with the fallback count unchanged).
        // Keyed per rendered alias (first poll per alias only syncs the
        // baselines WITHOUT latching, so historical bypasses never trip the
        // banner on load). Sticky-traffic note: sticky serves skip
        // resolve_family (no snapshot publish), so under a stuck session the
        // banner can clear up to K requests late — accepted, not a bug.
        let prevFbByAlias = {}, prevResByAlias = {}, seenAlias = {}, bannerVisible = false;
        function familyStatusLabel(status, isPick) {
            if (status === undefined || status === null || status === '') status = 'cold';
            if (status === 'active') return isPick ? '(•) active — serving' : '(•) active';
            if (status === 'cold') return '(•) cold-start-default';
            if (status === 'cooldown') return '(•) cooldown';
            // Epic 5a emits `excluded:denylisted` for 404-denylisted members
            // (1h TTL); surface the ux.md §2.3 copy for that case verbatim.
            if (status === 'excluded:denylisted') return '(•) excluded: 404 (denylisted 1h)';
            return '(•) ' + esc(status);
        }
        function renderFamily(family) {
            if (!family) return;
            const aliases = Object.keys(family);
            if (aliases.length === 0) return;
            const alias = family['auto-coding'] ? 'auto-coding' : aliases[0];
            const entry = family[alias];
            if (!entry) return;
            document.getElementById('family-alias-name').textContent = alias;
            document.getElementById('rollback-host').textContent = location.host;
            const members = entry.members || [];
            const pick = entry.current_pick || (members[0] && members[0].model) || '—';
            const cold = !entry.resolutions_total || members.every(m => m.status === 'cold');
            const anyExcluded = members.some(m => m.status && m.status.indexOf('excluded') === 0);
            const pickEl = document.getElementById('family-pick-line');
            const prev = entry.previous_pick ? ' (previous: ' + entry.previous_pick + ')' : ' (no previous pick yet)';
            if (cold) {
                pickEl.innerHTML = 'Cold start — serving config-order default (<span id="family-pick">' + esc(pick) + '</span>) until 20 requests accumulate.';
            } else {
                let html = 'NOW SERVING (copy-pasteable): <span id="family-pick">' + esc(pick) + '</span>';
                if (anyExcluded) html += ' · dead IDs excluded before dispatch';
                pickEl.innerHTML = html;
            }
            const pickMember = members.find(m => m.model === pick);
            if (pickMember && pickMember.status !== 'cold') {
                document.getElementById('family-why').textContent =
                    'why: err ' + fmtErr(pickMember.error_rate || 0) + ' (' + (pickMember.samples || 0) + ' req) · p50 ' + fmtP50(pickMember.latency_p50_ms || 0);
            } else if (pickMember) {
                document.getElementById('family-why').textContent = 'why: collecting stats — err — · p50 — (cold-start-default)';
            }
            const changeTime = entry.last_change_at ? new Date(entry.last_change_at).toLocaleTimeString() : '—';
            document.getElementById('family-last-change').textContent = 'last change: ' + changeTime + prev;
            const since = entry.last_change_at ? new Date(entry.last_change_at).toLocaleTimeString() : 'restart';
            document.getElementById('family-window').textContent =
                'stats window: last 500 reqs, age ' + (entry.window_age_s || 0) + 's / since ' + since;
            // Safety-net banner: edge-detect on the cumulative counters.
            // A bypass bumps both fallback_to_default_total AND
            // resolutions_total (snapshot publish); a healthy resolution
            // bumps only resolutions_total — so the banner latches when the
            // fallback count grows since last poll and clears once
            // resolutions_total advances with the fallback count unchanged.
            // Until then it names the served pick + time + rollback curl.
            const bypasses = entry.fallback_to_default_total || 0;
            const resTotal = entry.resolutions_total || 0;
            if (!seenAlias[alias]) {
                // First poll for this alias: sync baselines without
                // latching — historical bypasses must not trip the banner.
                seenAlias[alias] = true;
            } else if (bypasses > (prevFbByAlias[alias] || 0)) {
                bannerVisible = true;
            } else if (resTotal > (prevResByAlias[alias] || 0) && bypasses === prevFbByAlias[alias]) {
                bannerVisible = false;
            }
            prevFbByAlias[alias] = bypasses;
            prevResByAlias[alias] = resTotal;
            const banner = document.getElementById('family-banner');
            const card = document.getElementById('family-card');
            const tag = document.getElementById('family-bypassed-tag');
            if (bannerVisible) {
                banner.hidden = false;
                card.classList.add('bypassed');
                tag.hidden = false;
                document.getElementById('family-banner-text').textContent =
                    'All ' + alias + ' members unhealthy — bypassed cooldown and served ' + pick + ' at ' + changeTime + ' (' + bypasses + ' bypass(es) total).';
            } else {
                banner.hidden = true;
                card.classList.remove('bypassed');
                tag.hidden = true;
            }
            // Ranked table: pick first, then active by err/p50, cooldown next,
            // cold then excluded last. Members with no data show —, never 0%.
            const weight = s => s === 'active' ? 0 : s === 'cooldown' ? 1 : s === 'cold' ? 2 : 3;
            const ranked = members.slice().sort((a, b) => {
                if (a.model === pick) return -1;
                if (b.model === pick) return 1;
                const w = weight(a.status) - weight(b.status);
                if (w !== 0) return w;
                if ((a.error_rate || 0) !== (b.error_rate || 0)) return (a.error_rate || 0) - (b.error_rate || 0);
                return (a.latency_p50_ms || 0) - (b.latency_p50_ms || 0);
            });
            const tbody = document.getElementById('family-members-body');
            tbody.innerHTML = ranked.length === 0
                ? '<tr><td colspan="4" class="no-errors">No family members configured</td></tr>'
                : ranked.map(m => {
                    const isPick = m.model === pick;
                    const noData = m.status === 'cold';
                    const cls = (m.status && m.status.indexOf('excluded') === 0) ? ' class="member-excluded"' : '';
                    const dot = m.status === 'active' ? 'status-active' : m.status === 'cooldown' ? 'status-cooldown' : 'status-cold';
                    return '<tr' + cls + '><td class="member-id">' + esc(m.model) + '</td>'
                        + '<td>' + (noData ? '—' : fmtErr(m.error_rate || 0)) + '</td>'
                        + '<td>' + (noData ? '—' : fmtP50(m.latency_p50_ms || 0)) + '</td>'
                        + '<td><span class="status-indicator ' + dot + '"></span> ' + familyStatusLabel(m.status, isPick) + '</td></tr>';
                }).join('');
            // Paid alias renders as a separate distinct card, never merged
            // into the free card.
            const paid = family['auto-coding-paid'];
            const paidCard = document.getElementById('family-card-paid');
            if (paid) {
                paidCard.hidden = false;
                document.getElementById('family-paid-pick').textContent = paid.current_pick || '—';
                const pm = (paid.members || []).find(m => m.model === paid.current_pick);
                document.getElementById('family-paid-why').textContent = pm && pm.status !== 'cold'
                    ? 'why: err ' + fmtErr(pm.error_rate || 0) + ' (' + (pm.samples || 0) + ' req) · p50 ' + fmtP50(pm.latency_p50_ms || 0)
                    : 'why: collecting stats — err — · p50 — (cold-start-default)';
                document.getElementById('family-paid-count').textContent = paid.paid_resolutions || 0;
            } else {
                paidCard.hidden = true;
            }
        }
        // Pinned-session count (Epic 4.2 sessions-view skip: the card links
        // GET /api/sessions instead of rendering a sessions view). Counts
        // sessions with a live pin/override; best-effort — leaves the
        // server-rendered default on fetch failure.
        async function loadFamilySessions() {
            try {
                const data = await fetch('/api/sessions').then(r => r.json());
                const pinned = (data.sessions || []).filter(s => s.override).length;
                document.getElementById('family-pinned').innerHTML =
                    'pinned sessions: ' + pinned + ' (see <a href="/api/sessions">GET /api/sessions</a>)';
            } catch (e) {
                console.error('Failed to load family sessions:', e);
            }
        }

        async function loadMetrics() {
            try {
                const response = await fetch('/metrics');
                const data = await response.json();

                document.getElementById('total-requests').textContent = data.summary.total_requests.toLocaleString();
                document.getElementById('success-rate').textContent = data.summary.success_rate.toFixed(1) + '%';
                document.getElementById('success-count').textContent = data.summary.total_success.toLocaleString() + ' successful';
                document.getElementById('error-rate').textContent = data.summary.error_rate.toFixed(1) + '%';
                document.getElementById('error-count').textContent = data.summary.total_errors.toLocaleString() + ' errors';
                document.getElementById('fallback-count').textContent = data.summary.total_fallbacks.toLocaleString();

                const upstreamNames = Object.keys(data.providers || {});
                const displayName = n => n.charAt(0).toUpperCase() + n.slice(1);
                const palette = ['#3b82f6', '#10b981', '#f59e0b', '#8b5cf6', '#ef4444', '#14b8a6', '#eab308', '#ec4899'];

                const statusBar = document.getElementById('provider-status-bar');
                statusBar.innerHTML = upstreamNames.length === 0
                    ? '<span style="color:#666;font-size:14px;">No upstream traffic yet</span>'
                    : upstreamNames.map(name => {
                        const cd = (data.cooldowns && data.cooldowns[name]) || {};
                        const cooling = cd.cooling_down && cd.remaining_seconds > 0;
                        const lastKind = (data.providers[name] || {}).last_error_kind;
                        const cls = lastKind === 'auth' ? 'status-auth-required'
                            : lastKind === 'response_shape_mismatch' ? 'status-schema-drift'
                            : cooling ? 'status-cooldown'
                            : 'status-active';
                        const suffix = cls === 'status-auth-required' ? ' (needs re-auth)'
                            : cls === 'status-schema-drift' ? ' (schema drift — code fix needed)'
                            : cooling ? ' (' + cd.remaining_seconds + 's)'
                            : '';
                        const label = displayName(name) + suffix;
                        return '<span style="display:flex;align-items:center;gap:8px;">'
                            + '<span class="status-indicator ' + cls + '"></span><span>' + label + '</span></span>';
                    }).join('');

                if (rpmChart && data.rpm_data) {
                    rpmChart.data.labels = data.rpm_data.map(d => d.minute);
                    rpmChart.data.datasets[0].data = data.rpm_data.map(d => d.requests);
                    rpmChart.update();
                }

                if (providerChart) {
                    providerChart.data.labels = upstreamNames.map(displayName);
                    providerChart.data.datasets[0].data = upstreamNames.map(n => data.providers[n].requests || 0);
                    providerChart.data.datasets[0].backgroundColor = upstreamNames.map((_, i) => palette[i % palette.length]);
                    providerChart.update();
                }

                if (durationChart && data.duration_distribution) {
                    const dist = data.duration_distribution;
                    durationChart.data.datasets[0].data = [
                        dist['< 1s'] || 0, dist['1-5s'] || 0, dist['5-30s'] || 0,
                        dist['30-60s'] || 0, dist['> 60s'] || 0
                    ];
                    durationChart.update();
                }

                const lagMs = data.current_lag_ms || 0;
                const lagEl = document.getElementById('loop-lag');
                const lagStatus = document.getElementById('loop-lag-status');
                lagEl.textContent = lagMs.toFixed(1) + 'ms';
                if (lagMs >= 50) { lagEl.style.color = '#ef4444'; lagStatus.textContent = 'contended'; }
                else if (lagMs >= 10) { lagEl.style.color = '#f59e0b'; lagStatus.textContent = 'elevated'; }
                else { lagEl.style.color = '#10b981'; lagStatus.textContent = 'healthy'; }

                if (lagChart && data.lag_data) {
                    lagChart.data.labels = data.lag_data.map(d => d.minute);
                    lagChart.data.datasets[0].data = data.lag_data.map(d => d.max_ms);
                    lagChart.data.datasets[1].data = data.lag_data.map(d => d.avg_ms);
                    lagChart.update();
                }

                if (data.compression) {
                    const c = data.compression;
                    const saved = c.total_tokens_saved || 0;
                    const ratio = c.avg_compression_ratio || 0;
                    const requests = c.total_requests_compressed || 0;
                    document.getElementById('tokens-saved').textContent = saved.toLocaleString();
                    document.getElementById('compression-ratio').textContent =
                        ratio > 0 ? (ratio * 100).toFixed(1) + '% avg saved' : '— compression';
                    document.getElementById('comp-requests').textContent = requests.toLocaleString();
                    document.getElementById('comp-ratio').textContent = ratio > 0 ? (ratio * 100).toFixed(1) + '%' : '0%';
                    document.getElementById('comp-before').textContent = (c.total_tokens_before || 0).toLocaleString();
                    document.getElementById('comp-after').textContent = (c.total_tokens_after || 0).toLocaleString();
                    document.getElementById('compression-disabled-notice').style.display = requests === 0 ? 'block' : 'none';
                }

                const requestsBody = document.getElementById('requests-body');
                if (data.recent_requests && data.recent_requests.length > 0) {
                    const abbrevModel = m => {
                        if (!m || m === 'unknown') return m || '—';
                        if (m.includes('opus')) return 'opus';
                        if (m.includes('sonnet')) return 'sonnet';
                        if (m.includes('haiku')) return 'haiku';
                        return m.split('-').slice(-1)[0] || m;
                    };
                    const fmtMs = ms => !ms ? '—' : ms >= 1000 ? (ms/1000).toFixed(1)+'s' : Math.round(ms)+'ms';
                    const provColor = p => p === 'anthropic' ? '#1e3a5f' : p === 'bedrock' ? '#1a3a2a' : '#2a2a2a';
                    const fmtTypes = (json, cm) => {
                        if (!json) return '—';
                        try {
                            const t = JSON.parse(json);
                            const abbrevs = {text:'T', tool_use:'TU', tool_result:'TR', image:'IMG', document:'DOC', search_result:'SR'};
                            // Keys ride client message content — escape.
                            const parts = Object.entries(t).map(([k,v]) => esc(abbrevs[k]||k)+':'+esc(v));
                            const cmBadge = cm ? ' <span class="error-type" style="background:#3a2a1a;font-size:10px">CM</span>' : '';
                            return '<span style="font-size:11px;color:#aaa">' + parts.join(' ') + '</span>' + cmBadge;
                        } catch { return json; }
                    };
                    requestsBody.innerHTML = data.recent_requests.slice(0, 20).map(r => {
                        const time = new Date(r.timestamp).toLocaleTimeString();
                        const saved = r.tokens_before > 0 ? r.tokens_before - r.tokens_after : 0;
                        const pct = r.tokens_before > 0 ? ((saved / r.tokens_before) * 100).toFixed(1) + '%' : '—';
                        const tokStr = r.compressed
                            ? r.tokens_before.toLocaleString() + ' → ' + r.tokens_after.toLocaleString()
                            : r.tokens_before.toLocaleString();
                        const typeLabel = r.stream
                            ? '<span class="error-type" style="background:#1e3a5f">stream</span>'
                            : '<span class="error-type" style="background:#1a3a1a">sync</span>';
                        // r.provider / r.model are client-controlled (via
                        // the request body) — escape every interpolation.
                        // The row click carries its args in data-attributes
                        // (never a string-spliced onclick handler).
                        const provLabel = r.provider && r.provider !== 'unknown'
                            ? '<span class="error-type" style="background:' + provColor(r.provider) + '">' + esc(r.provider) + '</span>'
                            : '—';
                        const ttft = r.bedrock_first_byte_ms > 0 ? fmtMs(r.bedrock_first_byte_ms) : fmtMs(r.first_byte_ms);
                        return '<tr style="cursor:pointer" data-req="' + esc(r.request_id) + '" data-model="' + esc(r.model) + '" data-time="' + esc(time) + '">'
                            + '<td>' + time + '</td>'
                            + '<td style="font-family:monospace;font-size:11px">' + esc(r.request_id) + '</td>'
                            + '<td>' + provLabel + '</td>'
                            + '<td>' + esc(abbrevModel(r.model)) + '</td>'
                            + '<td style="font-family:monospace">' + fmtMs(r.duration_ms) + '</td>'
                            + '<td style="font-family:monospace">' + ttft + '</td>'
                            + '<td style="font-family:monospace">' + tokStr + '</td>'
                            + '<td>' + (r.compressed ? pct : '—') + '</td>'
                            + '<td style="font-family:monospace">' + (r.message_count || '—') + '</td>'
                            + '<td>' + fmtTypes(r.msg_types, r.has_context_management) + '</td>'
                            + '<td>' + typeLabel + '</td>'
                            + '</tr>';
                    }).join('');
                    // Row clicks via delegation-safe listeners on the
                    // data-attributes above (see the onclick note).
                    requestsBody.querySelectorAll('tr[data-req]').forEach(tr => {
                        tr.addEventListener('click', () => showRequestBody(tr.dataset.req, tr.dataset.model, tr.dataset.time));
                    });
                } else {
                    requestsBody.innerHTML = '<tr><td colspan="11" class="no-errors">No requests yet</td></tr>';
                }

                const latencyCards = document.getElementById('latency-cards');
                const latencyNames = Object.keys(data.provider_latency || {});
                if (latencyNames.length === 0) {
                    latencyCards.innerHTML = '<div class="stat-card"><div class="stat-label">No upstream traffic yet</div></div>';
                } else {
                    const fmtMs2 = ms => ms > 0 ? (ms >= 1000 ? (ms/1000).toFixed(1)+'s' : ms+'ms') : '—';
                    latencyCards.innerHTML = latencyNames.map(name => {
                        const pl = data.provider_latency[name];
                        const label = esc(displayName(name));
                        return '<div class="stat-card">'
                            + '<div class="stat-label">' + label + ' Avg Duration</div>'
                            + '<div class="stat-value">' + fmtMs2(pl.avg_duration_ms || 0) + '</div>'
                            + '<div class="stat-subtitle">' + (pl.requests || 0).toLocaleString() + ' requests</div>'
                            + '</div>'
                            + '<div class="stat-card">'
                            + '<div class="stat-label">' + label + ' Avg TTFT</div>'
                            + '<div class="stat-value">' + fmtMs2(pl.avg_first_byte_ms || 0) + '</div>'
                            + '</div>';
                    }).join('');
                }

                const errorsBody = document.getElementById('errors-body');
                if (data.recent_errors && data.recent_errors.length > 0) {
                    errorsBody.innerHTML = data.recent_errors.map(err => {
                        const time = new Date(err.timestamp).toLocaleTimeString();
                        // err.provider / err.model are client-controlled —
                        // escape every interpolation.
                        return '<tr>'
                            + '<td>' + time + '</td>'
                            + '<td><span class="error-type">' + esc(err.error_type) + '</span></td>'
                            + '<td>' + esc(err.provider) + '</td>'
                            + '<td>' + esc(err.model) + '</td>'
                            + '</tr>';
                    }).join('');
                } else {
                    errorsBody.innerHTML = '<tr><td colspan="4" class="no-errors">No errors yet</td></tr>';
                }

                if (data.count_tokens) {
                    const ct = data.count_tokens;
                    document.getElementById('ct-total').textContent = ct.total.toLocaleString();
                    document.getElementById('ct-failures').textContent = ct.failures.toLocaleString();
                    document.getElementById('ct-failure-rate').textContent = (ct.failure_rate * 100).toFixed(1) + '% failure rate';
                    if (ct.failures > 0) document.getElementById('ct-failures').style.color = '#ef4444';
                    document.getElementById('ct-last-count').textContent = ct.last_count > 0 ? ct.last_count.toLocaleString() : '—';
                    document.getElementById('ct-last-model').textContent = ct.last_model || '—';
                }

                // Family card text-swap on the same 30s poll (no extra fetch;
                // sessions count rides its own poll in loadFamilySessions).
                renderFamily(data.family);

                document.getElementById('refresh-time').textContent = '↺ ' + new Date().toLocaleTimeString();
            } catch (error) {
                console.error('Failed to load metrics:', error);
            }
        }

        function closeModal() { document.getElementById('body-modal').style.display = 'none'; }
        document.addEventListener('keydown', e => { if (e.key === 'Escape') closeModal(); });

        let _modalRequestId = null;
        let _modalStage = 'original';

        async function fetchAndRenderBody() {
            document.getElementById('modal-body').textContent = 'Loading…';
            try {
                const resp = await fetch('/requests/' + encodeURIComponent(_modalRequestId) + '?stage=' + _modalStage);
                if (!resp.ok) {
                    document.getElementById('modal-body').textContent = _modalStage === 'compressed'
                        ? '(no compressed snapshot — compression may have been skipped)'
                        : 'Not found or evicted from ring buffer';
                    return;
                }
                const data = await resp.json();
                document.getElementById('modal-body').textContent = JSON.stringify(data, null, 2);
            } catch (e) {
                document.getElementById('modal-body').textContent = 'Error: ' + e.message;
            }
        }

        async function showRequestBody(requestId, model, time) {
            _modalRequestId = requestId;
            _modalStage = 'original';
            document.getElementById('modal-stage-orig').style.fontWeight = 'bold';
            document.getElementById('modal-stage-comp').style.fontWeight = 'normal';
            document.getElementById('modal-title').textContent = '[' + requestId + '] ' + model + ' — ' + time;
            document.getElementById('body-modal').style.display = 'block';
            await fetchAndRenderBody();
        }

        async function switchStage(stage) {
            _modalStage = stage;
            document.getElementById('modal-stage-orig').style.fontWeight = stage === 'original' ? 'bold' : 'normal';
            document.getElementById('modal-stage-comp').style.fontWeight = stage === 'compressed' ? 'bold' : 'normal';
            await fetchAndRenderBody();
        }

        async function loadErrorTypes() {
            try {
                const data = await fetch('/errors/summary').then(r => r.json());
                const tbody = document.getElementById('error-types-body');
                if (data.errors && data.errors.length > 0) {
                    tbody.innerHTML = data.errors.map(e => {
                        const first = new Date(e.first_seen).toLocaleString();
                        const last = new Date(e.last_seen).toLocaleString();
                        const fp = e.fingerprint.substring(0, 8);
                        const msg = e.message.length > 80 ? e.message.substring(0, 80) + '…' : e.message;
                        // e.provider / e.message carry upstream error text —
                        // escape every interpolation including the title attr.
                        return '<tr>'
                            + '<td style="font-family:monospace;font-size:11px;">' + esc(fp) + '</td>'
                            + '<td>' + esc(e.provider) + '</td>'
                            + '<td><span class="error-type">' + esc(e.error_type) + '</span></td>'
                            + '<td>' + e.count + '</td>'
                            + '<td style="font-size:11px;">' + first + '</td>'
                            + '<td style="font-size:11px;">' + last + '</td>'
                            + '<td style="font-size:11px;max-width:300px;word-break:break-word;" title="' + esc(e.message) + '">' + esc(msg) + '</td>'
                            + '</tr>';
                    }).join('');
                } else {
                    tbody.innerHTML = '<tr><td colspan="7" class="no-errors">No errors recorded yet</td></tr>';
                }
            } catch (error) {
                console.error('Failed to load error types:', error);
            }
        }

        // Progressive enhancement: the family card above is server-rendered
        // HTML, so it stays readable when Chart.js fails to load — guard the
        // chart init (and every chart update above) so a blocked CDN never
        // kills the metrics/family text polling.
        if (typeof Chart !== 'undefined') {
            try { initCharts(); } catch (e) { console.error('Chart init failed:', e); }
        }
        loadMetrics();
        loadFamilySessions();
        loadErrorTypes();
        setInterval(loadMetrics, 30000);
        setInterval(loadFamilySessions, 30000);
        setInterval(loadErrorTypes, 60000);
    </script>
</body>
</html>"#;

/// `GET /dashboard` — serve the monitoring dashboard HTML page.
// Kept `async` for signature symmetry with the other Axum route handlers in
// `main.rs`'s router, even though this one never awaits anything.
#[allow(clippy::unused_async)]
pub async fn handle_dashboard() -> impl IntoResponse {
    Html(DASHBOARD_HTML)
}

#[cfg(test)]
mod tests {
    use super::DASHBOARD_HTML;

    #[test]
    fn no_upstream_is_hardcoded_by_name() {
        for hardcoded in [
            "anthropic-status",
            "bedrock-status",
            "anthropic-text",
            "bedrock-text",
            "lat-anthropic-dur",
            "lat-bedrock-dur",
        ] {
            assert!(
                !DASHBOARD_HTML.contains(hardcoded),
                "dashboard must not hardcode a specific upstream's id ({hardcoded}) — \
                 provider/latency sections must render dynamically from /metrics"
            );
        }
    }

    #[test]
    fn dynamic_containers_present() {
        for id in ["provider-status-bar", "latency-cards"] {
            assert!(
                DASHBOARD_HTML.contains(id),
                "missing dynamic container #{id}"
            );
        }
    }

    // ── Story 1.5.2 (REQ-12): three-way error-state classification ────────
    //
    // These are lint-level regression guards on the embedded JS *text* (string
    // matching/position checks on `DASHBOARD_HTML`), not behavioral proof —
    // no JS runtime ever executes this code in these tests.

    /// Extracts the `const cls = ...;` ternary chain from `DASHBOARD_HTML`'s
    /// JS, so tests can make position/content assertions on just that block
    /// instead of the whole page string.
    #[allow(clippy::expect_used)]
    fn extract_cls_block() -> &'static str {
        let start = DASHBOARD_HTML
            .find("const cls = lastKind")
            .expect("cls ternary must exist in loadMetrics()'s JS");
        let after = &DASHBOARD_HTML[start..];
        let end = after
            .find(";\n")
            .expect("cls ternary must be terminated by a semicolon");
        &after[..end]
    }

    #[test]
    fn dashboard_html_should_render_status_auth_required_class_and_css_rule() {
        assert!(
            DASHBOARD_HTML.contains(".status-auth-required { background: #ef4444; }"),
            "missing .status-auth-required CSS rule"
        );
        assert!(
            extract_cls_block().contains("'status-auth-required'"),
            "cls ternary must be able to produce 'status-auth-required'"
        );
    }

    #[test]
    fn dashboard_html_should_render_status_schema_drift_class_and_css_rule() {
        assert!(
            DASHBOARD_HTML.contains(".status-schema-drift { background: #8b5cf6; }"),
            "missing .status-schema-drift CSS rule"
        );
        assert!(
            extract_cls_block().contains("'status-schema-drift'"),
            "cls ternary must be able to produce 'status-schema-drift'"
        );
    }

    /// Regression guard (REQ-12, adversarial-review finding): the design
    /// correction in plan.md Story 1.5.2 requires `last_error_kind` to be
    /// checked BEFORE `cooling`, since a real auth failure never trips
    /// `cooling_down`. If a future edit re-introduces the
    /// `cooling ? ... : 'status-active'` binary and gates the new classes
    /// behind it, this must fail loudly.
    #[test]
    #[allow(clippy::expect_used)]
    fn dashboard_js_status_logic_should_check_last_error_kind_before_cooling() {
        let block = extract_cls_block();
        let auth_pos = block
            .find("lastKind === 'auth' ?")
            .expect("cls ternary must check lastKind === 'auth' first");
        let cooling_pos = block
            .find("cooling ?")
            .expect("cls ternary must still fall back to a cooling check");
        assert!(
            auth_pos < cooling_pos,
            "last_error_kind must be checked BEFORE cooling — auth errors never trip \
             cooling_down, so gating status-auth-required behind `cooling` would silently \
             hide it (see plan.md Story 1.5.2's design-correction note)"
        );
    }

    /// Cold-start non-regression (UX §5 item 7): no `last_error_kind` key
    /// and `cooling: false` must never render anything but `status-active`.
    #[test]
    #[allow(clippy::expect_used)]
    fn dashboard_js_should_render_status_active_on_cold_start_with_no_last_error_kind_and_not_cooling(
    ) {
        let block = extract_cls_block();
        // Structural rather than exact-suffix: proves 'status-active' is
        // positioned AFTER the `cooling ?` branch (i.e. it's the ternary's
        // final fallback, reached only when lastKind matches neither special
        // case and cooling is falsy) without coupling to the source's exact
        // formatting/whitespace.
        let cooling_pos = block
            .find("cooling ?")
            .expect("cls ternary must check cooling");
        let active_pos = block
            .find("'status-active'")
            .expect("cls ternary must be able to produce 'status-active'");
        assert!(
            active_pos > cooling_pos,
            "'status-active' must be the ternary's final fallback branch, after the cooling \
             check — not an earlier alternative"
        );
        assert!(
            DASHBOARD_HTML.contains("(data.providers[name] || {}).last_error_kind"),
            "missing-provider-entry lookup must be guarded so a cold-start upstream with no \
             /metrics data never throws or misclassifies"
        );
    }

    /// Non-regression: an ordinary self-healing cooldown (e.g.
    /// `last_error_kind: \"rate_limited\"`, `cooling: true`) must still
    /// render the existing amber `status-cooldown`, unaffected by the two
    /// new classes.
    #[test]
    fn dashboard_js_should_still_render_status_cooldown_for_rate_limited_kind_when_cooling_true() {
        let block = extract_cls_block();
        assert!(
            block.contains("cooling ? 'status-cooldown'"),
            "a cooling upstream whose last_error_kind is neither 'auth' nor \
             'response_shape_mismatch' (e.g. \"rate_limited\") must still render \
             status-cooldown"
        );
    }

    /// UX §5 item 8: color is never the only signal — both new classes must
    /// pair with a distinct, non-overlapping text suffix.
    #[test]
    fn dashboard_js_new_status_classes_should_each_pair_with_a_distinct_text_suffix() {
        assert!(
            DASHBOARD_HTML.contains("' (needs re-auth)'"),
            "status-auth-required must pair with a '(needs re-auth)' text suffix"
        );
        assert!(
            DASHBOARD_HTML.contains("' (schema drift — code fix needed)'"),
            "status-schema-drift must pair with a '(schema drift — code fix needed)' text suffix"
        );
        assert!(
            !"(needs re-auth)".contains("(schema drift — code fix needed)")
                && !"(schema drift — code fix needed)".contains("(needs re-auth)"),
            "the two suffixes must be non-overlapping (UX §5 item 2)"
        );
    }

    /// UX §5 item 9 / plan.md's own convention: adding the two new classes
    /// must not introduce any upstream-name-specific string into
    /// `DASHBOARD_HTML` — the classification stays driven by
    /// `last_error_kind`/`cooling`, generically, for every upstream.
    #[test]
    fn no_upstream_is_hardcoded_by_name_should_continue_to_pass_unmodified_after_gemini_additions()
    {
        for hardcoded in [
            "gemini-status",
            "gemini-text",
            "lat-gemini-dur",
            "\"gemini\"",
        ] {
            assert!(
                !DASHBOARD_HTML.contains(hardcoded),
                "dashboard must not hardcode Gemini's name ({hardcoded}) — the new status \
                 classes must be driven by last_error_kind/cooling generically"
            );
        }
    }

    // ── Epic 5b (Story 5.2): family card lint-level guards ──────────────
    //
    // Same convention as the Story 1.5.2 guards above: string
    // matching/position checks on `DASHBOARD_HTML`, not behavioral proof —
    // no JS runtime executes this code in these tests.

    #[test]
    #[allow(clippy::expect_used)]
    fn family_card_should_render_above_stat_cards() {
        let card = DASHBOARD_HTML
            .find("id=\"family-section\"")
            .expect("family section must exist");
        let stats = DASHBOARD_HTML
            .find("class=\"stats-grid\"")
            .expect("stat-cards grid must exist");
        assert!(
            card < stats,
            "family card must render above the stat-cards (ux.md §2 glance)"
        );
    }

    #[test]
    fn family_pick_line_should_use_polite_live_region() {
        assert!(
            DASHBOARD_HTML.contains("id=\"family-pick-line\" aria-live=\"polite\""),
            "pick line must carry aria-live=\"polite\" for 30s polling (ux.md §2.3)"
        );
        assert!(
            DASHBOARD_HTML.contains("id=\"family-paid-pick-line\" aria-live=\"polite\""),
            "paid pick line must carry aria-live=\"polite\" too"
        );
    }

    #[test]
    fn family_card_should_render_cold_start_copy_with_never_zero_percent() {
        assert!(
            DASHBOARD_HTML.contains("Cold start — serving config-order default"),
            "missing ux.md §2.3 cold-start copy template"
        );
        assert!(
            DASHBOARD_HTML.contains("cold-start-default"),
            "missing cold-start-default status label"
        );
        assert!(
            DASHBOARD_HTML.contains("never 0%"),
            "card must state members with no data show —, never 0%"
        );
    }

    #[test]
    fn family_banner_should_carry_copy_pasteable_rollback_with_exact_route_name() {
        assert!(
            DASHBOARD_HTML.contains("bypassed cooldown and served"),
            "missing ux.md §2.1 safety-net banner copy"
        );
        assert!(
            DASHBOARD_HTML.contains("default-pinned"),
            "banner rollback curl must name the exact route default-pinned"
        );
        assert!(
            DASHBOARD_HTML.contains("BYPASSED"),
            "bypass must pair the red border with a BYPASSED text label (no color-only)"
        );
    }

    #[test]
    fn family_paid_card_should_stand_apart_with_counter() {
        assert!(
            DASHBOARD_HTML.contains("PAID — may spend"),
            "paid card must carry the distinct PAID — may spend label"
        );
        assert!(
            DASHBOARD_HTML.contains("paid resolutions:"),
            "paid card must show the paid resolutions counter"
        );
        assert!(
            DASHBOARD_HTML.contains("family-card-paid"),
            "paid card must be a separate card (never merged into the free card)"
        );
    }

    #[test]
    fn family_card_should_link_route_and_sessions_apis() {
        assert!(
            DASHBOARD_HTML.contains("href=\"/api/route\""),
            "card header must link GET /api/route (rollback path, no editing UI)"
        );
        assert!(
            DASHBOARD_HTML.contains("href=\"/api/sessions\""),
            "card must link GET /api/sessions (Epic 4.2 sessions-view skip)"
        );
        assert!(
            DASHBOARD_HTML.contains("pinned sessions:"),
            "card must show the pinned-session count"
        );
    }

    #[test]
    fn family_status_rows_should_pair_every_dot_with_text() {
        // Grayscale-legibility: the JS label fn must emit a text label for
        // every status the /metrics family section can carry.
        for label in [
            "(•) active — serving",
            "(•) active",
            "(•) cold-start-default",
            "(•) cooldown",
            "(•) excluded: 404 (denylisted 1h)",
        ] {
            assert!(
                DASHBOARD_HTML.contains(label),
                "missing dot+text status label: {label}"
            );
        }
    }

    #[test]
    fn family_card_should_poll_text_swap_on_30s_cadence() {
        assert!(
            DASHBOARD_HTML.contains("renderFamily(data.family)"),
            "loadMetrics() must text-swap the family card from the /metrics family section on its 30s poll"
        );
        assert!(
            DASHBOARD_HTML.contains("setInterval(loadFamilySessions, 30000)"),
            "pinned-session count must poll on a ≤30s cadence"
        );
    }

    #[test]
    fn family_card_should_survive_blocked_chart_cdn() {
        assert!(
            DASHBOARD_HTML.contains("typeof Chart !== 'undefined'"),
            "chart init must be guarded so a blocked CDN never kills the family/metrics text polling"
        );
        assert!(
            DASHBOARD_HTML.contains("if (rpmChart && data.rpm_data)"),
            "chart updates must be guarded for the CDN-blocked path"
        );
    }

    #[test]
    fn family_card_should_show_hysteresis_window_and_tie_lines() {
        assert!(
            DASHBOARD_HTML.contains("err &gt;2pp AND p50 &gt;10% to dethrone (hysteresis)"),
            "missing ux.md §2.3 tie/flap stability line"
        );
        assert!(
            DASHBOARD_HTML.contains("stats window: last 500 reqs, age"),
            "missing ux.md §2.3 stale-stats window-age line (reads real window_age_s)"
        );
        assert!(
            DASHBOARD_HTML.contains("dead IDs excluded before dispatch"),
            "missing ux.md §2.3 delisted-member pick-line note"
        );
        assert!(
            DASHBOARD_HTML.contains("Session pins take precedence; pick sticks per session"),
            "missing session stickiness footer (K=50 re-evaluation)"
        );
    }

    #[test]
    fn family_pick_and_member_cells_should_escape_model_ids() {
        // XSS guard: model IDs from /metrics flow into innerHTML builders,
        // so an esc() helper must exist and be applied at the pick builder
        // and the member-table model cell. Previous_pick/banner ride
        // textContent and stay unescaped by design.
        assert!(
            DASHBOARD_HTML.contains("function esc(s)"),
            "missing esc() HTML-escape helper"
        );
        for entity in ["&amp;", "&lt;", "&gt;", "&quot;", "&#39;"] {
            assert!(
                DASHBOARD_HTML.contains(entity),
                "esc() must escape to {entity}"
            );
        }
        assert!(
            DASHBOARD_HTML.contains("esc(pick)"),
            "pick-line innerHTML builder must escape the pick"
        );
        assert!(
            DASHBOARD_HTML.contains("esc(m.model)"),
            "member-table model cell must escape m.model"
        );
        assert!(
            !DASHBOARD_HTML.contains("'(•) ' + status"),
            "familyStatusLabel fallback must not concatenate raw status (use esc)"
        );
    }

    #[test]
    fn family_banner_should_edge_detect_instead_of_cumulative_latch() {
        // Bypass latch guard: cumulative fallback_to_default_total never
        // resets, so visibility must edge-detect (latch on fallback-count
        // growth, clear once resolutions_total advances with the fallback
        // count unchanged) — never `if (bypasses > 0)`. State is keyed per
        // rendered alias, and the first poll per alias only syncs baselines
        // without latching (historical bypasses must not trip the banner).
        assert!(
            DASHBOARD_HTML.contains("bypasses > (prevFbByAlias[alias]"),
            "banner must latch when the fallback count grows since last poll"
        );
        assert!(
            DASHBOARD_HTML.contains("resTotal > (prevResByAlias[alias]"),
            "banner must clear once resolutions advance with fallback count unchanged"
        );
        assert!(
            DASHBOARD_HTML.contains("seenAlias[alias]"),
            "first poll per alias must sync baselines without latching"
        );
        assert!(
            DASHBOARD_HTML.contains("if (bannerVisible)"),
            "banner visibility must come from the edge-detect latch"
        );
        assert!(
            !DASHBOARD_HTML.contains("if (bypasses > 0)"),
            "banner must not latch on the never-resetting cumulative count"
        );
    }

    #[test]
    fn family_card_should_fall_back_to_cold_never_undefined() {
        assert!(
            DASHBOARD_HTML.contains("status = 'cold'"),
            "familyStatusLabel(undefined) must fall back to 'cold'"
        );
        assert!(
            DASHBOARD_HTML.contains("(previous: '"),
            "tie line must render `previous:` per ux.md §2.3"
        );
        assert!(
            !DASHBOARD_HTML.contains("(prev: '"),
            "stale `(prev:)` copy must not remain"
        );
        assert!(
            DASHBOARD_HTML.contains("if (ms == null)"),
            "fmtP50 must use a null check so p50 0 renders 0ms, not —"
        );
        assert!(
            !DASHBOARD_HTML.contains("loadFamily()"),
            "stale loadFamily() reference must not remain (family rides loadMetrics())"
        );
    }

    #[test]
    fn family_card_should_record_perf_budget_rollout_note() {
        assert!(
            DASHBOARD_HTML.contains("p99 <= 1ms"),
            "dashboard must record the Epic 3 C11 1ms resolution-overhead budget"
        );
        assert!(
            DASHBOARD_HTML.contains("tests/family_perf.rs"),
            "rollout note must point at the benchmark that prints the measured numbers"
        );
    }
}
