#!/usr/bin/env python3
"""Hermetic local MV3/viewport smoke. Uses an existing local Chrome for Testing; downloads nothing."""
import argparse
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

sys.path.insert(0, str(Path(__file__).parent / 'lib'))
from fixture_extension import create as create_fixture

spec = importlib.util.spec_from_file_location('isolation', Path(__file__).with_name('test-linux-display-cutover.py'))
isolation = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = isolation
spec.loader.exec_module(isolation)
require = isolation.require

# All CDP reads are against this smoke's exact browser child and local fixtures.
# Onboarding and offscreen must actually exchange a message with the approved
# worker, not merely appear in /json/list. No external site or script is used.
CHECK_JS = r'''
const [port,id,runtime,worker,name] = process.argv.slice(1);
async function call(target,method,params={}) {
  const endpoint = new URL(target.webSocketDebuggerUrl);
  if(endpoint.hostname !== '127.0.0.1' || endpoint.port !== port ||
     endpoint.pathname !== `/devtools/page/${target.id}`) throw Error('foreign CDP endpoint');
  const ws = new WebSocket(endpoint);
  try { return await new Promise((resolve,reject) => {
    const timer = setTimeout(()=>reject(Error('CDP deadline')),3000);
    ws.onerror = ()=>{clearTimeout(timer);reject(Error('CDP failed'));};
    ws.onopen = ()=>ws.send(JSON.stringify({id:1,method,params}));
    ws.onmessage = event=>{
      const v=JSON.parse(event.data);if(v.id!==1)return;clearTimeout(timer);
      if(v.error)reject(Error('CDP refused'));else resolve(v.result);
    };
  }); } finally {ws.close();}
}
let result;
for(let attempt=0;attempt<40;attempt++) {
  const targets=await(await fetch(`http://127.0.0.1:${port}/json/list`,{signal:AbortSignal.timeout(3000)})).json();
  const prefix=`chrome-extension://${runtime}/`;
  if(targets.some(t=>t.url?.startsWith('chrome-extension:')&&!t.url.startsWith(prefix)))throw Error('foreign extension target');
  const page=targets.find(t=>t.id===id&&t.type==='page');
  if(!page||!page.url.startsWith('http://127.0.0.1:'))throw Error('bound local application missing or onboarding selected');
  const service=targets.find(t=>t.type==='service_worker'&&t.url===prefix+worker);
  const onboarding=targets.find(t=>t.type==='page'&&t.url===prefix+'onboarding.html');
  const offscreen=targets.find(t=>t.url===prefix+'offscreen.html');
  if(service&&onboarding&&offscreen) {
    const a=await call(onboarding,'Runtime.evaluate',{expression:'document.body.textContent',returnByValue:true});
    const b=await call(offscreen,'Runtime.evaluate',{expression:'document.body.textContent',returnByValue:true});
    if(a.result?.value===`ready:${name}`&&b.result?.value===`ready:${name}`) {
      const metrics=await call(page,'Page.getLayoutMetrics');
      const css=metrics.cssLayoutViewport, device=metrics.layoutViewport;
      if(css.clientWidth!==1024||css.clientHeight!==768||device.clientWidth!==1024||device.clientHeight!==768)throw Error('exact viewport or scale mismatch');
      result={onboarding:true,offscreen:true,serviceWorker:worker,runtimeId:runtime,metrics};break;
    }
  }
  await new Promise(resolve=>setTimeout(resolve,150));
}
if(!result)throw Error('local extension did not complete onboarding/offscreen worker roundtrip');
console.log(JSON.stringify(result));
'''


