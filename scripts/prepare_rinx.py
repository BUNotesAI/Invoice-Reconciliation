#!/usr/bin/env python3
"""Prepare two isolated native demo apps with real Matrix login sessions."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import secrets
import shutil
import subprocess

from dev_env import DATA, BASE, VERSIONS, api, private_json, private_write


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--checkout", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    commit = subprocess.check_output(["git", "-C", str(args.checkout), "rev-parse", "HEAD"], text=True).strip()
    if commit != VERSIONS["rinx_commit"]:
        raise RuntimeError("Rinx checkout does not match the pin")
    if not args.binary.is_file():
        raise RuntimeError("Build the pinned Rinx binary first")
    binary_hash = hashlib.sha256(args.binary.read_bytes()).hexdigest()
    os.umask(0o077)
    credentials = json.loads((DATA / "credentials.json").read_text())
    for role, localpart in (("Applicant", "reimb-linyi"), ("Finance", "reimb-zhoumin")):
        data = DATA / "rinx" / role.lower()
        data.mkdir(parents=True, exist_ok=True)
        account = credentials["accounts"][localpart]
        persistent = data / account["user_id"].replace(":", "_").replace("@", "") / "persistent_state"
        persistent.mkdir(parents=True, exist_ok=True)
        if not (persistent / "session").exists():
            login = api("POST", "/_matrix/client/v3/login", {
                "type": "m.login.password", "identifier": {"type": "m.id.user", "user": localpart},
                "password": account["password"], "initial_device_display_name": "Reimb Rinx " + role})
            private_json(persistent / "session", {
                "client_session": {"homeserver": BASE, "db_path": "matrix-store", "passphrase": secrets.token_urlsafe(32)},
                "user_session": {"user_id": login["user_id"], "device_id": login["device_id"],
                                 "access_token": login["access_token"]},
                "sliding_sync_version": "Native"})
            private_write(data / "latest_user_id.txt", account["user_id"])
        bundle = DATA / "apps" / f"Reimb {role}.app" / "Contents"
        (bundle / "MacOS").mkdir(parents=True, exist_ok=True)
        info = {"CFBundleIdentifier": "local.reimb." + role.lower(), "CFBundleName": "Reimb " + role,
                "CFBundleExecutable": "robrix", "CFBundlePackageType": "APPL", "CFBundleVersion": "1",
                "LSEnvironment": {"ROBRIX_DATA_DIR": str(data), "NO_PROXY": "127.0.0.1,localhost",
                                  "no_proxy": "127.0.0.1,localhost"}}
        (bundle / "Info.plist").write_bytes(plistlib.dumps(info))
        binary = bundle / "MacOS" / "robrix"
        if not binary.exists():
            shutil.copy2(args.binary, binary)
            binary.chmod(0o700)
        elif hashlib.sha256(binary.read_bytes()).hexdigest() != binary_hash:
            raise RuntimeError("Existing app binary differs; stop the app before replacing its bundle")
        print(json.dumps({"role": role, "app": str(bundle.parent), "rinx_commit": commit, "binary_sha256": binary_hash}))


if __name__ == "__main__":
    main()
