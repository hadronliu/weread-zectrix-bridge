#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p dist
python3 - <<'PYTHON'
from pathlib import Path
from zipfile import ZipFile, ZIP_DEFLATED
root=Path.cwd()
with ZipFile(root/'dist/weread-zectrix-bridge-source.zip','w',ZIP_DEFLATED) as z:
    for name in ['src','browser-controller','web','vendor','scripts','docs','.github','Cargo.toml','Cargo.lock','README.md','TUTORIAL.md','LICENSE','THIRD_PARTY_NOTICES.md','.gitignore','.env.example']:
        p=root/name
        for f in ([p] if p.is_file() else sorted(p.rglob('*'))):
            rel=f.relative_to(root)
            if not f.is_file() or any(x in {'node_modules','.user-data','target','.git','__pycache__'} for x in rel.parts): continue
            if any(x.startswith('.last-') for x in rel.parts) or f.name in {'.reader-dom.html','ink-preview.png','.DS_Store','settings.json'} or f.suffix=='.log': continue
            z.write(f,Path(root.name)/rel)
print('dist/weread-zectrix-bridge-source.zip')
PYTHON
