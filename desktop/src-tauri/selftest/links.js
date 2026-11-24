// Test hook for tests/links.rs: each real click (popups need a user
// gesture) opens the next link the ways the SPA and email content do,
// then the app tries to navigate away.
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const report = (body) => fetch('/__probe/report', { method: 'POST', body: JSON.stringify(body) });

const mail = document.createElement('a');
mail.href = 'mailto:ann@x.example';
mail.target = '_blank';
mail.rel = 'noreferrer';

const clicks = [
  () => window.open('https://example.com/new-window', '_blank', 'noreferrer'),
  () => mail.click(),
  () => window.open('file:///etc/passwd', '_blank'),
];

async function away() {
  location.href = 'https://example.com/top';
  await sleep(2000);
  location.href = 'file:///etc/hosts';
  await sleep(1000);
  await report({ stage: 'done', href: location.href });
}

function start() {
  const target = document.createElement('div');
  target.style.cssText = 'position:fixed;inset:0;z-index:2147483647';
  target.addEventListener('click', () => {
    clicks.shift()?.();
    report({ stage: 'clicked', left: clicks.length });
    if (!clicks.length) {
      target.remove();
      away();
    }
  });
  document.body.append(target, mail);
  report({ stage: 'ready' });
}

if (document.body) start();
else addEventListener('DOMContentLoaded', start);
