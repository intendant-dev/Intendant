#!/usr/bin/env python3
"""Build-free guard against application policy returning to shared browser/CU code."""
import argparse
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
# This file is the sole exception: these negative examples are the regression
# rule, never an extension approval or a runtime/catalog dependency.
FORBIDDEN = re.compile(
    r'(?<!schema-)\bscout\b|rabby|scout_cdn|'
    r'cdn[-_](?:attempt|capture|cu|proof|linux)|'
    r'daf7819d7371a67ef447c788e899b1df628f95e380a460c6e5dd3b86bbe09e4f|'
    r'\b0\.94\.6\b|\b16_?216_?742\b|'
    r'APPROVED_BROWSER_EXTENSION_(?:SHA256|BYTE_LENGTH|VERSION|SERVICE_WORKER)|'
    r'proof_viewport|(?:120\s*(?:\.\.=|[–-]|<=).*159)|'
    r'--min-display-id\s+120\s+--max-display-id\s+159', re.IGNORECASE)
SUFFIXES = {'.rs', '.md', '.py', '.json', '.toml', '.yaml', '.yml', '.sh', '.js', '.cjs', '.mjs', '.txt', '.html'}
SCOPES = ('src/', 'crates/', 'scripts/', 'tests/', 'docs/', 'skills/', '.github/', 'static/app/')


def violations(path, text):
    found = []
    if FORBIDDEN.search(path):
        found.append(f'{path}: application-specific filename')
    for line, value in enumerate(text.splitlines(), 1):
        # A standard dictionary contains the ordinary English noun, not policy.
        if path == 'static/app/32-vault-custody.js' and value.startswith("const VAULT_BIP39_WORDLIST = '"):
            continue
        if FORBIDDEN.search(value):
            found.append(f'{path}:{line}: application policy or fixture coupling')
    if path.startswith('src/bin/caller/browser_workspace') or path == 'docs/src/browser-extensions.md':
        for literal in re.findall(r'\b[0-9a-fA-F]{64}\b', text):
            # Repeated-digit examples are synthetic; approved real hashes must
            # come only from the owner-supplied startup snapshot.
            if len(set(literal.lower())) > 1:
                found.append(f'{path}: hardcoded archive identity in generic browser code/docs')
    return found


def self_test():
    for bad in ['SCOUT_CDN_LEASE_KIND', 'scout_cdn_capture', 'Rabby', 'cdn-attempt:demo',
                'cdn-linux-cutover', '0.94.6', '16_216_742', '16216742',
                'APPROVED_BROWSER_EXTENSION_SHA256', 'mod proof_viewport;',
                '--min-display-id 120 --max-display-id 159']:
        assert violations('src/test.rs', bad), bad
    for good in ['schema-scout', 'https://cdn.example.net/library.js', 'content-delivery-network',
                 'bounded_cu', 'intendant-external-cu-proof-v1', 'worker.js', '170..=179']:
        assert not violations('src/test.rs', good), good
    assert violations('scripts/test-rabby.py', '')
    assert violations('src/bin/caller/browser_workspace/extension_policy.rs', '0123456789abcdef' * 4)
    assert not violations('src/bin/caller/browser_workspace.rs', 'a' * 64)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        self_test()
    names = subprocess.check_output(['git', 'ls-files', '--cached', '--others', '--exclude-standard', '-z'], cwd=ROOT)
    errors, scanned = [], 0
    for name in sorted(set(names.decode().split('\0'))):
        path = ROOT / name
        if not name.startswith(SCOPES) or path.suffix not in SUFFIXES or path == Path(__file__).resolve() or not path.is_file():
            continue
        errors.extend(violations(name, path.read_text(encoding='utf-8')))
        scanned += 1
    if errors:
        print('\n'.join(errors))
        return 1
    print(f'Browser/application coupling guard passed ({scanned} files).')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
