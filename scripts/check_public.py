#!/usr/bin/env python3
"""Reject accidentally staged runtime secrets without echoing their values."""
import json
import re
import subprocess
from pathlib import Path

from dev_env import DATA, ROOT


def secret_values(value, key=""):
    if isinstance(value, dict):
        for name, child in value.items():
            yield from secret_values(child, name)
    elif isinstance(value, list):
        for child in value:
            yield from secret_values(child, key)
    elif isinstance(value, str) and any(word in key.lower() for word in ("password", "token", "api_key", "passphrase")):
        if len(value) >= 8:
            yield value.encode()


def pattern_violations(data, path):
    text = data.decode("utf-8", errors="replace")
    findings = []
    tax = re.compile(r"(?<![A-Z0-9])[0-9][0-9A-HJ-NPQRTUWXY]{17}(?![A-Z0-9])")
    if any(match.group() != "91440300XXXXXXXX0A" for match in tax.finditer(text)):
        findings.append("tax_id")
    if re.search(r"(?<![0-9])1[3-9][0-9]{9}(?![0-9])", text):
        findings.append("phone")
    for match in re.finditer(r"[A-Za-z0-9_.+%-]+@([A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+)", text):
        domain = match.group(1).lower()
        if not (domain.endswith(".example") or domain == "reimb.local"):
            findings.append("email")
    if "fixtures" not in path.parts and re.search(r"(?<![0-9])[0-9]{20}(?![0-9])", text):
        findings.append("invoice_number")
    return sorted(set(findings))


def main():
    local_secrets = []
    for path in (DATA / "credentials.json", DATA / "octos-smoke/profiles/reimb-smoke.json"):
        if path.exists():
            local_secrets.extend(secret_values(json.loads(path.read_text())))
    paths = subprocess.check_output(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=ROOT).split(b"\0")
    violations = []
    for name in set(paths):
        if not name:
            continue
        path = ROOT / name.decode()
        if not path.is_file():
            continue
        data = path.read_bytes()
        if any(secret in data for secret in local_secrets) or re.search(rb"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----", data):
            violations.append({"file": name.decode(), "rules": ["secret"]})
        rules = pattern_violations(data, Path(name.decode()))
        if rules:
            violations.append({"file": name.decode(), "rules": rules})
    print(json.dumps({"status": "failed" if violations else "passed", "files_with_secret_matches": violations}))
    return bool(violations)


if __name__ == "__main__":
    raise SystemExit(main())
