/* Shared runtime for the WireView Pro II Xeneon Edge widgets.
 *
 * Polls the local bridge (wireview-bridge.exe) and hands each reading to
 * the page. Configuration comes from the page URL's query string so one hosted
 * page serves every setup:
 *
 *   ?host=http://localhost:8765   bridge origin (local pages use their own origin)
 *   ?wire_limit=10.5              amps per wire that counts as 100 % (TG default limit)
 *   ?total_limit=55               amps total that counts as 100 %
 *   ?cable_w=600                  cable power rating used by the power gauge (default: what the cable reports)
 *   ?interval=1000                poll interval in ms
 *   ?decimals=2                   decimals on the headline numbers
 *   ?accent=f08e33                accent colour (hex, no #)
 *   ?bg=000000  ?fg=ffffff        background / text colour
 *   ?label=0                      hide the small caption line
 */
(function () {
  const q = new URLSearchParams(location.search);
  const num = (k, d) => { const v = parseFloat(q.get(k)); return Number.isFinite(v) ? v : d; };
  const pos = (k, d) => { const v = num(k, d); return v > 0 ? v : d; };   // limits must be positive
  const STALE_S = 5, SERVED_STALE_S = 10;
  const hex = (k) => { const v = q.get(k); return v && /^[0-9a-fA-F]{3,8}$/.test(v) ? '#' + v : null; };
  const localPage = /^https?:$/.test(location.protocol) && ['localhost', '127.0.0.1', '[::1]'].includes(location.hostname);

  const cfg = {
    host: (q.get('host') || (localPage ? location.origin : 'http://localhost:8765')).replace(/\/+$/, ''),
    wireLimit: pos('wire_limit', 10.5),
    totalLimit: pos('total_limit', 55),
    cableW: pos('cable_w', 600),
    cableWSet: q.has('cable_w'),
    interval: Math.min(60000, Math.max(250, num('interval', 1000))),
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
  let freshnessTimer = null;
  let controller = null;
  let generation = 0;
  let onData = null;
  const received = new WeakMap();

  function elapsed(d) {
    const at = received.get(d);
    return at ? Math.max(0, (performance.now() - at.time) / 1000) : 0;
  }

  function ages(d) {
    const since = elapsed(d), at = received.get(d);
    const served = at ? at.served + since : (Number.isFinite(d.served_at) ? Date.now() / 1000 - d.served_at : 0);
    // age_s is measured when the reply is produced; time spent in transit
    // also ages the source sample before it reaches the widget.
    const source = (Number.isFinite(d.age_s) ? Math.max(0, d.age_s) : 0) + since + Math.max(0, at ? at.served : served);
    return { source, served };
  }

  function watchFreshness(d) {
    clearTimeout(freshnessTimer);
    if (!d || !d.ok) return;
    const age = ages(d);
    const left = Math.min(STALE_S - age.source, SERVED_STALE_S - age.served);
    if (left < 0) return; // The response callback already shows it as stale.
    freshnessTimer = setTimeout(() => {
      // Runs independently of polling, including during a stalled fetch or
      // a long polling interval. Receipt time is monotonic, so clock changes
      // cannot keep a frozen reading healthy.
      onData(d, null);
    }, Math.ceil(left * 1000) + 1);
  }

  async function tick(epoch) {
    const ctl = new AbortController();
    controller = ctl;
    const to = setTimeout(() => ctl.abort(), Math.min(5000, Math.max(800, cfg.interval * 0.9)));
    try {
      const r = await fetch(cfg.host + '/api/wireview', { signal: ctl.signal, cache: 'no-store' });
      if (!r.ok) throw new Error('Bridge HTTP ' + r.status);
      const d = await r.json();
      if (epoch !== generation) return;
      if (d && typeof d === 'object') received.set(d, {
        time: performance.now(),
        served: Number.isFinite(d.served_at) ? Date.now() / 1000 - d.served_at : 0,
      });
      failures = 0;
      onData(d, null);
      watchFreshness(d);
    } catch (e) {
      if (epoch !== generation) return;
      failures += 1;
      if (failures >= 2) {
        clearTimeout(freshnessTimer);
        onData(null, e);
      }
    } finally {
      clearTimeout(to);
      if (epoch === generation) {
        controller = null;
        timer = setTimeout(() => tick(epoch), cfg.interval);
      }
    }
  }

  function restart() {
    generation += 1;
    // A tab coming back is a fresh start: a failure counted while it was
    // hidden must not turn the first new hiccup into the offline overlay.
    failures = 0;
    clearTimeout(timer);
    if (controller) controller.abort();
    tick(generation);
  }

  function start(callback) {
    onData = callback;
    clearTimeout(freshnessTimer);
    restart();
  }
  document.addEventListener('visibilitychange', () => {
    if (onData && document.visibilityState === 'visible') restart();
  });

  // Offline / error copy shared by every widget.
  function problemText(d, err) {
    if (err || !d) return { title: 'Bridge offline', hint: cfg.host };
    if (d.ok) {
      // Never show old numbers as healthy: the reading itself must be fresh
      // and the bridge must still be producing replies.
      const age = ages(d);
      if (age.served > SERVED_STALE_S) return { title: 'Stale readings', hint: 'bridge stopped updating' };
      if (age.source > STALE_S) return { title: 'Stale readings', hint: 'source stopped updating' };
      return null;
    }
    if (d.status) return { title: d.status, hint: d.hint || '' };
    // Older bridge without status/hint fields.
    if (d.hwinfo_running === false) return { title: 'HWiNFO not running', hint: 'start HWiNFO64 with Shared Memory on' };
    if (!d.device_found) return { title: 'WireView not found', hint: 'close the WireView app, restart HWiNFO' };
    return { title: 'No data', hint: d.error || '' };
  }

  window.WireView = { cfg, level, activeFaults, fmt, start, problemText };
})();
