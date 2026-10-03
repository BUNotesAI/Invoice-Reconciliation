#!/usr/bin/env python3
"""Start the isolated demo environment and run the reimbursement bot in the foreground.

Writes the private bot config to $REIMB_DATA/bot.json (mode 0600). The agent defaults to the recorded replay of one
real octos run; set REIMB_AGENT=octos:<octos data dir> for live model calls or REIMB_AGENT=none for rules mode.
"""
import json
import os
import shutil
import subprocess

from dev_env import BASE, DATA, ROOT, main as provision, private_json


def bot_config():
    credentials = json.loads((DATA / "credentials.json").read_text())
    policy = DATA / "policy.yaml"
    if not policy.exists():
        shutil.copyfile(ROOT / "policy" / "example.yaml", policy)
        policy.chmod(0o600)
    config = {
        "homeserver": BASE,
        "bot_user": credentials["accounts"]["reimb-bot"]["user_id"],
        "credentials": str(DATA / "credentials.json"),
        "credentials_account": "reimb-bot",
        "data_root": str(DATA / "work"),
        "policy": str(policy),
        "python": str(ROOT / ".venv" / "bin" / "python"),
        "core_dir": str(ROOT / "core"),
        "history": str(ROOT / "fixtures" / "demo" / "history.json"),
        "period": "2026-10",
        "desk_bind": "127.0.0.1:8787",
        "desk_origin": "http://127.0.0.1:8787",
        "agent": os.environ.get("REIMB_AGENT", "replay:" + str(ROOT / "fixtures" / "agent-replay" / "demo")),
        "applicants": {credentials["accounts"]["reimb-linyi"]["user_id"]: credentials["rooms"]["applicant_bot"]},
        "finance_rooms": {credentials["accounts"]["reimb-zhoumin"]["user_id"]: credentials["rooms"]["finance_bot"]},
    }
    path = DATA / "bot.json"
    private_json(path, config)
    return path


if __name__ == "__main__":
    provision()
    config = bot_config()
    environment = dict(os.environ, REIMB_BOT_CONFIG=str(config), NO_PROXY="127.0.0.1,localhost", no_proxy="127.0.0.1,localhost")
    result = subprocess.run(["cargo", "run", "--locked", "--manifest-path", str(ROOT / "bot/Cargo.toml"), "--bin", "reimb-bot"],
                            env=environment)
    raise SystemExit(result.returncode)
