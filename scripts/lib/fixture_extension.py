"""Deterministic harmless MV3 archives for local browser acceptance only."""
import hashlib
import json
from pathlib import Path
import zipfile


def create(path: Path, name: str, worker: str = 'worker.js', version: str = '1.0.0') -> dict:
    """Create an onboarding + service-worker + offscreen fixture without network permissions."""
    manifest = {'manifest_version': 3, 'name': 'Local fixture ' + name, 'version': version,
                'permissions': ['offscreen'], 'background': {'service_worker': worker}}
    script = '''self.fixtureName = NAME;
chrome.runtime.onMessage.addListener((message, sender, reply) => {
  if (message.fixturePing) reply({fixture: self.fixtureName});
});
chrome.runtime.onInstalled.addListener(async () => {
  await chrome.offscreen.createDocument({url: 'offscreen.html', reasons: ['DOM_PARSER'],
    justification: 'Parse a local fixture document for browser acceptance'});
  await chrome.tabs.create({url: chrome.runtime.getURL('onboarding.html')});
});
'''.replace('NAME', json.dumps(name))
    view_script = '''async function check() {
  const reply = await chrome.runtime.sendMessage({fixturePing: true});
  const parsed = new DOMParser().parseFromString('<p>ready:' + reply.fixture + '</p>', 'text/html');
  document.body.textContent = parsed.body.textContent;
}
check(); setInterval(check, 1000);
'''
    files = {'manifest.json': json.dumps(manifest, sort_keys=True).encode(), worker: script.encode(),
             'view.js': view_script.encode(), 'theme+base.css': b'body { font-family: sans-serif; }'}
    for view in ('onboarding', 'offscreen'):
        files[view + '.html'] = ('<!doctype html><meta charset="utf-8"><title>Local ' + view +
                                '</title><body>waiting<script src="view.js"></script>').encode()
    with zipfile.ZipFile(path, 'w') as archive:
        for filename, data in sorted(files.items()):
            info = zipfile.ZipInfo(filename, date_time=(2020, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.create_system = 3
            info.external_attr = 0o100644 << 16
            archive.writestr(info, data)
    raw = path.read_bytes()
    return {'archive_sha256': hashlib.sha256(raw).hexdigest(), 'archive_byte_length': len(raw),
            'manifest_version': 3, 'version': version, 'service_worker': worker}
