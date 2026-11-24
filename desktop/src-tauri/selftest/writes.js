// Test hook: a write the app takes offline must show at once (optimistic
// overlay) and re-sending its op id once online must not make a second op.
const OP_ID = '5b0d8f3e-2a8c-4c1e-9f6a-1d2e3f4a5b6c';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function step(name) {
  while ((await fetch(`/__probe/step?name=${name}`)).status !== 200) await sleep(200);
}

function report(body) {
  return fetch('/__probe/report', { method: 'POST', body: JSON.stringify(body) });
}

function star() {
  return fetch('/api/ops', {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-requested-with': 'tagmail' },
    body: JSON.stringify({ opId: OP_ID, kind: 'star', threadIds: [1] }),
  });
}

async function run() {
  await step('offline');
  const queued = await star();
  const rows = await (await fetch('/api/threads?ids=1')).json();
  const ops = await (await fetch('/api/ops')).json();
  await report({
    stage: 'offline',
    status: queued.status,
    starred: rows.threads?.[0]?.starred ?? null,
    pending: (ops.ops ?? []).filter((o) => o.opId === OP_ID).map((o) => o.status),
  });
  await step('online');
  const replayed = await star();
  await report({ stage: 'replay', status: replayed.status, op: await replayed.json() });
}

run().catch((e) => report({ stage: 'error', error: String(e) }));
