#!/usr/bin/env python3
"""Real provider-free CU acceptance. Owns only temporary test resources."""
import argparse
import base64
import hashlib
import http.server
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid

spec = importlib.util.spec_from_file_location('cutover', Path(__file__).with_name('test-linux-display-cutover.py'))
cutover = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = cutover
spec.loader.exec_module(cutover)


def require(value, message):
    if not value:
        raise RuntimeError(message)


def save(path, value):
    path.write_text(json.dumps(value, sort_keys=True, indent=2) + '\n')
    path.chmod(0o600)


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


class Page(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'''<!doctype html><meta charset="utf-8"><title>EXTERNAL CU KEYLESS TEST</title>
<style>html,body{margin:0;width:100%;height:100%;background:rgb(220,35,45)}input{position:absolute;inset:0;box-sizing:border-box;width:100%;height:100%;font:30px sans-serif;background:transparent;border:0}</style>
<input autofocus id="proof" aria-label="Proof input" oninput="document.documentElement.style.background=document.body.style.background=this.value==='proof-ready'?'rgb(30,170,70)':'rgb(220,35,45)'">'''
        self.send_response(200)
        self.send_header('Content-Type', 'text/html')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--bin', required=True, type=Path)
    parser.add_argument('--evidence', required=True, type=Path)
    parser.add_argument('--skip-expiry', action='store_true')
    args = parser.parse_args()
    binary, out = args.bin.resolve(), args.evidence.resolve()
    require(sys.platform.startswith('linux'), 'Linux required')
    require(not out.exists(), 'evidence path must be new')
    os.umask(0o077)
    out.mkdir(mode=0o700, parents=True)
    runners = cutover.runner_snapshot()
    require(not any(r['comm'] == 'Runner.Worker' for r in runners), 'CI has priority')
    root = Path(tempfile.mkdtemp(prefix='intendant-external-cu-'))
    home = root / 'home'
    home.mkdir(mode=0o700)
    env = {'HOME': str(home), 'INTENDANT_HOME': str(home / '.intendant'),
           'PATH': os.environ.get('PATH', '/usr/bin:/bin'), 'LANG': 'C.UTF-8',
           'INTENDANT_BROWSER_WORKSPACE_EXECUTABLE': '/usr/lib/chromium/chromium',
           'INTENDANT_BROWSER_WORKSPACE_ALLOW_SYSTEM_BROWSER': '1'}
    require(Path(env['INTENDANT_BROWSER_WORKSPACE_EXECUTABLE']).is_file(), 'native Chromium missing')
    daemon = foreign = server = workspace = display = proof = None
    daemon_log, foreign_log = (out / 'daemon.log').open('wb'), (out / 'foreign.log').open('wb')
    port, checks, commands = free_port(), [], []

    def call(*argv):
        done = subprocess.run([str(binary), 'ctl', '--port', str(port), '--json', *argv],
                              env=env, cwd=root, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                              stderr=subprocess.PIPE, text=True, timeout=95)
        commands.append({'command': list(argv[:3]), 'exit': done.returncode,
                         'stdoutSha256': hashlib.sha256(done.stdout.encode()).hexdigest(),
                         'stderrBytes': len(done.stderr)})
        require(done.returncode == 0, f'ctl failed: {done.stderr[:200]} {done.stdout[:300]}')
        return json.loads(done.stdout)

    def request(body, okay=True):
        result = call('cu', 'proof', '--request', json.dumps(body, separators=(',', ':')))
        require(result.get('ok') is okay, f'proof result: {str(result)[:300]}')
        return result

    def mutate(op, **kwargs):
        nonlocal proof
        result = request({'op': op, 'proof_id': proof['proofId'], 'sequence': proof['sequence'], **kwargs})
        if not result.get('closed'):
            proof = result
        return result

    def create_owned_browser():
        nonlocal workspace, display
        display = call('display', 'create', '--width', '1280', '--height', '900')
        require(display.get('ok') is True, 'display create')
        attempt = 'cdn-external-test-' + uuid.uuid4().hex
        workspace = call('browser', 'create', f'http://127.0.0.1:{server.server_port}/', '--provider', 'cdp',
                         '--display-target', display['display_target'], '--profile-dir', str(root / ('profile-' + attempt)),
                         '--session', attempt)
        require(workspace.get('status') == 'ready', f'browser create: {workspace}')
        workspace = call('browser', 'acquire', workspace['id'], '--holder', attempt,
                         '--holder-kind', 'scout_cdn_capture', '--note', 'external proof acceptance')
        return {'op': 'begin', 'attempt_id': attempt, 'workspace_id': workspace['id'],
                'display_id': display['display_id'], 'display_target': display['display_target'],
                'capture_generation': display['capture_generation'],
                'job_sha256': hashlib.sha256(b'acceptance fixture, not candidate qualification').hexdigest()}

    def destroy_owned():
        nonlocal workspace, display
        if workspace:
            result = call('browser', 'close', workspace['id'], '--reason', 'external-proof-test-cleanup')
            require(result.get('status') == 'closed', 'browser close')
            pid = workspace['process_id']
            cutover.wait_for(lambda: not Path(f'/proc/{pid}').exists(), 'browser process remains', 10)
            profile = Path(workspace['profile_dir'])
            require(profile.is_relative_to(root), 'profile ownership')
            if profile.exists():
                shutil.rmtree(profile)
            workspace = None
        if display:
            result = call('display', 'destroy', str(display['display_id']), display['capture_generation'],
                          '--note', 'external-proof-test-cleanup')
            require(result.get('ok') is True, 'display destroy')
            display = None

    def listening():
        with socket.socket() as sock:
            return sock.connect_ex(('127.0.0.1', port)) == 0

    try:
        n = cutover.choose_foreign_display()
        foreign = subprocess.Popen(['Xvfb', f':{n}', '-screen', '0', '640x480x24', '-nolisten', 'tcp'],
                                   stdout=foreign_log, stderr=subprocess.STDOUT)
        cutover.wait_for(lambda: Path(f'/tmp/.X11-unix/X{n}').exists(), 'foreign Xvfb did not start')
        foreign_id = cutover.pid_signature(foreign.pid)
        # Same loopback-only, per-boot-token test transport used by repository CI.
        # This never changes the persistent service or its transport configuration.
        daemon = subprocess.Popen([str(binary), '--web', str(port), '--bind', '127.0.0.1', '--no-tls', '--no-presence'],
                                  env=env, cwd=root, stdin=subprocess.DEVNULL, stdout=daemon_log, stderr=subprocess.STDOUT)
        cutover.wait_for(listening, 'daemon not listening', 30)
        cutover.wait_for(lambda: call('status').get('phase') == 'idle', 'daemon not ready for authenticated control', 30)
        checks.append('isolated daemon with no provider credentials')
        display_baseline=call('display','list')
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Page)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        begin = create_owned_browser()
        rejected = request(dict(begin, capture_generation='vdcg-stale'), False)
        require(rejected['error']['code'] == 'bounded-cu-display-generation-mismatch', 'stale generation accepted')
        checks.append('stale display generation refused')
        proof = request(begin)
        # CDP readiness and a fresh frame do not imply the first page paint.
        # Wait for this fixture's pixels before sending any native input.
        for paint_attempt in range(16):
            paint_path=out/f'initial-paint-{paint_attempt}.png'
            paint_path.write_bytes(base64.b64decode(proof['frame']['pngBase64'],validate=True))
            pw,ph,pc,pr=cutover.decode_png(paint_path)
            painted=(pw,ph)==(1280,900) and all(tuple(pr[y][x*pc:x*pc+3])==(220,35,45) for x,y in [(600,500),(900,700),(400,400)])
            if painted:break
            require(paint_attempt<15,'fixture did not become paint-ready before input')
            mutate('actions',actions_json=json.dumps([{'type':'wait','ms':300}]))
        checks.append(f'fixture paint ready before input after {paint_attempt} waits')
        expected = {'binding': proof['binding'], 'actor': proof['actor'],
                    'preObservationSha256': hashlib.sha256(b'acceptance pre-observation fixture').hexdigest()}
        save(out / 'expected.json', expected)
        require(request(begin, False)['error']['code'] == 'external-proof-busy', 'duplicate attempt accepted')
        checks.append('exclusive attempt and display')
        seq = proof['sequence']
        bad_batch = [{'type': 'type', 'text': 'must-not-be-injected'}, {'type': 'paste', 'text': 'forbidden'}]
        rejected = request({'op': 'actions', 'proof_id': proof['proofId'], 'sequence': seq,
                            'actions_json': json.dumps(bad_batch)}, False)
        require(rejected['error']['code'] == 'external-proof-action-forbidden', 'invalid batch accepted')
        require(request({'op': 'status', 'proof_id': proof['proofId']})['sequence'] == seq, 'failed batch advanced')
        checks.append('invalid batch refused before any input')
        mutate('actions', actions_json=json.dumps([{'type': 'click', 'x': 180, 'y': 145}, {'type': 'wait', 'ms': 200},
                                                   {'type': 'type', 'text': 'proof-ready'}, {'type': 'wait', 'ms': 300}]))
        rejected = request({'op': 'actions', 'proof_id': proof['proofId'], 'sequence': 0,
                            'actions_json': '[{"type":"type","text":"must-not-run"}]'}, False)
        require(rejected['error']['code'] == 'external-proof-sequence', 'stale input replayed')
        checks.append('stale action sequence refused')
        mutate('freeze')
        rejected = request({'op': 'actions', 'proof_id': proof['proofId'], 'sequence': proof['sequence'],
                            'actions_json': '[{"type":"type","text":"must-not-run"}]'}, False)
        require(rejected['error']['code'] == 'external-proof-phase', 'input after freeze accepted')
        checks.append('irreversible input freeze')
        mutate('observe', pre_observation_sha256=expected['preObservationSha256'])
        (out / 'proof.png').write_bytes(base64.b64decode(proof['frame']['pngBase64'], validate=True))
        width, height, channels, rows = cutover.decode_png(out / 'proof.png')
        require((width, height) == (1280, 900), 'wrong PNG geometry')
        samples = [tuple(rows[y][x * channels:x * channels + 3]) for x, y in [(600, 500), (900, 700), (400, 400)]]
        input_effect_verified=all(rgb == (30,170,70) for rgb in samples)
        if input_effect_verified:checks.append('real native typing changed visible page pixels')
        rejected = request({'op': 'finish', 'proof_id': proof['proofId'], 'sequence': proof['sequence'],
                            'observation_sha256': '0' * 64, 'claims_json': '{}'}, False)
        require(rejected['error']['code'] == 'external-proof-observation', 'substituted frame accepted')
        checks.append('claims bind exact issued frame')
        mutate('finish', observation_sha256=proof['frame']['sha256'],
               claims_json=json.dumps({'acceptanceFixture': True, 'observedGreenAfterNativeTyping': input_effect_verified}, separators=(',', ':')))
        save(out / 'receipt.json', proof['receipt'])
        closed = mutate('close')
        save(out / 'close.json', closed)
        require(closed.get('cleanupComplete') is True, 'proof close failed')
        checks.append('receipt-bound native cleanup acknowledged')
        proof = None
        destroy_owned()
        if not args.skip_expiry:
            begin = create_owned_browser()
            proof = request(begin)
            mutate('freeze')
            proof_id = proof['proofId']
            time.sleep(46)
            rejected = request({'op': 'status', 'proof_id': proof_id}, False)
            require(rejected['error']['code'] == 'external-proof-not-found', 'abandoned proof did not expire')
            checks.append('abandoned frozen session expires and releases fence')
            proof = None
            destroy_owned()
        require(cutover.signature_live(foreign_id), 'foreign Xvfb changed')
        require(cutover.runner_snapshot() == runners, 'CI runner identities changed')
        checks.extend(['foreign Xvfb preserved', 'CI runner identities preserved'])
        require(call('display','list') == display_baseline, 'managed display inventory changed after cleanup')
        result = {'passed': input_effect_verified, 'inputEffectVerified': input_effect_verified, 'test': 'external-cu-proof-real-linux-v1', 'binarySha256': cutover.sha256_file(binary),
                  'checks': checks, 'runners': runners, 'foreign': foreign_id, 'pngSamples': samples, 'commands': commands}
        save(out / 'acceptance.json', result)
        print(json.dumps({key: value for key, value in result.items() if key != 'commands'}, indent=2))
        if not input_effect_verified:print(f'native input did not update page: {samples}',file=sys.stderr)
        return 0 if input_effect_verified else 1
    except Exception as error:
        save(out / 'acceptance.json', {'passed': False, 'error': str(error), 'checks': checks, 'commands': commands})
        print(error, file=sys.stderr)
        return 1
    finally:
        if proof:
            try:
                request({'op': 'abort', 'proof_id': proof['proofId'], 'sequence': proof['sequence']})
            except Exception:
                pass
        try:
            destroy_owned()
        except Exception as error:
            print('test cleanup:', error, file=sys.stderr)
        if server:
            server.shutdown()
            server.server_close()
        for child in [daemon, foreign]:
            if child and child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)
        daemon_log.close()
        foreign_log.close()
        shutil.rmtree(root)


if __name__ == '__main__':
    raise SystemExit(main())
