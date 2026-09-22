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
        <div class="errors-title">Model Token Metrics & Statistics</div>
        <table class="errors-table">
            <thead>
                <tr>
                    <th>Model</th><th>Requests</th><th>Input Tokens</th>
                    <th>Output Tokens</th><th>Total Tokens</th><th>Errors</th><th>Rate Limits</th>
                </tr>
            </thead>
            <tbody id="models-body">
                <tr><td colspan="7" class="no-errors">No model traffic recorded yet</td></tr>
            </tbody>
        </table>
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

                if (data.rpm_data) {
                    rpmChart.data.labels = data.rpm_data.map(d => d.minute);
                    rpmChart.data.datasets[0].data = data.rpm_data.map(d => d.requests);
                    rpmChart.update();
                }

                providerChart.data.labels = upstreamNames.map(displayName);
                providerChart.data.datasets[0].data = upstreamNames.map(n => data.providers[n].requests || 0);
                providerChart.data.datasets[0].backgroundColor = upstreamNames.map((_, i) => palette[i % palette.length]);
                providerChart.update();

                if (data.duration_distribution) {
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

                if (data.lag_data) {
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

                const modelsBody = document.getElementById('models-body');
                if (data.models && Object.keys(data.models).length > 0) {
                    modelsBody.innerHTML = Object.entries(data.models).map(([model, m]) =>
                        '<tr>'
                        + '<td><code>' + model + '</code></td>'
                        + '<td>' + (m.requests || 0).toLocaleString() + '</td>'
                        + '<td>' + (m.input_tokens || 0).toLocaleString() + '</td>'
                        + '<td>' + (m.output_tokens || 0).toLocaleString() + '</td>'
                        + '<td><strong>' + (m.total_tokens || 0).toLocaleString() + '</strong></td>'
                        + '<td>' + (m.errors > 0 ? '<span class="error-type">' + m.errors + '</span>' : '0') + '</td>'
                        + '<td>' + (m.rate_limits > 0 ? '<span class="error-type" style="background:#4a2a1a;color:#fca5a5">' + m.rate_limits + '</span>' : '0') + '</td>'
                        + '</tr>'
                    ).join('');
                } else {
                    modelsBody.innerHTML = '<tr><td colspan="7" class="no-errors">No model traffic recorded yet</td></tr>';
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
                            const parts = Object.entries(t).map(([k,v]) => (abbrevs[k]||k)+':'+v);
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
                        const provLabel = r.provider && r.provider !== 'unknown'
                            ? '<span class="error-type" style="background:' + provColor(r.provider) + '">' + r.provider + '</span>'
                            : '—';
                        const ttft = r.bedrock_first_byte_ms > 0 ? fmtMs(r.bedrock_first_byte_ms) : fmtMs(r.first_byte_ms);
                        return '<tr style="cursor:pointer" onclick="showRequestBody(\'' + r.request_id + '\',\'' + r.model + '\',\'' + time + '\')">'
                            + '<td>' + time + '</td>'
                            + '<td style="font-family:monospace;font-size:11px">' + r.request_id + '</td>'
                            + '<td>' + provLabel + '</td>'
                            + '<td>' + abbrevModel(r.model) + '</td>'
                            + '<td style="font-family:monospace">' + fmtMs(r.duration_ms) + '</td>'
                            + '<td style="font-family:monospace">' + ttft + '</td>'
                            + '<td style="font-family:monospace">' + tokStr + '</td>'
                            + '<td>' + (r.compressed ? pct : '—') + '</td>'
                            + '<td style="font-family:monospace">' + (r.message_count || '—') + '</td>'
                            + '<td>' + fmtTypes(r.msg_types, r.has_context_management) + '</td>'
                            + '<td>' + typeLabel + '</td>'
                            + '</tr>';
                    }).join('');
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
                        const label = displayName(name);
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
                        return '<tr>'
                            + '<td>' + time + '</td>'
                            + '<td><span class="error-type">' + err.error_type + '</span></td>'
                            + '<td>' + err.provider + '</td>'
                            + '<td>' + err.model + '</td>'
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
                const resp = await fetch('/requests/' + _modalRequestId + '?stage=' + _modalStage);
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
                        return '<tr>'
                            + '<td style="font-family:monospace;font-size:11px;">' + fp + '</td>'
                            + '<td>' + e.provider + '</td>'
                            + '<td><span class="error-type">' + e.error_type + '</span></td>'
                            + '<td>' + e.count + '</td>'
                            + '<td style="font-size:11px;">' + first + '</td>'
                            + '<td style="font-size:11px;">' + last + '</td>'
                            + '<td style="font-size:11px;max-width:300px;word-break:break-word;" title="' + e.message + '">' + msg + '</td>'
                            + '</tr>';
                    }).join('');
                } else {
                    tbody.innerHTML = '<tr><td colspan="7" class="no-errors">No errors recorded yet</td></tr>';
                }
            } catch (error) {
                console.error('Failed to load error types:', error);
            }
        }

        initCharts();
        loadMetrics();
        loadErrorTypes();
        setInterval(loadMetrics, 30000);
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
    fn dashboard_js_should_render_status_active_on_cold_start_with_no_last_error_kind_and_not_cooling(
    ) {
        let block = extract_cls_block();
        assert!(
            block.trim_end().ends_with(": 'status-active'"),
            "the ternary's final fallback (reached when lastKind matches neither special \
             case and cooling is falsy) must be 'status-active'"
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

    // MAJOR finding (PR #16 Gate 2 review): the tests above only check that
    // each class string appears *somewhere* in the ternary block, and that
    // one condition's position precedes another's — never that a specific
    // `lastKind` value maps to its *own* specific class. A swap of the two
    // consequents (`'auth' ? 'status-schema-drift' : ... 'response_shape_mismatch'
    // ? 'status-auth-required'`) would pass every test above. These two
    // assert direct adjacency between condition and consequent instead.

    #[test]
    fn dashboard_js_should_map_auth_kind_to_auth_required_class_specifically() {
        let block = extract_cls_block();
        assert!(
            block.contains("lastKind === 'auth' ? 'status-auth-required'"),
            "lastKind === 'auth' must map directly to 'status-auth-required'"
        );
    }

    #[test]
    fn dashboard_js_should_map_response_shape_mismatch_kind_to_schema_drift_class_specifically() {
        let block = extract_cls_block();
        assert!(
            block.contains("lastKind === 'response_shape_mismatch' ? 'status-schema-drift'"),
            "lastKind === 'response_shape_mismatch' must map directly to 'status-schema-drift'"
        );
    }
}
