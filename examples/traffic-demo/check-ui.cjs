// Exercise the dashboard's event handlers without a browser or external packages.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

function element() {
  return {
    value: '', textContent: '', disabled: false, dataset: {}, handlers: {}, children: [],
    classList: {add() {}, remove() {}, toggle() {}},
    addEventListener(type, callback) { this.handlers[type] = callback; },
    append(child) { this.children.push(child); },
    replaceChildren(...children) { this.children = children; },
  };
}

function deferred() {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return {promise, resolve};
}

const response = data => ({ok: true, json: async () => data});
const settle = () => new Promise(resolve => setImmediate(resolve));

async function main() {
  const html = fs.readFileSync(path.join(__dirname, 'index.html'), 'utf8');
  const elements = Object.fromEntries([...html.matchAll(/id="([^"]+)"/g)].map(match => [match[1], element()]));
  const intervals = new Map();
  let nextTimer = 0;
  const calls = [];
  const unpaid = {status: 402, paid: false, retry_after: '1', time: 100, latency: 1};
  const bucket = {accepted: 2, unpaid_accepted: 1, paid_accepted: 1, payment_required: 3, unpaid_blocked: 3, errors: 0};
  const stats = {
    config: {rate: 25, burst: 10, price: 2, mint: 'https://mint.example.com'},
    totals: {total: 5, accepted: 2, paid_accepted: 1, payment_required: 3, unpaid_blocked: 3},
    history: Array.from({length: 30}, () => ({...bucket})),
    recent: [unpaid], rps: 5, quoted_sats: 6,
  };
  let payment;
  let background;
  let statsOnline = true;
  const context = vm.createContext({
    document: {getElementById: id => {
      assert.ok(elements[id], `Missing HTML element: ${id}`);
      return elements[id];
    }, createElement: element, addEventListener() {}},
    window: {addEventListener() {}},
    AbortSignal,
    setInterval(callback, delay) { intervals.set(++nextTimer, {callback, delay}); return nextTimer; },
    clearInterval(id) { intervals.delete(id); },
    setTimeout() {},
    fetch: async (url, options) => {
      calls.push({url, options});
      if (url === '/api/stats') return {...response(stats), ok: statsOnline};
      if (url === '/api/pay') return payment.promise;
      assert.equal(url, '/api/request');
      return background ? background.promise : response(unpaid);
    },
  });
  vm.runInContext(fs.readFileSync(path.join(__dirname, 'app.js'), 'utf8'), context);
  await settle();
  assert.equal(elements.single.disabled, false);
  assert.equal(elements.accepted.textContent, '1', 'Paid successes must not count as unpaid');
  assert.equal(elements['paid-total'].textContent, '1');
  assert.match(elements.pressure.textContent, /returning 402/);
  assert.ok(!elements.chart.innerHTML.includes('NaN'));

  elements.overload.handlers.click();
  assert.equal(Number(elements.rate.value), 100, 'High traffic follows the configured allowance');
  assert.equal(Number(elements.rate.max), 100, 'Slider must permit the high traffic target');
  assert.equal(intervals.size, 1);
  const traffic = [...intervals.values()][0];
  assert.equal(traffic.delay, 10);

  // Fill all eight background slots. A manual probe must still get through.
  const flood = deferred();
  background = flood;
  const pending = Array.from({length: 8}, () => traffic.callback());
  background = null;
  await elements.single.handlers.click();
  assert.equal(elements['unpaid-status'].dataset.outcome, 'blocked');
  const probeResult = elements['unpaid-status'].textContent;

  // A slow mint must neither pause the flood nor allow a second payment submission.
  payment = deferred();
  elements.token.value = 'cashuBfixture';
  const submitting = elements['payment-form'].handlers.submit({preventDefault() {}});
  assert.equal(intervals.size, 1, 'Paying must leave high traffic running');
  assert.equal(elements.pay.disabled, true);
  await elements['payment-form'].handlers.submit({preventDefault() {}});
  assert.equal(calls.filter(call => call.url === '/api/pay').length, 1);
  flood.resolve(response(unpaid));
  await Promise.all(pending);
  const before = calls.length;
  await traffic.callback();
  assert.equal(calls.length, before + 1, 'Unpaid traffic continues while the mint is responding');
  payment.resolve(response({status: 200, paid: true}));
  await submitting;
  assert.equal(elements.token.value, '');
  assert.equal(elements['payment-status'].dataset.outcome, 'paid');
  const paidResult = elements['payment-status'].textContent;

  // Background completions and polling cannot erase either manual result.
  await traffic.callback();
  await vm.runInContext('poll()', context);
  assert.equal(elements['unpaid-status'].textContent, probeResult);
  assert.equal(elements['payment-status'].textContent, paidResult);
  assert.ok(calls.filter(call => call.url === '/api/request').every(call => !call.options.body));

  // Failed payments keep the token available for retry, with traffic still running.
  payment = deferred();
  elements.token.value = 'cashuBretry';
  const retry = elements['payment-form'].handlers.submit({preventDefault() {}});
  payment.resolve(response({status: 503, paid: true}));
  await retry;
  assert.equal(elements.token.value, 'cashuBretry');
  assert.match(elements['payment-status'].textContent, /retry the same token/);
  assert.equal(intervals.size, 1);

  statsOnline = false;
  await vm.runInContext('poll()', context);
  assert.equal(intervals.size, 0);
  for (const id of ['overload', 'single', 'pay']) assert.equal(elements[id].disabled, true);
  console.log('PASS: high traffic, manual probes under load, paid/unpaid results, payment retries, and disconnect handling');
}

main().catch(error => { console.error(error); process.exitCode = 1; });
