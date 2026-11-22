const results = {};
const out = document.getElementById('out');

function check(name, ok, detail) {
  results[name] = { ok: Boolean(ok), detail };
}

async function api(path, body) {
  const init = body === undefined ? {} : { method: 'POST', body: JSON.stringify(body) };
  const res = await fetch(path, init);
  return res.json();
}

function frame(src, sandbox) {
  return new Promise((resolve) => {
    const el = document.createElement('iframe');
    if (sandbox !== null) el.setAttribute('sandbox', sandbox);
    el.addEventListener('load', () => resolve(el), { once: true });
    el.src = src;
    document.getElementById('frames').append(el);
  });
}

async function armEvent(name, timeoutMs) {
  let resolve;
  const payload = new Promise((res, rej) => {
    resolve = res;
    setTimeout(() => rej(new Error(`no ${name} event`)), timeoutMs);
  });
  await window.__TAURI__.event.listen(name, (e) => resolve(e.payload));
  return { payload };
}

async function transport() {
  const first = performance.now();
  const threads = await api('/api/threads');
  const firstMs = performance.now() - first;
  const samples = [];
  for (let i = 0; i < 20; i++) {
    const t = performance.now();
    await api('/api/threads');
    samples.push(performance.now() - t);
  }
  samples.sort((a, b) => a - b);
  check('threads', threads.length === 50, { count: threads.length, first_ms: firstMs, median_ms: samples[10], max_ms: samples[19] });
  for (const t of threads.slice(0, 10)) {
    const li = document.createElement('li');
    li.textContent = `${t.subject} [${t.labels.join(', ')}]`;
    document.getElementById('threads').append(li);
  }
  return threads;
}

async function render(id) {
  const framed = await frame(`/api/messages/${id}/render`, 'allow-same-origin');
  const doc = framed.contentDocument;
  const height = doc ? doc.documentElement.scrollHeight : null;
  check('iframe_height_readable', height !== null && height >= 1500, { height });
  await frame(`/api/messages/${id}/render?csp=0`, 'allow-same-origin');
  await frame(`/api/messages/${id}/render?csp=0`, null);
  await new Promise((r) => setTimeout(r, 500));
  const beacons = await api('/api/beacons');
  const n = (k) => beacons[k] ?? 0;
  check('csp_blocks_img', n('img:csp') === 0 && n('img:nocsp') === 2, beacons);
  check('sandbox_blocks_script', n('script:csp') === 0 && n('script:nocsp') === 1, beacons);
}

function spaCsp() {
  let evalBlocked = false;
  try {
    eval('1');
  } catch {
    evalBlocked = true;
  }
  check('spa_csp_blocks_eval', evalBlocked, {});
}

async function events() {
  const latencies = [];
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('no tick events')), 5000);
    window.__TAURI__.event.listen('tick', (e) => {
      latencies.push(Date.now() - e.payload.sent_ms);
      if (latencies.length === 8) {
        clearTimeout(timer);
        resolve();
      }
    });
  });
  latencies.sort((a, b) => a - b);
  check('emit_events', true, { count: latencies.length, median_ms: latencies[4], max_ms: latencies[7] });
}

async function offlineLabel(thread) {
  await api('/api/network', { online: false });
  const { op_id: opId } = await api('/api/labels', { email_ids: thread.email_ids, add: ['Spike'] });
  const [shown] = await api('/api/threads');
  const serverBefore = await api(`/api/server/labels/${thread.email_ids[0]}`);
  check('optimistic_overlay', shown.pending && shown.labels.includes('Spike') && !serverBefore.includes('Spike'), {
    opId,
    shown: shown.labels,
    serverBefore,
  });

  const flushed = await armEvent('replica:flushed', 5000);
  const lost = await api('/api/network', { online: true, lose_acks: true });
  const lostEvent = await flushed.payload;
  const replayed = await api('/api/network', { online: true });
  const serverAfter = await api(`/api/server/labels/${thread.email_ids[0]}`);
  const [settled] = await api('/api/threads');
  check(
    'flush_dedupe',
    lostEvent.sent === 1 &&
      lost.server_applied === 1 &&
      replayed.flush.duplicates === 1 &&
      replayed.server_applied === 1 &&
      serverAfter.includes('Spike') &&
      !settled.pending,
    { lost, lostEvent, replayed, serverAfter, settled: settled.labels },
  );
}

async function main() {
  let threads = [];
  try {
    threads = await transport();
    spaCsp();
    await render(threads[0].email_ids[0]);
    await events();
    await offlineLabel(threads[0]);
  } catch (e) {
    check('exception', false, `${e} ${e && e.stack}`);
  }
  const ok = Object.values(results).every((r) => r.ok);
  const report = { ok, userAgent: navigator.userAgent, origin: location.origin, href: location.href, results };
  out.textContent = JSON.stringify(report, null, 2);
  await api('/api/report', report);
}

main();
