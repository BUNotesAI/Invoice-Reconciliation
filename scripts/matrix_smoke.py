#!/usr/bin/env python3
"""Exercise the actual Matrix bot and verify persisted notice and card events."""
import argparse
import json
import time
import urllib.parse
import urllib.request
import uuid

from dev_env import DATA, HTTP, api, private_json


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", required=True)
    args = parser.parse_args()
    credentials = json.loads((DATA / "credentials.json").read_text())
    account = credentials["accounts"]["reimb-linyi"]
    room = credentials["rooms"]["applicant_bot"]
    token = account["access_token"]
    before = api("GET", "/_matrix/client/v3/sync?timeout=0", token=token)["next_batch"]
    sent = api("PUT", "/_matrix/client/v3/rooms/" + urllib.parse.quote(room, safe="") +
               "/send/m.room.message/reimb-smoke-" + uuid.uuid4().hex,
               {"msgtype": "m.text", "body": "P0 echo"}, token)
    deadline = time.monotonic() + 45
    events = []
    notice = card = None
    while time.monotonic() < deadline and (notice is None or card is None):
        sync = api("GET", "/_matrix/client/v3/sync?timeout=1000&since=" + urllib.parse.quote(before, safe=""), token=token)
        before = sync["next_batch"]
        for event in sync.get("rooms", {}).get("join", {}).get(room, {}).get("timeline", {}).get("events", []):
            if event.get("sender") != "@reimb-bot:reimb.local":
                continue
            content = event.get("content", {})
            if content.get("msgtype") == "m.notice":
                notice = event
            if content.get("msgtype") == "rs.robius.robrix.mini_app":
                card = event
            events.append(event)
    if notice is None or card is None:
        raise RuntimeError("Bot did not produce both notice and mini app card")
    assert "<b>" in notice["content"]["formatted_body"]
    url = card["content"]["mini_app"]["url"]
    assert url == "http://127.0.0.1:8787/desk/b/p0-demo"
    with HTTP.open(url, timeout=5) as response:
        html = response.read().decode()
        assert response.status == 200 and "聊天卡片已连接到对账台" in html
    from pathlib import Path
    private_json(Path(args.out), {"status": "passed", "trigger_event": sent["event_id"],
                                 "events": events, "http_page_status": 200,
                                 "ui_rendering": "not_verified"})
    print(json.dumps({"status": "passed", "checks": ["matrix_echo", "formatted_body_payload", "mini_app_payload", "http_page"],
                      "ui_rendering": "not_verified"}))


if __name__ == "__main__":
    main()
