const assert = require('node:assert/strict');
const { test } = require('node:test');
const fs = require('node:fs');
const vm = require('node:vm');
const script = fs.readFileSync(require('node:path').join(__dirname, '../docs/common/wireview.js'), 'utf8');

function widget(url = 'http://localhost:8765/?interval=1000') {
  let now = 0, serial = 0;
  const timers = new Map(), listeners = new Map(), requests = [], updates = [];
  const attrs = {};
  const document = {
    documentElement: { style: { setProperty() {} }, classList: { add() {} }, setAttribute(k, v) { attrs[k] = v; } },
    visibilityState: 'visible',
    addEventListener(name, cb) { listeners.set(name, [...(listeners.get(name) || []), cb]); },
  };
  const ctx = vm.createContext({
    location: new URL(url), URLSearchParams, AbortController, document, window: {},
    Date: { now: () => 1790000000000 + now }, performance: { now: () => now },
    setTimeout(fn, delay) { const id = ++serial; timers.set(id, { fn, due: now + delay }); return id; },
    clearTimeout(id) { timers.delete(id); },
    // Deliberately permit a late completion after abort: the generation guard
    // must suppress an obsolete reply even if the transport already finished.
    fetch(url, options) { return new Promise((resolve, reject) => requests.push({ url, options, resolve, reject })); },
  });
  vm.runInContext(script, ctx);
  const api = ctx.window.WireView;
  const drain = async () => { for (let i = 0; i < 8; i++) await Promise.resolve(); };
  return {
    api, requests, updates, timers, attrs,
    start() { api.start((data, error) => updates.push({ data, problem: api.problemText(data, error) })); },
    async reply(i, data = { ok: true, age_s: 0, served_at: (1790000000000 + now) / 1000 }) {
      requests[i].resolve({ ok: true, json: async () => data }); await drain();
    },
    async fail(i) { requests[i].reject(new Error('offline')); await drain(); },
    async advance(ms) {
      const end = now + ms;
      while (true) {
        const next = [...timers].filter(([, t]) => t.due <= end).sort((a, b) => a[1].due - b[1].due)[0];
        if (!next) break;
        now = next[1].due; timers.delete(next[0]); next[1].fn(); await drain();
      }
      now = end; await drain();
    },
    visible() { for (const cb of listeners.get('visibilitychange') || []) cb(); },
    listenerCount() { return (listeners.get('visibilitychange') || []).length; },
  };
}

test('local pages follow their origin, hosted pages retain the loopback default', () => {
  for (const origin of ['http://localhost:9000', 'http://127.0.0.1:9000', 'http://[::1]:9000']) {
    const w = widget(origin + '/combined/'); w.start();
    assert.equal(w.requests[0].url, origin + '/api/wireview');
  }
  assert.equal(widget('https://jlobue10.github.io/wireview-xeneon-edge/combined/').api.cfg.host, 'http://localhost:8765');
  assert.equal(widget('http://localhost:9000/?host=http://localhost:9999/').api.cfg.host, 'http://localhost:9999');
});

test('a known theme is applied through data-theme, unknown names and the default leave it alone', () => {
  for (const name of ['corsair', 'ice', 'mono', 'nord', 'light']) {
    const w = widget('http://localhost:8765/total-power/?theme=' + name);
    assert.equal(w.api.cfg.theme, name);
    assert.equal(w.attrs['data-theme'], name);
  }
  assert.equal(widget('http://localhost:8765/total-power/?theme=NORD').attrs['data-theme'], 'nord');
  for (const url of ['http://localhost:8765/total-power/', 'http://localhost:8765/total-power/?theme=plaid', 'http://localhost:8765/?theme=']) {
    const w = widget(url);
    assert.equal(w.api.cfg.theme, 'grizzly');
    assert.equal(w.attrs['data-theme'], undefined);
  }
});

test('both connector temperatures are labelled, missing ones shown as --', () => {
  const { api } = widget();
  assert.equal(api.temps({ temp_in: 35.46, temp_out: 35.84 }), 'in 35.5 · out 35.8 °C');
  assert.equal(api.temps({ temp_in: -5.5, temp_out: null }), 'in -5.5 · out -- °C');
  assert.equal(api.temps({ temp_in: 'x' }), 'in -- · out -- °C');
  assert.equal(api.temps(null), 'in -- · out -- °C');
});

test('source staleness is displayed at five seconds even with a 60-second poll interval', async () => {
  const w = widget('http://localhost:9000/?interval=60000'); w.start(); await w.reply(0);
  assert.equal(w.updates[0].problem, null);
  await w.advance(5000); assert.equal(w.updates.length, 1);
  await w.advance(1); assert.equal(w.updates[1].problem.title, 'Stale readings');
  assert.equal(w.requests.length, 1);
});

test('watchdog accounts for the age of both source and served response', async () => {
  const w = widget('http://localhost:9000/?interval=60000'); w.start();
  await w.reply(0, { ok: true, age_s: 4, served_at: 1790000000 });
  await w.advance(1001); assert.equal(w.updates.at(-1).problem.hint, 'source stopped updating');
  const cached = widget('http://localhost:9000/?interval=60000'); cached.start();
  await cached.reply(0, { ok: true, age_s: 0, served_at: 1789999989 });
  assert.equal(cached.updates.at(-1).problem.hint, 'bridge stopped updating');
});

test('time spent receiving a reply counts toward source staleness', async () => {
  const w = widget('http://localhost:9000/?interval=60000'); w.start(); await w.advance(4000);
  await w.reply(0, { ok: true, age_s: 2, served_at: 1790000000 });
  assert.equal(w.updates[0].problem.title, 'Stale readings');
});

test('a pending fetch cannot keep the previous reading healthy', async () => {
  const w = widget(); w.start(); await w.reply(0); await w.advance(5001);
  assert.equal(w.requests.length, 2);
  assert.equal(w.updates.at(-1).problem.title, 'Stale readings');
});

test('visibility restart aborts the old request and leaves a single poll loop', async () => {
  const w = widget(); w.start(); w.visible();
  assert.equal(w.requests[0].options.signal.aborted, true);
  await w.reply(0); assert.equal(w.updates.length, 0);
  await w.reply(1); assert.equal(w.updates.length, 1);
  await w.advance(1000); assert.equal(w.requests.length, 3);
});

test('a visibility restart forgets failures counted before it', async () => {
  const w = widget(); w.start(); await w.fail(0);
  assert.equal(w.updates.length, 0); // one failure is not yet a problem
  await w.advance(1000); w.visible(); // the second poll is aborted by the restart
  await w.fail(2);
  assert.equal(w.updates.length, 0, 'first failure after a restart must not show the overlay');
  await w.advance(1000); await w.fail(3);
  assert.equal(w.updates.at(-1).problem.title, 'Bridge offline');
});

test('repeated start replaces the callback and registers one visibility listener', async () => {
  const w = widget(); w.start(); w.start();
  assert.equal(w.listenerCount(), 1);
  await w.reply(0); assert.equal(w.updates.length, 0);
  await w.reply(1); await w.advance(1000); assert.equal(w.requests.length, 3);
});

test('a newer reply replaces the previous freshness deadline', async () => {
  const w = widget(); w.start(); await w.reply(0); await w.advance(1000); await w.reply(1);
  await w.advance(4001); assert.equal(w.updates.at(-1).problem, null);
  await w.advance(1000); assert.equal(w.updates.at(-1).problem.title, 'Stale readings');
});
