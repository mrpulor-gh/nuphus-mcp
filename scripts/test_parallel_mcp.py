#!/usr/bin/env python3
"""End-to-end regression: two real MCP stdio processes, isolated headless Chrome.
Usage: python3 scripts/test_parallel_mcp.py --binary target/debug/nuphus-mcp --chrome '/path/to/Chrome'
Never attaches to a user's browser/profile. Python stdlib only.
"""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
import time
import urllib.request


from mcp_client import MCP

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--chrome', required=True)
    args=parser.parse_args()
    a=b=chrome=None
    with tempfile.TemporaryDirectory(prefix='nuphus-parallel-mcp-') as td:
        root=Path(td)
        with (root/'chrome.log').open('w') as clog, (root/'mcp.log').open('w') as mlog:
            try:
                chrome=subprocess.Popen([args.chrome,'--headless=new','--no-sandbox','--no-first-run',
                    '--remote-debugging-port=0',f'--user-data-dir={root / "profile"}', 'about:blank'],
                    stdout=subprocess.DEVNULL,stderr=clog)
                portfile=root/'profile'/'DevToolsActivePort'
                deadline=time.monotonic()+20
                while not portfile.exists():
                    assert chrome.poll() is None, 'test Chrome exited'
                    assert time.monotonic()<deadline, 'test Chrome startup timeout'
                    time.sleep(.05)
                endpoint='http://127.0.0.1:'+portfile.read_text().splitlines()[0]
                a=MCP(args.binary.resolve(), endpoint, mlog)
                a.ok('browser_new_tab')
                aid=next(t['target_id'] for t in json.loads(a.ok('browser_list_tabs')) if t['selected'])
                b=MCP(args.binary.resolve(), endpoint, mlog)
                b.ok('browser_new_tab')
                bid=next(t['target_id'] for t in json.loads(b.ok('browser_list_tabs')) if t['selected'])
                assert aid != bid
                a.ok('browser_evaluate',script='document.body.innerHTML="<input id=terminal><div id=status>A</div>"')
                b.ok('browser_evaluate',script='document.body.innerHTML="<input id=terminal><div id=status>B</div>"')
                # An idle owner still owns its target, and failed switching keeps B selected.
                denied=b.call('browser_switch_tab',target_id=aid)
                assert denied.get('isError') and 'busy' in str(denied), denied
                assert next(t['target_id'] for t in json.loads(b.ok('browser_list_tabs')) if t['selected'])==bid
                invalid=b.call('browser_switch_tab')
                assert invalid.get('isError'), 'missing selector must not silently select index 0'
                print('PASS stable target selection, persistent ownership and safe failed switch',flush=True)
                # A and B rendezvous while holding their tool-call guards. Different
                # pages exchange a nonce via local fixture HTTP, proving real overlap
                # rather than timing two quick serial calls.
                import http.server
                waiting=threading.Barrier(2)
                class Fixture(http.server.BaseHTTPRequestHandler):
                    def do_GET(self):
                        if self.path.startswith('/barrier'):
                            try: waiting.wait(timeout=8)
                            except threading.BrokenBarrierError: self.send_error(500); return
                        self.send_response(200)
                        self.send_header('Access-Control-Allow-Origin','*')
                        self.send_header('Content-Type','text/html')
                        self.end_headers()
                        self.wfile.write(b'<input id="terminal"><div id="status">ready</div>')
                    def log_message(self,*args): pass
                server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Fixture)
                threading.Thread(target=server.serve_forever,daemon=True).start()
                fixture=f'http://127.0.0.1:{server.server_port}'
                a.ok('browser_navigate',url=fixture+'/a')
                b.ok('browser_navigate',url=fixture+'/b')
                def command(client,label):
                    return client.ok('browser_evaluate',script=f'''const r=await fetch('/barrier?{label}'); if(!r.ok) throw Error('overlap failed'); document.querySelector('#terminal').value='{label}-only'; return document.querySelector('#terminal').value;''')
                with concurrent.futures.ThreadPoolExecutor(2) as pool:
                    fa=pool.submit(command,a,'upstream'); fb=pool.submit(command,b,'downstream')
                    assert 'upstream-only' in fa.result(timeout=30)
                    assert 'downstream-only' in fb.result(timeout=30)
                print('PASS two real MCP processes execute simultaneously in different tabs',flush=True)
                # Hold a background operation; desktop / explicit activation and
                # browser-wide writes must reject, ordinary reads on B must work.
                with concurrent.futures.ThreadPoolExecutor(1) as pool:
                    held=pool.submit(a.ok,'browser_wait_for',selector='#never-appears',timeout_ms=3500)
                    time.sleep(.6)
                    for tool,kw in [('desktop_screen_size',{}),('browser_switch_tab',{'target_id':bid,'activate':True}),
                                    ('browser_cookies_set',{'name':'parallel-test','value':'x'})]:
                        r=b.call(tool,**kw)
                        assert r.get('isError') and 'busy' in str(r), (tool,r)
                    assert 'downstream-only' in b.ok('browser_evaluate',script="document.querySelector('#terminal').value")
                    try: held.result(timeout=10)
                    except AssertionError as e: assert 'timeout' in str(e).lower(), e
                print('PASS desktop/foreground and browser-wide mutation exclusion',flush=True)
                # EOF releases A's claim but leaves shared Chrome and B alive.
                a.close()
                assert chrome.poll() is None
                assert 'downstream-only' in b.ok('browser_evaluate',script="document.querySelector('#terminal').value")
                b.ok('browser_switch_tab',target_id=aid)
                assert 'upstream-only' in b.ok('browser_evaluate',script="document.querySelector('#terminal').value")
                print('PASS session exit releases claim without closing shared browser',flush=True)
                b.p.kill(); b.p.wait(timeout=10)
                a=MCP(args.binary.resolve(), endpoint, mlog)
                a.ok('browser_switch_tab',target_id=aid)
                assert 'upstream-only' in a.ok('browser_evaluate',script="document.querySelector('#terminal').value")
                a.ok('browser_close')
                assert chrome.poll() is None
                print('PASS crashed MCP releases its claim and another process can take over',flush=True)
                server.shutdown(); server.server_close()
            except BaseException:
                mlog.flush()
                print((root/'mcp.log').read_text()[-6000:])
                raise
            finally:
                if a: a.close()
                if b: b.close()
                if chrome and chrome.poll() is None:
                    chrome.terminate()
                    try: chrome.wait(timeout=10)
                    except subprocess.TimeoutExpired: chrome.kill();chrome.wait()

if __name__=='__main__': main()
