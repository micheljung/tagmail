// Test hook: reports each compose window's fields once, so tests can check
// what a mailto link opened without a WebDriver.
const reported = new WeakSet();

function field(win, label) {
  const group = win.querySelector(`[role="group"][aria-label="${label}"]`);
  return group ? [...group.querySelectorAll('.chip .name')].map((c) => c.textContent.trim()) : [];
}

setInterval(() => {
  for (const win of document.querySelectorAll('section.window')) {
    const body = win.querySelector('[role="textbox"][aria-label="Message body"]');
    if (reported.has(win) || !body?.isContentEditable) continue;
    reported.add(win);
    const report = {
      to: field(win, 'To'),
      cc: field(win, 'Cc'),
      bcc: field(win, 'Bcc'),
      subject: win.querySelector('input[aria-label="Subject"]')?.value ?? null,
      body: body.innerText,
    };
    fetch('/__probe/report', { method: 'POST', body: JSON.stringify(report) });
  }
}, 200);
