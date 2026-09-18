#!/usr/bin/env python3
"""Hermetic tests of manual-browser harness transport; no browser, native API or network."""
import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import struct
import sys
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('chromium_harness', Path(__file__).with_name('verify-macos-chromium-controls.py'))
h = importlib.util.module_from_spec(spec)
spec.loader.exec_module(h)


def frame(value, opcode=1):
    body = value if isinstance(value, bytes) else json.dumps(value).encode()
    length = bytes([len(body)]) if len(body) < 126 else b'\x7e' + struct.pack('!H', len(body))
    return bytes([0x80 | opcode]) + length + body


class Socket:
    def __init__(self, frames=b'', refuse=False):
        self.buffer = b''
        self.frames = frames
        self.refuse = refuse
        self.closed = False
        self.sent = []

    def settimeout(self, value):
        pass

    def sendall(self, value):
        self.sent.append(value)
        if value.startswith(b'GET '):
            key = value.split(b'Sec-WebSocket-Key: ')[1].split(b'\r\n')[0].decode()
            accept = base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
            if self.refuse:
                accept = b'wrong'
            self.buffer = b'HTTP/1.1 101 Switching Protocols\r\nSec-WebSocket-Accept: ' + accept + b'\r\n\r\n' + self.frames

    def recv(self, size):
        result, self.buffer = self.buffer[:size], self.buffer[size:]
        return result

    def close(self):
        self.closed = True


class Tests(unittest.TestCase):
    def connect(self, socket):
        with patch.object(h.socket, 'create_connection', return_value=socket) as connect:
            client = h.CDP(12345, '/devtools/browser/test-123')
            connect.assert_called_once_with(('127.0.0.1', 12345), timeout=5)
        self.addCleanup(client.close)
        return client

    def test_window_readiness_only_repeats_reads_and_keeps_exact_identity(self):
        current = {'browser_pid': 42, 'windows': [{'window_id': 7}]}
        ready = {'ok': True, 'candidates': [{'identity': {'window_id': 7}}]}
        pending = {'ok': False, 'error': 'application does not expose bounded AX windows'}
        for responses in [[pending, ready], [pending, pending, pending]]:
            calls = []
            pauses = []
            def call(name, **args):
                self.assertEqual(name, 'list_macos_windows')
                self.assertEqual(args, {'pid': 42})
                result = responses[len(calls)]
                calls.append(result)
                return result
            last, candidates, attempts = h.list_ready_fixture_window(call, lambda: current, current, pauses.append)
            self.assertEqual(calls, responses)
            self.assertEqual(attempts, responses)
            self.assertEqual(len(pauses), len(calls) - 1)
            self.assertEqual(bool(candidates), last is ready)

    def test_window_readiness_never_masks_permission_or_retargets_window(self):
        current = {'browser_pid': 42, 'windows': [{'window_id': 7}]}
        with patch.object(h.time, 'sleep') as sleep:
            result = h.list_ready_fixture_window(lambda *a, **kw: {'error': 'permission denied'}, lambda: current, current, sleep)
            self.assertEqual(len(result[2]), 1)
            sleep.assert_not_called()
        for changed in [{'browser_pid': 99, 'windows': [{'window_id': 7}]},
                        {'browser_pid': 42, 'windows': [{'window_id': 8}]}]:
            with self.assertRaises(RuntimeError):
                h.list_ready_fixture_window(lambda *a, **kw: {'candidates': []}, lambda: changed, current, lambda _: None)

    def test_address_refuses_header_injection_and_nonbrowser_paths(self):
        for port, path in [(0, '/devtools/browser/a'), (65536, '/devtools/browser/a'),
                           (1234, '/devtools/page/a'), (1234, '/devtools/browser/a\r\nInjected: true'),
                           (1234, '/devtools/browser/a b'), (1234, '/devtools/browser/')]:
            with patch.object(h.socket, 'create_connection') as connect, self.assertRaises(RuntimeError):
                h.CDP(port, path)
            connect.assert_not_called()

    def test_bad_handshake_closes_socket(self):
        socket = Socket(refuse=True)
        with patch.object(h.socket, 'create_connection', return_value=socket), self.assertRaises(RuntimeError):
            h.CDP(12345, '/devtools/browser/test')
        self.assertTrue(socket.closed)

    def test_reply_event_ping_and_masked_request(self):
        socket = Socket(frame({'method': 'fixture.event'}) + frame(b'x', 9) + frame({'id': 1, 'result': {'ok': True}}))
        client = self.connect(socket)
        self.assertEqual(client.call('Fixture.read', {'n': 1}, 'test-session'), {'ok': True})
        sent = socket.sent[1]
        self.assertTrue(sent[1] & 128)
        size = sent[1] & 127
        mask = sent[2:6]
        value = json.loads(bytes(x ^ mask[i % 4] for i, x in enumerate(sent[6:6+size])))
        self.assertEqual(value['sessionId'], 'test-session')
        self.assertEqual(value['method'], 'Fixture.read')
        self.assertEqual(socket.sent[-1][0] & 15, 10)

    def test_protocol_error_and_excessive_frame_refuse(self):
        for frames in [frame({'id': 1, 'error': {'message': 'fixture refusal'}}),
                       b'\x81\x7f' + struct.pack('!Q', 1024 * 1024 + 1), b'\x01\x00']:
            client = self.connect(Socket(frames))
            with self.assertRaises(RuntimeError):
                client.call('Fixture.read')

    def test_bounded_child_output_and_errors(self):
        argv = [sys.executable, '-c']
        self.assertEqual(h.run_bounded(argv + ['print("fixture")'], time.monotonic() + 5), b'fixture\n')
        for code in ['raise SystemExit(3)', 'import sys; sys.stderr.write("x"*70000)',
                     'import sys; sys.stdout.write("x"*70000)']:
            with self.assertRaises(RuntimeError):
                h.run_bounded(argv + [code], time.monotonic() + 5)

    def test_expired_deadline_kills_and_reaps_child(self):
        with self.assertRaises(RuntimeError):
            h.run_bounded([sys.executable, '-c', 'import time; time.sleep(30)'], time.monotonic() - 1)


if __name__ == '__main__':
    unittest.main()
