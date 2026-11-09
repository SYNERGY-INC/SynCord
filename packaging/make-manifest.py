#!/usr/bin/env python3
"""Write latest.json for automatic updates and sign every installer.

Usage: make-manifest.py <version> <dist dir> <notes file>

The private signing key comes from the SIGNING_KEY environment variable (the
SYNCORD_UPDATE_KEY GitHub secret, an Ed25519 PEM key). Without it the manifest
is still written, but apps and servers won't install unsigned updates.
"""
import base64, hashlib, json, os, subprocess, sys, tempfile

version, dist, notes_file = sys.argv[1], sys.argv[2], sys.argv[3]
repo = os.environ.get("GITHUB_REPOSITORY", "bxpo/SynCord")
key_pem = os.environ.get("SIGNING_KEY", "").strip()

wanted = {
    "windows": f"Celerity-Setup-{version}.exe",
    "windows-server": f"Celerity-Server-Setup-{version}.exe",
    "macos-app": f"Celerity-{version}-macOS.dmg",
    # SynCord 0.6 and older look for SynCord.app under the "macos" key.
    "macos": f"Celerity-{version}-macOS-update-from-SynCord.dmg",
    "linux": f"celerity-{version}-linux-x86_64.tar.gz",
    "linux-deb": f"celerity_{version}_amd64.deb",
}

key_path = None
if key_pem:
    fd, key_path = tempfile.mkstemp()
    with os.fdopen(fd, "w") as f:
        f.write(key_pem + "\n")
else:
    print("::warning::SYNCORD_UPDATE_KEY isn't set: this release won't be installed automatically.")

def sign(path):
    if not key_path:
        return None
    sig = subprocess.run(
        ["openssl", "pkeyutl", "-sign", "-rawin", "-inkey", key_path, "-in", path],
        check=True, capture_output=True,
    ).stdout
    return base64.b64encode(sig).decode()

files = {}
try:
    for key, name in wanted.items():
        path = os.path.join(dist, name)
        if not os.path.exists(path):
            print(f"::warning::{name} is missing from this release")
            continue
        data = open(path, "rb").read()
        files[key] = {
            "name": name,
            "url": f"https://github.com/{repo}/releases/download/v{version}/{name}",
            "sha256": hashlib.sha256(data).hexdigest(),
            "size": len(data),
            "signature": sign(path),
        }
finally:
    if key_path:
        os.remove(key_path)

notes = open(notes_file).read().split("## Which file do I need?")[0].strip() if os.path.exists(notes_file) else ""
manifest = {
    "version": version,
    "notes": notes,
    "page": f"https://github.com/{repo}/releases/tag/v{version}",
    "files": files,
}
with open(os.path.join(dist, "latest.json"), "w") as f:
    json.dump(manifest, f, indent=2)
print(f"latest.json: {len(files)} files, signed: {bool(key_path)}")
