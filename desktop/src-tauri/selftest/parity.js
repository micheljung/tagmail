// Test hook: the parity checks the replica test cannot see, run in the
// webview: jump depth in the inbox, search through the app and opening a
// message older than the offline window. Stages survive page loads.
const STAGE = 'tagmail-parity-stage';
const JUMPS = [10000, 0, 99000, 50000, 99999, 99000];
const TIMED_FROM = 2;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitFor(what, ms, fn) {
  const end = performance.now() + ms;
  while (performance.now() < end) {
    const v = fn();
    if (v) return v;
    await sleep(25);
  }
  throw new Error(`timed out waiting for ${what}`);
}

function report(body) {
  return fetch('/__probe/report', { method: 'POST', body: JSON.stringify(body) });
}

function total() {
  const text = document.querySelector('.total')?.textContent ?? '';
  return Number(text.replace(/[^0-9]/g, ''));
}

function jump(list, target) {
  return new Promise((resolve, reject) => {
    const rowHeight = list.querySelector('[role="option"]').offsetHeight;
    const t0 = performance.now();
    list.scrollTop = target * rowHeight;
    const tick = () => {
      const row = list.querySelector(`[data-index="${target}"] [data-loaded]`);
      if (row) return resolve({ ms: performance.now() - t0, subject: row.querySelector('.subject').textContent });
      if (performance.now() - t0 > 5000) return reject(new Error(`row ${target} never loaded`));
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  });
}

async function inbox() {
  const list = await waitFor('the inbox', 60000, () =>
    document.querySelector('[role="listbox"]')?.querySelector('[data-loaded]') ? document.querySelector('[role="listbox"]') : null
  );
  const jumps = [];
  for (const target of JUMPS) jumps.push({ target, ...(await jump(list, target)) });
  await report({ stage: 'jump', total: total(), jumps: jumps.slice(TIMED_FROM) });
  sessionStorage.setItem(STAGE, 'search');
  location.href = '/search?q=invoice';
}

async function search() {
  await waitFor('search results', 30000, () => document.querySelector('[role="option"] [data-loaded]'));
  await report({ stage: 'search', total: total(), partial: !!document.querySelector('[data-partial]') });
  sessionStorage.setItem(STAGE, 'open');
  location.href = '/thread/5';
}

async function open() {
  const frame = await waitFor('the message', 30000, () => document.querySelector('iframe[title]'));
  const text = await waitFor('the rendered body', 30000, () => frame.contentDocument?.body?.innerText.trim());
  sessionStorage.removeItem(STAGE);
  await report({ stage: 'open', text });
}

const stages = { search, open };
(stages[sessionStorage.getItem(STAGE)] ?? inbox)().catch((e) => report({ stage: 'error', error: String(e) }));
