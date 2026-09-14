#!/usr/bin/env python3
"""Smoke-test the demo against a real Geata binary, without spending tokens."""
import concurrent.futures
import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time


def port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else 'result/bin/geata').resolve()
    dashboard, proxy = port(), port()
    scratch = os.environ.get('TMPDIR') or ('/data/rust/tmp' if Path('/data/rust/tmp').is_dir() else None)
    with tempfile.TemporaryDirectory(prefix='geata-demo-check-', dir=scratch) as directory:
        root = Path(directory)
        with (root / 'demo.log').open('w+') as log:
            process = subprocess.Popen([sys.executable, str(Path(__file__).with_name('demo.py')),
                                        '--geata', str(binary), '--port', str(dashboard),
                                        '--proxy-port', str(proxy), '--data-dir', str(root / 'wallet'),
                                        '--rate', '1', '--burst', '2', '--price', '7',
                                        '--mint', 'https://mint.example.com'], stdout=log, stderr=log)

            def request(path, data=None, origin=None):
                connection = http.client.HTTPConnection('127.0.0.1', dashboard, timeout=8)
                try:
                    headers = {'Content-Type': 'application/json'}
                    if origin:
                        headers['Origin'] = origin
                    connection.request('POST' if data is not None else 'GET', path, data, headers)
                    response = connection.getresponse()
                    return response.status, json.loads(response.read())
                finally:
                    connection.close()

            try:
                for _ in range(100):
                    if process.poll() is not None:
                        raise AssertionError('demo exited during startup')
                    try:
                        status, stats = request('/api/stats')
                        if status == 200:
                            break
                    except OSError:
                        pass
                    time.sleep(0.1)
                else:
                    raise AssertionError('demo did not become ready')
                assert stats['config']['mint'] == 'https://mint.example.com'
                assert stats['config']['rate'] == 1 and stats['config']['price'] == 7
                with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
                    events = list(pool.map(lambda _: request('/api/request', b'')[1], range(12)))
                stats = request('/api/stats')[1]
                assert stats['totals']['total'] == 12
                assert stats['totals']['accepted'] > 0 and stats['totals']['payment_required'] > 0
                assert stats['quoted_sats'] == sum(e['status'] == 402 for e in events) * 7
                assert all(e['retry_after'] for e in events if e['status'] == 402)
                token = 'cashuBinvalid-demo-token'
                result = request('/api/pay', json.dumps({'token': token}).encode())[1]
                assert result['status'] == 400 and result['paid']
                stats = request('/api/stats')[1]
                assert stats['totals'].get('paid_accepted', 0) == 0
                assert token not in json.dumps(stats)
                assert request('/api/pay', b'[]')[0] == 400
                assert request('/api/request', b'', 'https://elsewhere.example')[0] == 403
                time.sleep(1.1)
                assert request('/api/stats')[1]['rps'] == 0
                assert request('/api/request', b'')[1]['status'] == 200
            except Exception:
                log.flush()
                log.seek(0)
                print(log.read(), file=sys.stderr)
                raise
            finally:
                process.terminate()
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            assert process.returncode == 0
            assert (root / 'wallet' / 'proxy.lock').exists(), 'persistent state was removed on exit'
            log.seek(0)
            assert 'cashuBinvalid-demo-token' not in log.read()
            with socket.socket() as sock:
                assert sock.connect_ex(('127.0.0.1', proxy)) != 0, 'Geata was left running'
    print('PASS: demo configuration, real admission/402 quotes, metrics, refill, payment rejection, origin checks, and cleanup')


if __name__ == '__main__':
    main()
