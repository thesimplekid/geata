#!/usr/bin/env python3
"""A loopback dashboard and backend for exercising Geata's paid rate limit."""
import argparse
from collections import Counter, deque
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time


class Metrics:
    def __init__(self, args):
        self.args = args
        self.lock = threading.Lock()
        self.events = deque(maxlen=10000)
        self.totals = Counter()
        self.ready = False

    def record(self, status, latency, retry_after, paid=False):
        event = dict(time=time.time(), status=status, latency=round(latency, 1),
                     retry_after=retry_after, paid=paid)
        with self.lock:
            self.events.append(event)
            self.totals['total'] += 1
            if paid and status == 200:
                self.totals['paid_accepted'] += 1
            if not paid and status == 402:
                self.totals['unpaid_blocked'] += 1
            self.totals['accepted' if status == 200 else 'payment_required' if status == 402 else 'errors'] += 1
        return event

    def snapshot(self):
        now = time.time()
        with self.lock:
            events, totals = list(self.events), dict(self.totals)
        buckets = [dict(accepted=0, unpaid_accepted=0, paid_accepted=0,
                        payment_required=0, unpaid_blocked=0, errors=0) for _ in range(30)]
        for event in events:
            age = int(now) - int(event['time'])
            if 0 <= age < 30:
                key = 'accepted' if event['status'] == 200 else 'payment_required' if event['status'] == 402 else 'errors'
                buckets[29 - age][key] += 1
                if event['status'] == 200:
                    buckets[29 - age]['paid_accepted' if event['paid'] else 'unpaid_accepted'] += 1
                if not event['paid'] and event['status'] == 402:
                    buckets[29 - age]['unpaid_blocked'] += 1
        return dict(config=dict(rate=self.args.rate, burst=self.args.burst, price=self.args.price,
                                mint=self.args.mint, proxy_port=self.args.proxy_port, data_dir=str(self.args.data_dir)),
                    totals=totals, rps=sum(now - event['time'] < 1 for event in events),
                    quoted_sats=totals.get('payment_required', 0) * self.args.price,
                    history=buckets, recent=list(reversed(events[-12:])))


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, body, content_type='application/json'):
        if not isinstance(body, bytes):
            body = json.dumps(body).encode()
        self.send_response(status)
        self.send_header('Content-Type', content_type)
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Cache-Control', 'no-store')
        self.send_header('X-Content-Type-Options', 'nosniff')
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def dashboard_host(self):
        return self.headers.get('Host') == self.server.origin.removeprefix('http://')

    def do_GET(self):
        if self.path == '/work':
            self.reply(200, {'message': 'Hello from behind Geata!'})
        elif not self.dashboard_host():
            self.reply(404, {'error': 'Use the dashboard URL printed in the terminal.'})
        elif self.path == '/api/stats':
            self.reply(200 if self.server.metrics.ready else 503, self.server.metrics.snapshot())
        elif self.path in ('/', '/app.js', '/style.css'):
            name, kind = {'/': ('index.html', 'text/html; charset=utf-8'),
                          '/app.js': ('app.js', 'text/javascript; charset=utf-8'),
                          '/style.css': ('style.css', 'text/css; charset=utf-8')}[self.path]
            self.reply(200, Path(__file__).with_name(name).read_bytes(), kind)
        else:
            self.reply(404, {'error': 'Not found'})

    def do_POST(self):
        if (not self.dashboard_host() or
                self.headers.get('Origin', self.server.origin) != self.server.origin or
                self.headers.get('Sec-Fetch-Site') == 'cross-site'):
            self.reply(403, {'error': 'Use this dashboard to send demo requests.'})
            return
        if self.path not in ('/api/request', '/api/pay'):
            self.reply(404, {'error': 'Not found'})
            return
        token = None
        if self.path == '/api/pay':
            try:
                length = int(self.headers.get('Content-Length', '0'))
                if not 0 < length <= 20000:
                    raise ValueError('Invalid body size')
                self.connection.settimeout(5)
                body = json.loads(self.rfile.read(length))
                token = body.get('token')
                if (not isinstance(token, str) or not token.startswith('cashuB') or
                        len(token) > 16384 or not token.isascii() or any(c.isspace() for c in token)):
                    raise ValueError('Invalid token')
            except (ValueError, AttributeError, OSError):
                self.reply(400, {'error': 'Provide a cashuB token from the configured mint.'})
                return
        if not self.server.metrics.ready or not self.server.slots.acquire(blocking=False):
            self.reply(503, {'error': 'Demo is busy; wait for requests to finish.'})
            return
        started = time.monotonic()
        connection = http.client.HTTPConnection('127.0.0.1', self.server.metrics.args.proxy_port, timeout=35)
        try:
            # Every measured request actually crosses Geata's rate limiter.
            headers = {'Host': 'localhost'}
            if token:
                headers['X-Cashu'] = token
            connection.request('GET', '/work', headers=headers)
            response = connection.getresponse()
            response.read()
            status, retry_after = response.status, response.getheader('Retry-After')
        except (OSError, http.client.HTTPException):
            status, retry_after = 502, None
        finally:
            connection.close()
            self.server.slots.release()
        event = self.server.metrics.record(status, (time.monotonic() - started) * 1000, retry_after, paid=token is not None)
        self.reply(200, event)


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def stop(process):
    if process and process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--geata', default='result/bin/geata', help='path to the Geata binary')
    parser.add_argument('--port', type=int, default=9090, help='dashboard port (default: 9090)')
    parser.add_argument('--proxy-port', type=int, default=8080)
    parser.add_argument('--rate', type=int, default=5, help='free requests per second per IP')
    parser.add_argument('--burst', type=int, default=10)
    parser.add_argument('--price', type=int, default=2, help='fixed price in sats per paid request')
    parser.add_argument('--mint', default='https://testnut.cashudevkit.org', help='accepted mint URL (test or real)')
    parser.add_argument('--data-dir', type=Path, default=Path('.state/traffic-demo'), help='persistent Geata wallet/state directory')
    args = parser.parse_args()
    if not (1 <= args.rate <= 1000 and 1 <= args.burst <= 10000 and 1 <= args.price <= 4294967295):
        parser.error('rate must be 1–1000, burst 1–10000, and price 1–4294967295')
    if not (0 <= args.port <= 65535 and 0 <= args.proxy_port <= 65535):
        parser.error('ports must be between 0 and 65535')
    args.data_dir = args.data_dir.resolve()
    binary = Path(args.geata).resolve()
    if not binary.is_file():
        parser.error(f'Geata binary not found: {binary}. Run nix build path:. first.')
    if args.proxy_port == 0:
        args.proxy_port = free_port()
    scratch = os.environ.get('TMPDIR') or ('/data/rust/tmp' if Path('/data/rust/tmp').is_dir() else None)
    process = None
    server = ThreadingHTTPServer(('127.0.0.1', args.port), Handler)
    server.origin = f'http://127.0.0.1:{server.server_port}'
    server.metrics = Metrics(args)
    server.slots = threading.BoundedSemaphore(16)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    def interrupted(*_):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    try:
        with tempfile.TemporaryDirectory(prefix='geata-demo-', dir=scratch) as directory:
            root = Path(directory)
            config = root / 'Geatafile'
            config.write_text(f'''http://ready.local {{ respond "ready" }}
http://localhost {{
    rate_limit {args.rate}/s burst {args.burst}
    pay {args.price} sat {json.dumps(args.mint)}
    max_inflight 128
    reverse_proxy 127.0.0.1:{server.server_port}
}}
''')
            with (root / 'geata.log').open('w+') as log:
                try:
                    process = subprocess.Popen([str(binary), 'run', '--config', str(config),
                                                '--data-dir', str(args.data_dir),
                                                '--http-listen', f'127.0.0.1:{args.proxy_port}',
                                                '--https-listen', f'127.0.0.1:{free_port()}'],
                                               stdout=log, stderr=subprocess.STDOUT)
                    for _ in range(100):
                        if process.poll() is not None:
                            log.seek(0)
                            raise RuntimeError(log.read())
                        connection = http.client.HTTPConnection('127.0.0.1', args.proxy_port, timeout=0.2)
                        try:
                            connection.request('GET', '/', headers={'Host': 'ready.local'})
                            response = connection.getresponse()
                            if response.status == 200 and response.read() == b'ready':
                                break
                        except (OSError, http.client.HTTPException):
                            pass
                        finally:
                            connection.close()
                        time.sleep(0.1)
                    else:
                        raise RuntimeError('Geata did not become ready')
                    server.metrics.ready = True
                    print(f'Geata demo: {server.origin}\nFree: {args.rate}/s, burst {args.burst}; paid request: {args.price} sat + mint fees.\nMint: {args.mint}\nPersistent wallet: {args.data_dir}\nPress Ctrl+C to stop.', flush=True)
                    while process.poll() is None:
                        time.sleep(0.2)
                    raise RuntimeError('Geata exited; restart the demo.')
                finally:
                    server.metrics.ready = False
                    stop(process)
    except KeyboardInterrupt:
        pass
    finally:
        server.shutdown()
        server.server_close()


if __name__ == '__main__':
    main()
