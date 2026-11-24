// Test hook for AC 19 (desktop/tests/offline.rs): with the server down the
// app lists, opens and searches what it holds, explains what it cannot,
// and queues a send; once online the send goes out; a second send is queued
// for the kill-between-flush-and-ack check.
const RECENT = 'Q3 recent harbor';
const OLD = 'Q3 ancient lantern';
const SENDS = {
  first: { clientId: '2f6b1c1e-9d0a-4f43-8a51-0c0e1f3b5a01', opId: 'q3-send-first' },
  second: { clientId: '2f6b1c1e-9d0a-4f43-8a51-0c0e1f3b5a02', opId: 'q3-send-second' },
};
const SENT_WAIT_MS = 120_000;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const post = { 'content-type': 'application/json', 'x-requested-with': 'tagmail' };

const sentIds = new Set();

async function watchSent() {
  const internals = window.__TAURI_INTERNALS__;
  await internals.invoke('plugin:event|listen', {
    event: 'api:changes',
    target: { kind: 'Any' },
    handler: internals.transformCallback((e) => {
      for (const c of e.payload.changes) {
        if (c.entity === 'outbox' && c.op.kind !== 'delete' && c.payload?.status === 'sent') {
          sentIds.add(c.id);
        }
      }
    }),
  });
}

async function step(name) {
  while ((await fetch(`/__probe/step?name=${name}`)).status !== 200) await sleep(200);
}

function report(body) {
  return fetch('/__probe/report', { method: 'POST', body: JSON.stringify(body) });
}

// GET /api/views/{view}/index: zigzag deltas as LEB128 varints
// (packages/api-types/src/idIndex.ts).
function decodeIds(bytes) {
  const ids = [];
  let prev = 0;
  let pos = 0;
  while (pos < bytes.length) {
    let negative = false;
    let magnitude = 0;
    let scale = 1;
    for (let count = 0; ; count++) {
      const byte = bytes[pos++];
      if (count === 0) {
        negative = (byte & 1) === 1;
        magnitude = (byte & 0x7f) >>> 1;
        scale = 64;
      } else {
        magnitude += (byte & 0x7f) * scale;
        scale *= 128;
      }
      if ((byte & 0x80) === 0) break;
    }
    prev += negative ? -(magnitude + 1) : magnitude;
    ids.push(prev);
  }
  return ids;
}

async function view(id) {
  const resp = await fetch(`/api/views/${encodeURIComponent(id)}/index`);
  const ids = resp.ok ? decodeIds(new Uint8Array(await resp.arrayBuffer())) : [];
  return { status: resp.status, ids, partial: resp.headers.get('x-tagmail-partial') };
}

async function open(threadId) {
  const detail = await (await fetch(`/api/threads/${threadId}`)).json();
  const message = detail.messages[detail.messages.length - 1].id;
  const resp = await fetch(`/api/messages/${message}/render`);
  const body = await resp.text();
  return { status: resp.status, body: body.slice(0, 4096) };
}

async function send(which) {
  const { identities } = await (await fetch('/api/identities')).json();
  const draft = await fetch('/api/drafts', {
    method: 'POST',
    headers: post,
    body: JSON.stringify({
      clientId: SENDS[which].clientId,
      identityId: identities[0].id,
      to: [{ email: 'q3@example.com' }],
      cc: [],
      bcc: [],
      subject: `Q3 offline send ${which}`,
      text: `Sent exactly once (${which}).\n`,
      attachmentIds: [],
      saveNow: false,
    }),
  });
  const created = await draft.json();
  if (!draft.ok) return { draft: draft.status, error: created };
  const { id } = created;
  const sent = await fetch('/api/send', {
    method: 'POST',
    headers: post,
    body: JSON.stringify({ opId: SENDS[which].opId, draftId: id }),
  });
  return {
    draft: draft.status,
    send: sent.status,
    error: sent.ok ? null : await sent.json(),
    outbox: await outbox(which),
    ids: await outboxIds(which),
  };
}

async function items(which) {
  const { items } = await (await fetch('/api/outbox')).json();
  return items.filter((i) => i.opId === SENDS[which].opId);
}

async function outbox(which) {
  return (await items(which)).map((i) => i.status);
}

async function outboxIds(which) {
  return (await items(which)).map((i) => i.id);
}

async function run() {
  await watchSent();
  await step('offline');
  const inbox = await view('inbox');
  const rows = inbox.ids.length
    ? (await (await fetch(`/api/threads?ids=${inbox.ids.join(',')}`)).json()).threads
    : [];
  const recent = rows.find((r) => r.subject === RECENT);
  const old = rows.find((r) => r.subject === OLD);
  const offline = {
    stage: 'offline',
    subjects: rows.map((r) => r.subject),
    recent: recent ? await open(recent.id) : null,
    old: old ? await open(old.id) : null,
    searchRecent: { ...(await view('search:seagull')), want: recent?.id ?? null },
    searchOld: await view('search:lighthouse'),
  };
  const first = await send('first');
  await report({ ...offline, send: first });

  await step('online');
  const deadline = Date.now() + SENT_WAIT_MS;
  const seen = () => first.ids?.length > 0 && first.ids.every((id) => sentIds.has(id));
  while (!seen() && Date.now() < deadline) await sleep(500);
  await report({ stage: 'online', sentSeen: seen(), outbox: await outbox('first') });

  await step('offline2');
  await report({ stage: 'offline2', send: await send('second') });
}

run().catch((e) => report({ stage: 'error', error: String(e) }));
