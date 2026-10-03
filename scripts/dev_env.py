#!/usr/bin/env python3
"""Provision isolated local demo services; never print credentials."""
import json
import os
from pathlib import Path
import secrets
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
VERSIONS = json.loads((ROOT / "scripts/versions.json").read_text())
DATA = Path(os.environ.get("REIMB_DATA", Path.home() / ".reimb-demo")).resolve()
NAMESPACE = "reimb-demo"
BASE = "http://127.0.0.1:18128"
SERVER = "reimb.local"
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def private_json(path, value):
    private_write(path, json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def private_write(path, text):
    with open(path, "w", opener=lambda name, flags: os.open(name, flags, 0o600)) as handle:
        handle.write(text)
    path.chmod(0o600)


def docker(*args, check=True):
    result = subprocess.run(["docker", *args], capture_output=True, text=True, timeout=180)
    if check and result.returncode:
        raise RuntimeError(f"Docker operation failed: {args[0]}; exit={result.returncode}")
    return result


def request(method, path, body=None, token=None):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = "Bearer " + token
    payload = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(BASE + path, data=payload, headers=headers, method=method)
    try:
        with HTTP.open(req, timeout=20) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        try:
            return error.code, json.load(error)
        except json.JSONDecodeError:
            return error.code, {"errcode": "NON_JSON_RESPONSE"}


def api(method, path, body=None, token=None):
    status, result = request(method, path, body, token)
    if not 200 <= status < 300:
        raise RuntimeError(f"Matrix operation failed: HTTP {status}, {result.get('errcode', 'unknown')}")
    return result


def wait_for(check, label, seconds=90):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            if check():
                return
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(1)
    raise RuntimeError(f"Readiness timeout: {label}")


def ensure_container(name, image, options):
    found = docker("container", "inspect", name, check=False)
    if found.returncode:
        docker("run", "-d", "--name", name, "--label", "reimb.owner=invoice-reconciliation",
               *options, image)
    else:
        info = json.loads(found.stdout)[0]
        if (info["Config"].get("Labels") or {}).get("reimb.owner") != "invoice-reconciliation":
            raise RuntimeError(f"Refusing existing container without ownership label: {name}")
        if info["Config"]["Image"] != image:
            raise RuntimeError(f"Pinned image mismatch: {name}")
        if not info["State"]["Running"]:
            docker("start", name)


def provision_services(config):
    network = docker("network", "inspect", NAMESPACE, check=False)
    if network.returncode:
        docker("network", "create", "--label", "reimb.owner=invoice-reconciliation", NAMESPACE)
    elif json.loads(network.stdout)[0].get("Labels", {}).get("reimb.owner") != "invoice-reconciliation":
        raise RuntimeError("Refusing network without ownership label")
    for name in ("postgres", "media"):
        (DATA / name).mkdir(exist_ok=True)
    private_write(DATA / "postgres.env",
                  f"POSTGRES_USER=palpo\nPOSTGRES_DB=palpo\nPOSTGRES_PASSWORD={config['db_password']}\n")
    private_write(DATA / "palpo.toml", f'''server_name = "{SERVER}"
allow_registration = true
registration_token = "{config['registration_token']}"
rc_login = {{ per_second = 0.1, burst = 20 }}
rc_registration = {{ per_second = 0, burst = 1 }}
rc_message = {{ per_second = 0, burst = 1 }}
[[listeners]]
address = "0.0.0.0:8008"
[db]
url = "postgres://palpo:{config['db_password']}@{NAMESPACE}-db:5432/palpo"
[well_known]
client = "{BASE}"
''')
    ensure_container(NAMESPACE + "-db", VERSIONS["postgres_image"], [
        "--network", NAMESPACE, "--env-file", str(DATA / "postgres.env"),
        "-v", f"{DATA / 'postgres'}:/var/lib/postgresql"])
    wait_for(lambda: docker("exec", NAMESPACE + "-db", "pg_isready", "-U", "palpo",
                            check=False).returncode == 0, "database")
    ensure_container(NAMESPACE + "-hs", VERSIONS["palpo_image"], [
        "--network", NAMESPACE, "-p", "127.0.0.1:18128:8008",
        "-e", "PALPO_CONFIG=/var/palpo/palpo.toml",
        "-v", f"{DATA / 'palpo.toml'}:/var/palpo/palpo.toml:ro",
        "-v", f"{DATA / 'media'}:/var/palpo/media"])
    wait_for(lambda: request("GET", "/_matrix/client/versions")[0] == 200, "homeserver")


def provision_accounts(config):
    accounts = config.setdefault("accounts", {})
    # reimb-intruder is a third, unauthorised account for permission tests.
    for localpart, display in (("reimb-linyi", "林一"), ("reimb-zhoumin", "周敏"), ("reimb-bot", "报销助手"),
                               ("reimb-intruder", "陌生人")):
        account = accounts.setdefault(localpart, {"password": secrets.token_urlsafe(24)})
        private_json(DATA / "credentials.json", config)
        if "access_token" not in account:
            payload = {"username": localpart, "password": account["password"],
                       "initial_device_display_name": "Reimbursement demo"}
            status, result = request("POST", "/_matrix/client/v3/register", payload)
            if status == 401:
                payload["auth"] = {"type": "m.login.registration_token", "session": result["session"],
                                   "token": config["registration_token"]}
                status, result = request("POST", "/_matrix/client/v3/register", payload)
            if result.get("errcode") == "M_USER_IN_USE":
                result = api("POST", "/_matrix/client/v3/login", {
                    "type": "m.login.password", "identifier": {"type": "m.id.user", "user": localpart},
                    "password": account["password"], "initial_device_display_name": "Reimbursement demo"})
            elif status != 200:
                raise RuntimeError(f"Registration failed: HTTP {status}, {result.get('errcode', 'unknown')}")
            account.update({key: result[key] for key in ("access_token", "user_id", "device_id")})
            private_json(DATA / "credentials.json", config)
        api("PUT", "/_matrix/client/v3/profile/" + urllib.parse.quote(account["user_id"], safe="") + "/displayname",
            {"displayname": display}, account["access_token"])
    rooms = config.setdefault("rooms", {})
    for name, left, right in (("applicant_bot", "reimb-linyi", "reimb-bot"),
                             ("finance_bot", "reimb-zhoumin", "reimb-bot"),
                             ("applicant_finance", "reimb-linyi", "reimb-zhoumin"),
                             ("intruder_bot", "reimb-intruder", "reimb-bot")):
        first, second = accounts[left], accounts[right]
        alias = "reimb-" + name.replace("_", "-")
        if name not in rooms:
            status, found = request("GET", "/_matrix/client/v3/directory/room/" +
                                    urllib.parse.quote(f"#{alias}:{SERVER}", safe=""))
            if status == 200:
                rooms[name] = found["room_id"]
            else:
                rooms[name] = api("POST", "/_matrix/client/v3/createRoom", {
                    "room_alias_name": alias, "name": alias, "preset": "private_chat", "is_direct": True,
                    "invite": [second["user_id"]]}, first["access_token"])["room_id"]
            private_json(DATA / "credentials.json", config)
        api("POST", "/_matrix/client/v3/join/" + urllib.parse.quote(rooms[name], safe=""), {}, second["access_token"])
    private_json(DATA / "credentials.json", config)


def main():
    os.umask(0o077)
    if DATA == ROOT or ROOT in DATA.parents:
        raise RuntimeError("REIMB_DATA must be outside the source checkout")
    DATA.mkdir(parents=True, exist_ok=True, mode=0o700)
    credential_file = DATA / "credentials.json"
    config = json.loads(credential_file.read_text()) if credential_file.exists() else {
        "db_password": secrets.token_urlsafe(32), "registration_token": secrets.token_urlsafe(32)}
    private_json(credential_file, config)
    provision_services(config)
    provision_accounts(config)
    print(json.dumps({"status": "ready", "homeserver": BASE, "server_name": SERVER,
                      "accounts": list(config["accounts"]), "rooms": list(config["rooms"])}))


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, subprocess.TimeoutExpired) as error:
        print(json.dumps({"status": "failed", "error": str(error)}))
        raise SystemExit(1)
