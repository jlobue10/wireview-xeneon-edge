/* Shared runtime for the WireView Pro II Xeneon Edge widgets.
 *
 * Polls the local bridge (bridge/wireview_bridge.py) and hands each reading to
 * the page. Configuration comes from the page URL's query string so one hosted
 * page serves every setup:
 *
 *   ?host=http://localhost:8765   bridge origin (default shown)
 *   ?wire_limit=10.5              amps per wire that counts as 100 % (TG default limit)
 *   ?total_limit=55               amps total that counts as 100 %
 *   ?cable_w=600                  cable power rating used by the power gauge
 *   ?interval=1000                poll interval in ms
 *   ?decimals=2                   decimals on the headline numbers
 *   ?accent=f08e33                accent colour (hex, no #)
 *   ?bg=000000  ?fg=ffffff        background / text colour
 *   ?label=0                      hide the small caption line
 */
(function () {
  const q = new URLSearchParams(location.search);
  const num = (k, d) => { const v = parseFloat(q.get(k)); return Number.isFinite(v) ? v : d; };
  const hex = (k) => { const v = q.get(k); return v && /^[0-9a-fA-F]{3,8}$/.test(v) ? '#' + v : null; };

  const cfg = {
    host: (q.get('host') || 'http://localhost:8765').replace(/\/+$/, ''),
    wireLimit: num('wire_limit', 10.5),
    totalLimit: num('total_limit', 55),
    cableW: num('cable_w', 600),
    interval: Math.max(250, num('interval', 1000)),
    decimals: Math.max(0, Math.min(3, num('decimals', 2))),
    showLabel: q.get('label') !== '0',
    accent: hex('accent'), bg: hex('bg'), fg: hex('fg'),
  };

  const root = document.documentElement;
  if (cfg.accent) root.style.setProperty('--accent', cfg.accent);
  if (cfg.bg) root.style.setProperty('--surface', cfg.bg);
  if (cfg.fg) root.style.setProperty('--ink', cfg.fg);
  if (!cfg.showLabel) root.classList.add('no-label');

  // Status thresholds. Returns 'ok' | 'warn' | 'crit'.
  function level(value, limit) {
    if (value == null || !Number.isFinite(value) || !limit) return 'ok';
    const r = value / limit;
    if (r >= 1) return 'crit';
    if (r >= 0.8) return 'warn';
    return 'ok';
  }

  const FAULT_TEXT = {
    temp_chip: 'Chip over-temp', temp_sensor: 'Sensor over-temp',
    over_current_total: 'Over-current', over_current_wire: 'Wire over-current',
    over_power: 'Over-power', imbalance: 'Current imbalance',
  };
  function activeFaults(d) {
    const f = (d && d.faults) || {};
    return Object.keys(f).filter((k) => f[k]).map((k) => FAULT_TEXT[k] || k);
  }

  function fmt(v, decimals) {
    if (v == null || !Number.isFinite(v)) return '--';
    return v.toFixed(decimals == null ? cfg.decimals : decimals);
  }

  let failures = 0;
  let timer = null;
  async function tick(onData) {
    const ctl = new AbortController();
    const to = setTimeout(() => ctl.abort(), Math.max(800, cfg.interval * 0.9));
    try {
      const r = await fetch(cfg.host + '/api/wireview', { signal: ctl.signal, cache: 'no-store' });
      const d = await r.json();
      failures = 0;
      onData(d, null);
    } catch (e) {
      failures += 1;
      if (failures >= 2) onData(null, e);
    } finally {
      clearTimeout(to);
      timer = setTimeout(() => tick(onData), cfg.interval);
    }
  }

  function start(onData) {
    if (timer) clearTimeout(timer);
    tick(onData);
    document.addEventListener('visibilitychange', () => {
      if (document.visibilityState === 'visible') { clearTimeout(timer); tick(onData); }
    });
  }

  // Offline / error copy shared by every widget.
  function problemText(d, err) {
    if (err || !d) return { title: 'Bridge offline', hint: cfg.host };
    if (!d.hwinfo_running) return { title: 'HWiNFO not running', hint: 'start HWiNFO64 with Shared Memory on' };
    if (!d.device_found) return { title: 'WireView not found', hint: 'close the WireView app, restart HWiNFO' };
    if (!d.ok) return { title: 'No data', hint: d.error || '' };
    return null;
  }

  window.WireView = { cfg, level, activeFaults, fmt, start, problemText };
})();
