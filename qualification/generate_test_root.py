#!/usr/bin/env python3
"""Create the offline-only TUF root used by updater qualification."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import NamedTemporaryFile


ROOT = Path(__file__).resolve().parent
if len(sys.argv) > 2:
    raise SystemExit("usage: generate_test_root.py [output-directory]")
OUTPUT_ROOT = Path(sys.argv[1]).resolve() if len(sys.argv) == 2 else ROOT
KEY_DIR = OUTPUT_ROOT / "private"
KEY_PATH = KEY_DIR / "test-root-ed25519.pem"
ROOT_PATH = OUTPUT_ROOT / "test-root.json"


def canonical(value: object) -> bytes:
    return json.dumps(
        value, ensure_ascii=False, separators=(",", ":"), sort_keys=True
    ).encode("utf-8")


def run(*args: str, input_bytes: bytes | None = None) -> bytes:
    result = subprocess.run(
        args,
        input=input_bytes,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return result.stdout


def main() -> None:
    OUTPUT_ROOT.mkdir(parents=True, exist_ok=True)
    KEY_DIR.mkdir(mode=0o700, exist_ok=True)
    if ROOT_PATH.exists():
        raise SystemExit(f"refusing to overwrite qualification root: {ROOT_PATH}")
    if not KEY_PATH.exists():
        run("openssl", "genpkey", "-algorithm", "ED25519", "-out", str(KEY_PATH))
        KEY_PATH.chmod(0o600)

    public_der = run(
        "openssl", "pkey", "-in", str(KEY_PATH), "-pubout", "-outform", "DER"
    )
    public_key = public_der[-32:].hex()
    key = {
        "keytype": "ed25519",
        "keyval": {"public": public_key},
        "scheme": "ed25519",
    }
    key_id = hashlib.sha256(canonical(key)).hexdigest()
    signed = {
        "_type": "root",
        "consistent_snapshot": False,
        "expires": "2036-01-01T00:00:00Z",
        "keys": {key_id: key},
        "roles": {
            role: {"keyids": [key_id], "threshold": 1}
            for role in ("root", "targets", "snapshot", "timestamp")
        },
        "spec_version": "1.0.31",
        "version": 1,
    }
    with NamedTemporaryFile(dir=KEY_DIR) as message:
        message.write(canonical(signed))
        message.flush()
        signature = run(
            "openssl",
            "pkeyutl",
            "-sign",
            "-rawin",
            "-inkey",
            str(KEY_PATH),
            "-in",
            message.name,
        ).hex()
    envelope = {"signatures": [{"keyid": key_id, "sig": signature}], "signed": signed}
    ROOT_PATH.write_text(json.dumps(envelope, indent=2) + "\n", encoding="utf-8")
    ROOT_PATH.chmod(0o644)
    os.chmod(KEY_DIR, 0o700)
    print(f"wrote {ROOT_PATH}")
    print(f"wrote offline qualification key {KEY_PATH} (mode 0600)")
    print("This key is test-only; production must install its own root at the configured path.")


if __name__ == "__main__":
    main()
