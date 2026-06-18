#!/usr/bin/env python3
"""Register a swerve release with lyku.org/apps via the registerAppRelease mapi route.

Lyku's gateway speaks MessagePack; this hand-encodes the small request map (no deps) and
POSTs it with the X-CI-Token header the handler validates against CI_RELEASE_TOKEN.

  register-release.py <app> <platform> <version> <path> <size> [commitSha]
  env: REGISTER_URL (default https://api.lyku.org/register-app-release), CI_RELEASE_TOKEN
"""
import os
import sys
import urllib.request


def mp(obj):
    """Minimal MessagePack encoder for str / int / bool / dict (small maps)."""
    if isinstance(obj, bool):
        return bytes([0xC3 if obj else 0xC2])
    if isinstance(obj, str):
        b = obj.encode("utf-8")
        n = len(b)
        if n < 32:
            return bytes([0xA0 | n]) + b
        if n < 256:
            return bytes([0xD9, n]) + b
        return bytes([0xDA, (n >> 8) & 0xFF, n & 0xFF]) + b
    if isinstance(obj, int):
        if 0 <= obj < 128:
            return bytes([obj])
        return bytes([0xCF]) + obj.to_bytes(8, "big")  # uint64
    if isinstance(obj, dict):
        n = len(obj)
        out = bytes([0x80 | n]) if n < 16 else bytes([0xDE, (n >> 8) & 0xFF, n & 0xFF])
        for k, v in obj.items():
            out += mp(k) + mp(v)
        return out
    raise TypeError(type(obj))


def main():
    app, platform, version, path, size = sys.argv[1:6]
    sha = sys.argv[6] if len(sys.argv) > 6 else ""
    payload = {
        "app": app,
        "platform": platform,
        "version": version,
        "path": path,
        "size": int(size),
    }
    if sha:
        payload["commitSha"] = sha

    url = os.environ.get("REGISTER_URL", "https://api.lyku.org/register-app-release")
    token = os.environ.get("CI_RELEASE_TOKEN", "")
    req = urllib.request.Request(
        url,
        data=mp(payload),
        method="POST",
        headers={"Content-Type": "application/x-msgpack", "X-CI-Token": token},
    )
    with urllib.request.urlopen(req, timeout=20) as resp:
        print(f"registerAppRelease {resp.status}: {resp.read()[:200]!r}")


if __name__ == "__main__":
    main()
