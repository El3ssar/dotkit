#!/usr/bin/env python3
"""Fake provider artifact metadata for isolated CLI tests; never downloads anything."""
import json
import os
from pathlib import Path

config = Path('mise.toml').read_text()
result = ['lockfile_version = 3']
for line in config.splitlines():
    if ' = ' not in line:
        continue
    name, version = (json.loads(s.strip()) for s in line.split(' = ', 1))
    result.extend([f'[[tools.{json.dumps(name)}]]', f'version = {json.dumps(version)}'])
    for platform in ('macos-arm64', 'macos-x64', 'linux-x64', 'linux-arm64'):
        if platform == os.environ.get('KIT_TEST_MISSING_PLATFORM'):
            continue
        result.extend([f'[tools.{json.dumps(name)}."platforms.{platform}"]',
                       'url = "https://example.invalid/fixture.tar.gz"',
                       'checksum = "sha256:' + 'a' * 64 + '"'])
Path('mise.lock').write_text('\n'.join(result) + '\n')
