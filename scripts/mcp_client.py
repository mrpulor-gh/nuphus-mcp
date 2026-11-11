#!/usr/bin/env python3
"""Minimal MCP stdio client shared by this repository's regression scripts.

Python stdlib only. Drives a real nuphus-mcp binary over JSON-RPC so the e2e
scripts exercise the same transport an MCP client uses. Windows-safe: the child's
stdout is decoded as UTF-8 explicitly (the default locale codec is GBK there).
"""
import json
import os
import queue
import subprocess
import threading


class MCP:
    def __init__(self, binary, endpoint, log, name='nuphus-regression'):
        env = dict(os.environ, NUPHUS_MCP_BROWSER_CDP_URL=endpoint,
                   NUPHUS_MCP_ALLOW_PRIVATE_NAV='1')
        self.p = subprocess.Popen(
            [str(binary), '--confirm-write'],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log,
            text=True, encoding='utf-8', errors='replace', env=env)
        self.responses = queue.Queue()
        self.next_id = 0

        def read():
            for line in self.p.stdout:
                line = line.strip()
                if line:
                    self.responses.put(json.loads(line))

        threading.Thread(target=read, daemon=True).start()
        self.rpc('initialize', {
            'protocolVersion': '2024-11-05',
            'capabilities': {},
            'clientInfo': {'name': name, 'version': '1'},
        })

    def rpc(self, method, params):
        self.next_id += 1
        self.p.stdin.write(json.dumps({
            'jsonrpc': '2.0', 'id': self.next_id,
            'method': method, 'params': params}) + '\n')
        self.p.stdin.flush()
        result = self.responses.get(timeout=60)
        assert result.get('id') == self.next_id, result
        assert 'error' not in result, result
        return result['result']

    def call(self, tool_name, **args):
        return self.rpc('tools/call', {
            'name': tool_name, 'arguments': dict(args, confirm=True)})

    def ok(self, tool_name, **args):
        r = self.call(tool_name, **args)
        assert not r.get('isError'), (tool_name, r)
        return '\n'.join(x.get('text', '') for x in r.get('content', []))

    def close(self):
        if self.p.poll() is None:
            try:
                self.p.stdin.close()
            except Exception:
                pass
            try:
                self.p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.p.kill()
                self.p.wait()