class Page(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'<!doctype html><meta charset="utf-8"><title>Local browser fixture</title><h1>Ready</h1>'
        self.send_response(200)
        self.send_header('Content-Type', 'text/html')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin', required=True, type=Path)
    parser.add_argument('--browser-dir', required=True, type=Path, help='existing unpacked Linux Chrome for Testing directory containing chrome')
    parser.add_argument('--node', required=True, type=Path, help='Node 22+ executable')
    parser.add_argument('--evidence', required=True, type=Path)
    args = parser.parse_args()
    require(sys.platform.startswith('linux'), 'Linux display binding required')
    binary, browser, node = args.bin.resolve(strict=True), args.browser_dir.resolve(strict=True), args.node.resolve(strict=True)
    require((browser / 'chrome').is_file() and os.access(browser / 'chrome', os.X_OK), 'local Chrome for Testing runtime missing')
    out = args.evidence.absolute()
    require(not out.exists(), 'evidence path must be new')
    runners = isolation.runner_snapshot()
    if os.environ.get('GITHUB_ACTIONS') != 'true':
        require(not any(r['comm'] == 'Runner.Worker' for r in runners), 'CI has priority')
    os.umask(0o077)
    out.mkdir(mode=0o700, parents=True)
    root = Path(tempfile.mkdtemp(prefix='intendant-extension-fixture-'))
    home, cache = root / 'home', root / 'cache'
    home.mkdir(mode=0o700)
    managed = cache / 'intendant/browser-workspaces'
    managed.mkdir(parents=True, mode=0o700)
    # The supplied browser runtime is read-only input. Only our private cache
    # contains this symlink; no installed cache or daemon configuration changes.
    (managed / 'fixture-runtime').symlink_to(browser, target_is_directory=True)
    env = {'HOME': str(home), 'INTENDANT_HOME': str(home / '.intendant'), 'XDG_CACHE_HOME': str(cache),
           'PATH': os.environ.get('PATH', '/usr/bin:/bin'), 'LANG': 'C.UTF-8',
           'INTENDANT_MESSAGE_SEARCH_DISABLE_GLOBAL': '1'}
    fixtures = []
    for name, worker, version in [('alpha', 'worker.js', '1.0.0'), ('beta', 'background/observer.js', '2.0.0'), ('foreign', 'foreign.js', '3.0.0')]:
        archive = root / (name + '.zip')
        fixtures.append((name, archive, create_fixture(archive, name, worker, version)))
    policy = root / 'approved-extensions.json'
    policy.write_text(json.dumps({'schema_version': 1, 'extensions': [f[2] for f in fixtures[:2]]}, sort_keys=True))
    policy_pin = hashlib.sha256(policy.read_bytes()).hexdigest()
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
    daemon = workspace = display = server = None
    log = (out / 'daemon.log').open('wb')
    checks, cleanup_errors = [], []
    passed = False

    def ctl(*argv):
        result = subprocess.run([str(binary), 'ctl', '--port', str(port), '--json', *argv],
                                env=env, cwd=root, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=35)
        require(result.returncode == 0, f'control failed: {result.stderr[:200]}')
        return json.loads(result.stdout)

    def launch(pin=None):
        nonlocal daemon
        argv = [str(binary), '--web', str(port), '--bind', '127.0.0.1', '--no-tls', '--no-presence']
        if pin is not None:
            argv += ['--browser-extension-policy', str(policy), '--browser-extension-policy-sha256', pin]
        daemon = subprocess.Popen(argv, env=env, cwd=root, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
        isolation.wait_for(lambda: daemon.poll() is None and ctl('status').get('phase') == 'idle', 'isolated daemon not ready', 30)

    def stop():
        nonlocal daemon
        if daemon is not None:
            daemon.terminate()
            try:
                daemon.wait(timeout=15)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=5)
            daemon = None

    def create(fixture, label):
        _, archive, identity = fixture
        return ctl('browser', 'create', f'http://127.0.0.1:{server.server_port}/', '--provider', 'cdp',
                   '--display-target', display['display_target'], '--session', label,
                   '--profile-dir', str(root / label), '--viewport', '1024x768',
                   '--extension-archive', str(archive), '--extension-sha256', identity['archive_sha256'],
                   '--extension-bytes', str(identity['archive_byte_length']), '--extension-manifest-version', '3',
                   '--extension-version', identity['version'])

    def retire():
        nonlocal workspace, display
        if workspace is not None:
            before = workspace
            require(ctl('browser', 'close', before['id'], '--reason', 'local-fixture-smoke')['status'] == 'closed', 'browser cleanup failed')
            isolation.wait_for(lambda: not Path(f"/proc/{before['process_id']}").exists(), 'owned browser process leaked', 10)
            require(not Path(before['profile_dir']).exists(), 'private profile leaked')
            require(not Path(before['extension']['load_path']).parent.exists(), 'private extension tree leaked')
            workspace = None
        if display is not None:
            require(ctl('display', 'destroy', str(display['display_id']), display['capture_generation'], '--note', 'local-fixture-smoke')['ok'], 'exact display cleanup failed')
            display = None

    try:
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Page)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        launch()  # No approval, even a valid caller-computed digest is denied.
        display = ctl('display', 'create', '--width', '1920', '--height', '1080', '--min-display-id', '170', '--max-display-id', '179')
        denied = create(fixtures[0], 'no-policy')
        require(denied.get('ok') is False and 'not approved' in str(denied), 'default extension policy did not deny')
        retire()
        stop()
        checks.append('no-policy denies valid caller identity')
        launch(policy_pin)
        baseline = ctl('display', 'list')
        # Changing the file after startup must not add a third approval.
        policy.write_text(json.dumps({'schema_version': 1, 'extensions': [f[2] for f in fixtures]}))
        for fixture in fixtures[:2]:
            name, _, identity = fixture
            display = ctl('display', 'create', '--width', '1920', '--height', '1080', '--min-display-id', '170', '--max-display-id', '179')
            require(display.get('ok') is True, 'display create failed')
            require(170 <= display['display_id'] <= 179, 'display escaped example range')
            denied = create(fixtures[2], 'unapproved-' + name)
            require(denied.get('ok') is False and 'not approved' in str(denied), 'foreign extension or live-file approval was accepted')
            workspace = create(fixture, 'fixture-' + name)
            require(workspace.get('status') == 'ready', f'fixture browser failed: {workspace}')
            ext = workspace['extension']
            for key, value in identity.items():
                require(ext[key] == value, 'workspace differs from approved archive')
            checked = subprocess.run([str(node), '--input-type=module', '-e', CHECK_JS, str(workspace['debugging_port']),
                                      workspace['active_target_id'], ext['runtime_id'], ext['service_worker'], name],
                                     env=env, cwd=root, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=35)
            require(checked.returncode == 0, f'fixture CDP check failed: {checked.stderr[:500]}')
            result = json.loads(checked.stdout)
            (out / (name + '.json')).write_text(json.dumps({'workspace': workspace, 'check': result}, indent=2) + '\n')
            checks.append(name + ': exact archive/manifest/runtime, onboarding, offscreen roundtrip, viewport, foreign rejection')
            retire()
            require(ctl('display', 'list') == baseline, 'display leaked')
        require(isolation.runner_snapshot() == runners, 'CI runner interference')
        passed = True
    except Exception as error:
        checks.append('FAIL: ' + str(error))
        print(error, file=sys.stderr)
    finally:
        try:
            retire()
        except Exception as error:
            cleanup_errors.append(str(error))
        stop()
        if server:
            server.shutdown()
            server.server_close()
        log.close()
        if not cleanup_errors:
            shutil.rmtree(root)
        receipt = {'passed': passed and not cleanup_errors, 'checks': checks, 'cleanupErrors': cleanup_errors,
                   'testDaemonStopped': daemon is None, 'binarySha256': isolation.sha256_file(binary),
                   'retainedScratch': str(root) if cleanup_errors else None}
        (out / 'RESULT.json').write_text(json.dumps(receipt, indent=2) + '\n')
        print(json.dumps(receipt), flush=True)
    return 0 if receipt['passed'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
