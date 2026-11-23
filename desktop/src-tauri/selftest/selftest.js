const results = {};
const SANDBOX = 'allow-same-origin allow-popups allow-popups-to-escape-sandbox';

function check(name, ok, detail) {
  results[name] = { ok: Boolean(ok), detail };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(what, ms, fn) {
  const end = performance.now() + ms;
  while (performance.now() < end) {
    const value = await fn();
    if (value) return value;
    await sleep(50);
  }
  throw new Error(`timed out waiting for ${what}`);
}

async function json(path, init) {
  return (await fetch(path, init)).json();
}

function frame(src, sandbox) {
  return new Promise((resolve) => {
    const el = document.createElement('iframe');
    if (sandbox !== null) el.setAttribute('sandbox', sandbox);
    el.style.cssText = 'position:fixed;left:-10000px;width:600px;height:200px';
    el.addEventListener('load', () => resolve(el), { once: true });
    el.src = src;
    document.body.append(el);
  });
}

async function spa() {
  const started = performance.now();
  await waitFor('the inbox rows', 30000, () =>
    document.body?.innerText.toLowerCase().includes('self-test 60')
  );
  const index = await fetch('/');
  const csp = index.headers.get('content-security-policy') ?? '';
  check('spa_inbox', csp.includes("script-src 'self'"), {
    ms: Math.round(performance.now() - started),
    csp
  });
}

async function transport() {
  const samples = [];
  let last;
  for (let i = 0; i < 21; i++) {
    const t = performance.now();
    const resp = await fetch('/api/views/inbox/index');
    const bytes = (await resp.arrayBuffer()).byteLength;
    const rows = await json('/api/threads?ids=60,59,58');
    samples.push(performance.now() - t);
    last = { status: resp.status, cursor: resp.headers.get('x-tagmail-cursor'), bytes, rows };
  }
  samples.sort((a, b) => a - b);
  check('transport', last.status === 200 && last.cursor && last.rows.threads?.length === 3, {
    cursor: last.cursor,
    bytes: last.bytes,
    median_ms: samples[10],
    max_ms: samples[20]
  });
  return last.cursor;
}

function spaCsp() {
  let blocked = false;
  try {
    eval('1');
  } catch {
    blocked = true;
  }
  check('spa_csp_blocks_eval', blocked, {});
}

async function render() {
  const framed = await frame('/api/messages/1/render', SANDBOX);
  const height = framed.contentDocument?.documentElement.scrollHeight ?? null;
  check('iframe_height_readable', height !== null && height >= 1500, { height });
  await frame('/__selftest/control.html#sandboxed', 'allow-same-origin');
  await frame('/__selftest/control.html#open', null);
  await sleep(500);
  const beacons = await json('/__selftest/beacons');
  const n = (k) => beacons[k] ?? 0;
  check('render_blocks_img', n('img-render') === 0 && n('img-control') >= 1, beacons);
  check(
    'sandbox_blocks_script',
    n('script-render') === 0 && n('script-sandboxed') === 0 && n('script-open') === 1,
    beacons
  );
}

async function events(cursor) {
  const internals = window.__TAURI_INTERNALS__;
  const seen = [];
  const started = performance.now();
  await internals.invoke('plugin:event|listen', {
    event: 'api:changes',
    target: { kind: 'Any' },
    handler: internals.transformCallback((e) => seen.push(...e.payload.changes))
  });
  const start = await fetch(`/api/desktop/events/start?since=${encodeURIComponent(cursor)}`);
  const { threadId } = await json('/__selftest/touch', { method: 'POST' });
  await waitFor('the api:changes event', 5000, () =>
    seen.some((c) => c.entity === 'thread' && c.id === threadId)
  );
  check('emit_events', start.status === 204, {
    threadId,
    ms: Math.round(performance.now() - started)
  });
}

async function main() {
  try {
    await spa();
    const cursor = await transport();
    spaCsp();
    await render();
    await events(cursor);
  } catch (e) {
    check('exception', false, `${e} ${e && e.stack}`);
  }
  const ok = Object.values(results).every((r) => r.ok);
  const report = { ok, userAgent: navigator.userAgent, origin: location.origin, results };
  await fetch('/__selftest/report', { method: 'POST', body: JSON.stringify(report) });
}

main();
