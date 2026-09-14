'use strict';
const $ = id => document.getElementById(id);
let timer = null;
let pending = 0;
let connected = false;
let alive = true;
let paying = false;

function stopTraffic(message = 'Traffic stopped. The free allowance is refilling.') {
  clearInterval(timer);
  timer = null;
  $('toggle').textContent = 'Start traffic';
  $('toggle').classList.remove('running');
  $('activity').textContent = message;
}

async function sendOne() {
  if (!connected || pending >= 8) return;
  pending++;
  try {
    const response = await fetch('/api/request', {method: 'POST', signal: AbortSignal.timeout(7000)});
    if (!response.ok) throw new Error('The demo is busy or disconnected.');
    const event = await response.json();
    $('activity').textContent = event.status === 200 ? '200 · Request accepted.' : event.status === 402
      ? `402 · Payment required, or retry in ${event.retry_after || '1'}s.`
      : `${event.status} · Request failed. No payment was sent.`;
  } catch (error) {
    stopTraffic(error.message);
  } finally {
    pending--;
  }
}

function startTraffic() {
  clearInterval(timer);
  timer = setInterval(sendOne, 1000 / Number($('rate').value));
  $('toggle').textContent = 'Stop traffic';
  $('toggle').classList.add('running');
  $('activity').textContent = 'Sending real requests through Geata…';
}
$('toggle').addEventListener('click', () => timer === null ? startTraffic() : stopTraffic());
$('single').addEventListener('click', sendOne);
$('burst-button').addEventListener('click', async () => {
  $('burst-button').disabled = true;
  for (let i = 0; i < 20 && connected; i++) await sendOne();
  $('burst-button').disabled = !connected;
});
$('rate').addEventListener('input', () => {
  $('target').value = $('rate').value;
  if (timer !== null) startTraffic();
});
window.addEventListener('pagehide', () => { alive = false; stopTraffic(); });
document.addEventListener('visibilitychange', () => { if (document.hidden) stopTraffic('Paused while this tab is hidden.'); });

function drawChart(history, rate) {
  const max = Math.max(rate * 1.3, 5, ...history.map(b => b.accepted + b.payment_required + b.errors));
  const height = 195, width = 690, left = 35, baseline = 215;
  let svg = '';
  for (let n = 0; n <= 4; n++) {
    const y = baseline - height * n / 4;
    svg += `<line x1="${left}" y1="${y}" x2="735" y2="${y}" stroke="#e8eee8"/><text x="26" y="${y + 3}" text-anchor="end">${Math.round(max * n / 4)}</text>`;
  }
  history.forEach((bucket, i) => {
    let y = baseline;
    for (const [key, color] of [['accepted', '#318165'], ['payment_required', '#e0aa42'], ['errors', '#d25d57']]) {
      const h = bucket[key] / max * height;
      y -= h;
      svg += `<rect x="${left + i * width / 30 + 3}" y="${y}" width="17" height="${h}" rx="2" fill="${color}"/>`;
    }
  });
  const limitY = baseline - rate / max * height;
  svg += `<line x1="${left}" x2="735" y1="${limitY}" y2="${limitY}" stroke="#89a18d" stroke-dasharray="4 5"/><text x="${left + 5}" y="${limitY - 6}">Free refill · ${rate}/s</text>`;
  $('chart').innerHTML = svg;
}

function render(data) {
  for (const [id, value] of Object.entries({rps: data.rps, 'free-rate': data.config.rate,
    price: data.config.price, quoted: data.quoted_sats, total: data.totals.total || 0,
    accepted: data.totals.accepted || 0, required: data.totals.payment_required || 0, errors: data.totals.errors || 0})) {
    $(id).textContent = Number(value).toLocaleString();
  }
  $('mint').textContent = data.config.mint;
  $('mint').href = data.config.mint;
  $('paid-count').textContent = data.totals.paid_accepted || 0;
  if (!paying) $('pay').textContent = `Pay ${data.config.price} sat + fees for one request`;
  $('burst').textContent = `Per IP · burst of ${data.config.burst}`;
  drawChart(data.history, data.config.rate);
  if (!data.recent.length) return;
  $('requests').replaceChildren(...data.recent.map(event => {
    const row = document.createElement('tr');
    const values = [new Date(event.time * 1000).toLocaleTimeString(), '', `${event.latency.toFixed(1)} ms`,
      (event.status === 402 || (event.paid && event.status === 200)) ? `${data.config.price} sat + fees` : '—', event.retry_after ? `${event.retry_after}s` : '—'];
    values.forEach((value, index) => {
      const cell = document.createElement('td');
      if (index === 1) {
        const badge = document.createElement('span');
        badge.className = `badge ${event.status === 200 ? 'ok' : event.status === 402 ? 'pay' : 'error'}`;
        badge.textContent = event.status === 200 ? (event.paid ? '200 · Paid' : '200 · Free') : event.status === 402 ? '402 · Payment required' : `${event.status} · Error`;
        cell.append(badge);
      } else cell.textContent = value;
      row.append(cell);
    });
    return row;
  }));
}

async function poll() {
  try {
    const response = await fetch('/api/stats', {signal: AbortSignal.timeout(3000)});
    if (!response.ok) throw new Error('Unavailable');
    render(await response.json());
    if (!connected) {
      $('activity').textContent = 'Ready. Send one request, or start a stream.';
      for (const id of ['toggle', 'single', 'burst-button']) $(id).disabled = false;
    }
    connected = true;
    $('pay').disabled = paying;
    $('connection').textContent = '● Connected to Geata';
    $('connection').classList.add('online');
  } catch (_) {
    connected = false;
    $('pay').disabled = true;
    stopTraffic('Connection lost. Check the demo terminal.');
    $('connection').textContent = '○ Disconnected';
    $('connection').classList.remove('online');
    for (const id of ['toggle', 'single', 'burst-button']) $(id).disabled = true;
  }
  if (alive) setTimeout(poll, 500);
}
$('payment-form').addEventListener('submit', async event => {
  event.preventDefault();
  if (paying || !connected) return;
  const token = $('token').value.trim();
  if (!token.startsWith('cashuB')) {
    $('payment-status').textContent = 'Paste a cashuB token from the mint shown here.';
    return;
  }
  stopTraffic('Traffic paused for your payment.');
  paying = true;
  $('pay').disabled = true;
  $('pay').textContent = 'Verifying payment…';
  $('payment-status').textContent = 'Waiting for the mint. Do not submit this token elsewhere.';
  try {
    const response = await fetch('/api/pay', {method: 'POST', headers: {'Content-Type': 'application/json'},
      body: JSON.stringify({token}), signal: AbortSignal.timeout(40000)});
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || 'Payment request failed.');
    if (result.status === 200) {
      $('token').value = '';
      $('payment-status').textContent = 'Payment accepted. Your request reached the backend.';
    } else if (result.status === 400) {
      $('payment-status').textContent = 'Rejected: invalid, insufficient, or already-used token. Check the mint and fees.';
    } else {
      $('payment-status').textContent = `${result.status} · Payment could not be confirmed. Keep this token and retry the same token; do not pay with a new one.`;
    }
  } catch (_) {
    $('payment-status').textContent = 'Payment outcome is uncertain. Keep this token and retry the same token; do not pay with a new one.';
  } finally {
    paying = false;
    $('pay').disabled = !connected;
  }
});
poll();
