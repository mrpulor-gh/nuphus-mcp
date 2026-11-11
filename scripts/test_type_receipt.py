#!/usr/bin/env python3
"""Real MCP/CDP regression on an isolated headless Chrome, never the user's profile."""
import argparse
import http.server
import json
from pathlib import Path
import re
import subprocess
import tempfile
import threading
import time
from mcp_client import MCP


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--binary', type=Path, required=True)
    ap.add_argument('--chrome', required=True)
    ap.add_argument('--baseline', action='store_true')
    ap.add_argument('--click-baseline', action='store_true')
    args = ap.parse_args()
    inner = b'''<textarea aria-label="Terminal fixture"></textarea><canvas></canvas>
    <button style="display:block;margin-top:25px" aria-label="Nested action"
      onclick="parent.postMessage({clicked:true},'*')">Action</button>
    <script>const t=document.querySelector('textarea');let entered=0;
    t.addEventListener('keydown',e=>{if(e.key==='Enter'){entered++;parent.postMessage({entered},'*')}});
    t.addEventListener('input',e=>{const value=t.value;t.value='';
      document.querySelector('canvas').getContext('2d').fillText(value,1,20);
      parent.postMessage({value,entered},'*');});</script>'''
    class Fixture(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200)
            self.send_header('Content-Type', 'text/html; charset=utf-8')
            self.end_headers()
            self.wfile.write(inner if self.path == '/inner' else (
                '<h1>Static outer portal</h1><iframe src="http://127.0.0.1:%d/inner"></iframe>'
                '<script>window.addEventListener("message",e=>{if(e.data.clicked)window.clickReceipt=e.data;else window.receipt=e.data})</script>'
                % inner_server.server_port).encode())
        def log_message(self, *args):
            pass
    inner_server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
    outer_server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
    for server in (inner_server, outer_server):
        threading.Thread(target=server.serve_forever, daemon=True).start()
    client = chrome = None
    with tempfile.TemporaryDirectory(prefix='nuphus-type-test-', ignore_cleanup_errors=True) as td:
        root = Path(td)
        with (root/'chrome.log').open('w') as clog, (root/'mcp.log').open('w') as mlog:
            try:
                chrome = subprocess.Popen([args.chrome, '--headless=new', '--no-first-run',
                    '--remote-debugging-port=0', f'--user-data-dir={root / "profile"}', 'about:blank'],
                    stdout=subprocess.DEVNULL, stderr=clog)
                portfile = root/'profile'/'DevToolsActivePort'
                deadline = time.monotonic()+20
                while not portfile.exists():
                    assert chrome.poll() is None and time.monotonic() < deadline
                    time.sleep(.05)
                endpoint = None
                retry_deadline = time.monotonic()+20
                while endpoint is None:
                    try:
                        endpoint = 'http://127.0.0.1:'+portfile.read_text().splitlines()[0]
                    except (PermissionError, IndexError):
                        assert chrome.poll() is None and time.monotonic() < retry_deadline
                        time.sleep(.05)
                client = MCP(args.binary.resolve(), endpoint, mlog)
                client.ok('browser_new_tab')
                client.ok('browser_navigate', url=f'http://127.0.0.1:{outer_server.server_port}/')
                snap = client.ok('browser_snapshot')
                ref = re.search(r'(@\d+)[^\n]*Terminal fixture', snap)
                assert ref, snap
                # Real repeated terminal input starts with the outer frame already
                # focused. Otherwise changing focus alone masks the old defect.
                client.ok('browser_evaluate', script="document.querySelector('iframe').focus()")
                started = time.monotonic()
                result = client.call('browser_type', selector=ref[1], text='中文 terminal input', clear=False)
                elapsed = time.monotonic()-started
                receipt = client.ok('browser_evaluate', script='JSON.stringify(window.receipt)')
                deadline = time.monotonic()+2
                while receipt == 'null' and time.monotonic() < deadline:
                    time.sleep(.05)
                    receipt = client.ok('browser_evaluate', script='JSON.stringify(window.receipt)')
                assert '中文 terminal input' in receipt and '"entered":0' in receipt.replace('\\', ''), (receipt, result, snap)
                if args.baseline:
                    assert result.get('isError') and 'no change was detected' in str(result), result
                    print(f'BASELINE reproduced: input received once, no Enter, false error after {elapsed:.3f}s', flush=True)
                    return
                if args.click_baseline:
                    tabs_now = json.loads(client.ok('browser_list_tabs'))
                    target_index = next(t['index'] for t in tabs_now if t['selected'])
                    client.ok('browser_switch_tab', index=target_index, activate=True)
                    snap = client.ok('browser_snapshot')
                    ref = re.search(r'(@\d+)[^\n]*Nested action', snap)
                    r = client.call('browser_click', selector=ref[1], trusted=True)
                    received = client.ok('browser_evaluate', script='JSON.stringify(window.clickReceipt)')
                    print('BASELINE nested trusted click receipt:', received, 'toolError:', r.get('isError', False), flush=True)
                    assert received == 'null', received
                    return
                assert not result.get('isError'), result
                assert elapsed < 5, elapsed
                print(f'PASS cross-origin terminal consumed input once, no Enter, receipt in {elapsed:.3f}s', flush=True)
                started = time.monotonic()
                client.ok('browser_press', key='Enter')
                assert time.monotonic()-started < 1.5
                entered = ""
                dl_enter = time.monotonic()+2
                while time.monotonic() < dl_enter:
                    entered = client.ok('browser_evaluate', script='JSON.stringify(window.receipt)').replace(chr(92), '')
                    if '"entered":1' in entered:
                        break
                    time.sleep(.05)
                assert '"entered":1' in entered, entered
                snap = client.ok('browser_snapshot')
                ref = re.search(r'(@\d+)[^\n]*Nested action', snap)
                started = time.monotonic()
                client.ok('browser_click', selector=ref[1])
                assert time.monotonic()-started < 3
                okd = False
                dl_click = time.monotonic()+2
                while time.monotonic() < dl_click:
                    if 'true' in client.ok('browser_evaluate', script='JSON.stringify(window.clickReceipt)'):
                        okd = True
                        break
                    time.sleep(.05)
                assert okd, client.ok('browser_evaluate', script='JSON.stringify(window.clickReceipt)')
                client.ok('browser_evaluate', script='window.clickReceipt=null')
                # Trusted cross-frame hit-testing requires a visible compositor
                # surface in headless Chrome. Activate ONLY this isolated fixture.
                tabs_now = json.loads(client.ok('browser_list_tabs'))
                target_index = next(t['index'] for t in tabs_now if t['selected'])
                client.ok('browser_switch_tab', index=target_index, activate=True)
                started = time.monotonic()
                client.ok('browser_click', selector=ref[1], trusted=True)
                assert time.monotonic()-started < 3
                okd = False
                dl_click = time.monotonic()+2
                while time.monotonic() < dl_click:
                    if 'true' in client.ok('browser_evaluate', script='JSON.stringify(window.clickReceipt)'):
                        okd = True
                        break
                    time.sleep(.05)
                assert okd, client.ok('browser_evaluate', script='JSON.stringify(window.clickReceipt)')
                print('PASS nested terminal Enter and trusted iframe button click without fixed waits', flush=True)

                client.ok('browser_evaluate', script='''document.body.innerHTML=`
                  <input id=field value=prefix><input id=readonly readonly value=original>
                  <input id=disabled disabled><input id=blocked><input id=trap>
                  <input id=other><div id=editable contenteditable=true>old</div>`;
                  document.querySelector('#blocked').addEventListener('beforeinput',e=>e.preventDefault());
                  document.querySelector('#trap').addEventListener('focus',()=>document.querySelector('#other').focus());
                  window.unrelated=setInterval(()=>document.body.dataset.tick=Date.now(),20);''')
                def value(selector):
                    return client.ok('browser_evaluate', script=f'document.querySelector({json.dumps(selector)}).value')
                client.ok('browser_evaluate', script="const el=document.querySelector('#field');el.focus();el.setSelectionRange(6,6)")
                client.ok('browser_type', selector='#field', text='追加', clear=False)
                assert 'prefix追加' in value('#field')
                client.ok('browser_type', selector='#field', text='replacement')
                assert 'replacement' in value('#field') and 'prefix' not in value('#field')
                client.ok('browser_type', selector='#field', text='replacement')
                client.ok('browser_evaluate', script="document.querySelector('#field').select()")
                client.ok('browser_type', selector='#field', text='', clear=False)
                assert 'replacement' in value('#field')
                client.ok('browser_type', selector='#field', text='')
                assert 'replacement' not in value('#field')
                print('PASS append, replacement, unchanged value, empty input and empty clear', flush=True)
                client.ok('browser_type', selector='#editable', text='new editable')
                assert 'new editable' in client.ok('browser_evaluate', script="document.querySelector('#editable').textContent")
                for selector in ('#readonly', '#disabled', '#blocked', '#trap'):
                    r = client.call('browser_type', selector=selector, text='must not arrive', clear=False)
                    assert r.get('isError'), (selector, r)
                    assert 'must not arrive' not in value(selector), selector
                assert 'must not arrive' not in value('#other')
                print('PASS contenteditable; readonly, disabled, canceled and diverted focus rejected despite unrelated DOM mutations', flush=True)
                client.ok('browser_evaluate', script="clearInterval(window.unrelated);document.body.innerHTML='<input aria-label=Legacy><input aria-label=AX>'")
                client.ok('browser_type', selector='@e0', text='legacy')
                snap = client.ok('browser_snapshot')
                ref = re.search(r'(@\d+)[^\n]*textbox[^\n]*AX', snap)
                assert ref, snap
                client.ok('browser_type', selector=ref[1], text='ax')
                assert 'legacy' in client.ok('browser_evaluate', script="document.querySelectorAll('input')[0].value")
                assert 'ax' in client.ok('browser_evaluate', script="document.querySelectorAll('input')[1].value")
                print('PASS CSS, legacy and AX selector paths', flush=True)
            except BaseException:
                mlog.flush()
                print((root/'mcp.log').read_text()[-3000:])
                raise
            finally:
                if client:
                    client.close()
                if chrome and chrome.poll() is None:
                    chrome.terminate()
                    try:
                        chrome.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        chrome.kill()
                        chrome.wait()
                for server in (inner_server, outer_server):
                    server.shutdown()
                    server.server_close()


if __name__ == '__main__':
    main()
