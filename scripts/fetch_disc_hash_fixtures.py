#!/usr/bin/env python3
"""Build disc-hash fixtures by fetching matching disc##.json + disc##.txt
from TheDiscDb/data via the gh CLI."""
import base64
import hashlib
import json
import struct
import subprocess
import sys
from pathlib import Path

REPO = "TheDiscDb/data"
OUT = Path("/home/rob/projects/3drip/tests/fixtures/disc_hash")

def gh_fetch(repo_path):
    proc = subprocess.run(
        ["gh", "api", f"repos/{REPO}/contents/{repo_path}", "--jq", ".content"],
        capture_output=True, text=True
    )
    if proc.returncode != 0:
        raise RuntimeError(f"gh failed: {proc.stderr}")
    return base64.b64decode(proc.stdout).decode("utf-8", errors="replace")

def build_fixture(repo_dir, disc_stub, label, fmt_kind):
    txt = gh_fetch(f"{repo_dir}/{disc_stub}.txt")
    js  = gh_fetch(f"{repo_dir}/{disc_stub}.json")
    files = []
    for line in txt.splitlines():
        if line.startswith("HSH:"):
            parts = line[4:].rstrip().split(",")
            files.append({
                "index": int(parts[0]),
                "name": parts[1],
                "size": int(parts[-1]),
            })
    files.sort(key=lambda f: f["index"])
    disc = json.loads(js)
    expected = disc.get("ContentHash")
    if not files or not expected:
        print(f"  skip {label}: files={len(files)} hash={expected}", file=sys.stderr)
        return False
    # Verify locally before writing.
    h = hashlib.md5()
    for f in files:
        h.update(struct.pack("<q", f["size"]))
    computed = h.hexdigest().upper()
    if computed != expected:
        print(f"  MISMATCH {label}: {computed} != {expected}", file=sys.stderr)
        return False
    out_path = OUT / f"{label}.json"
    payload = {
        "label": label,
        "format": fmt_kind,
        "source": f"{repo_dir}/{disc_stub}",
        "expected_hash": expected,
        "file_count": len(files),
        "files": files,
    }
    out_path.write_text(json.dumps(payload, indent=2) + "\n")
    print(f"  wrote {out_path.name}: {len(files)} files -> {expected}")
    return True

FIXTURES = [
    ("data/movie/1917 (2019)/2020-4k", "disc01", "1917_2020_4k_disc01", "uhd"),
    ("data/movie/1917 (2019)/2020-4k", "disc02", "1917_2020_4k_disc02", "bluray"),
    ("data/movie/Logan (2017)/2017-4k", "disc03", "logan_2017_4k_disc03", "uhd"),
    ("data/movie/The Sandlot 2 (2005)/2005-multiformat-dvd", "disc01", "sandlot2_2005_dvd_disc01", "dvd"),
    ("data/series/Damages (2007)/2019-blu-ray", "disc08", "damages_2019_bd_disc08", "bluray"),
]

OUT.mkdir(parents=True, exist_ok=True)
ok = 0
for repo_dir, disc_stub, label, fmt in FIXTURES:
    print(f"fetching {label}...")
    try:
        if build_fixture(repo_dir, disc_stub, label, fmt):
            ok += 1
    except Exception as e:
        print(f"  error: {e}", file=sys.stderr)
print(f"{ok}/{len(FIXTURES)} fixtures written")
