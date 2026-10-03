#!/usr/bin/env python3
"""One command, script steps 1-15 and 17 end to end, checked against the hand-written answers.

Real components: the local Palpo, the reimb-bot binary with its SQLite state, the core subprocess and the desk over
HTTP. Replaceable boundaries only: the agent (recorded replay of one real octos run) and the clock (a file the bot
reads, so month-end and follow-up reminders can be reached). 林一 and 周敏 are scripted Matrix clients, plus one
account with no rights. Nothing here prints credentials; the run's private files stay under $REIMB_DATA/e2e/.

Usage: uv sync && python3 scripts/e2e.py
"""
import http.cookiejar
import json
import os
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path

from dev_env import BASE, DATA, ROOT, main as provision, private_json, private_write

EXPECTED = ROOT / "fixtures" / "expected"
DEMO = ROOT / "fixtures" / "demo"
CLAIM = ROOT / "fixtures" / "claim"
DESK_PORT = 8797
ORIGIN = f"http://127.0.0.1:{DESK_PORT}"
# 2026-10-31 09:00 and 2026-11-02 09:00 in Asia/Shanghai.
MONTH_END = 1_793_408_400
NEXT_MONDAY = MONTH_END + 2 * 86_400
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))
# Script steps covered, in order: 1, 2, 3-6, 7, 8-9, 10-11, 12, 13, 14, 15, 17.
STEPS = 11


class Failed(Exception):
    pass


def yuan(cents):
    whole, part = divmod(cents, 100)
    return f"¥{whole:,}.{part:02d}"


def quote(text):
    return urllib.parse.quote(text, safe="")


class Person:
    """One scripted Matrix account in its direct room with the bot."""

    def __init__(self, credentials, account, room):
        self.user = credentials["accounts"][account]["user_id"]
        self.token = credentials["accounts"][account]["access_token"]
        self.room = credentials["rooms"][room]

    def call(self, method, path, body=None, raw=None, content_type="application/json"):
        data = raw if raw is not None else (None if body is None else json.dumps(body).encode())
        request = urllib.request.Request(BASE + path, data=data, method=method,
                                         headers={"Authorization": "Bearer " + self.token, "Content-Type": content_type})
        with HTTP.open(request, timeout=30) as response:
            return json.load(response)

    def send(self, content):
        self.call("PUT", f"/_matrix/client/v3/rooms/{quote(self.room)}/send/m.room.message/{uuid.uuid4().hex}", content)

    def text(self, body):
        self.send({"msgtype": "m.text", "body": body})

    def file(self, path, name=None):
        name = name or path.name
        data = path.read_bytes()
        uri = self.call("POST", f"/_matrix/media/v3/upload?filename={quote(name)}", raw=data,
                        content_type="application/octet-stream")["content_uri"]
        self.send({"msgtype": "m.file", "body": name, "filename": name, "url": uri,
                   "info": {"size": len(data), "mimetype": "application/octet-stream"}})

    def bot_messages(self, bot, since):
        page = self.call("GET", f"/_matrix/client/v3/rooms/{quote(self.room)}/messages?dir=b&limit=500")
        events = [e for e in page.get("chunk", []) if e.get("sender") == bot and e.get("type") == "m.room.message"
                  and e.get("origin_server_ts", 0) >= since]
        return list(reversed(events))

    def wait(self, bot, since, what, test, timeout=180):
        started = time.time()
        while time.time() - started < timeout:
            for event in self.bot_messages(bot, since):
                if test(event["content"]):
                    return event["content"]
            time.sleep(0.7)
        raise Failed(f"timed out waiting for {what}")

    def count(self, bot, since, needle):
        return sum(needle in (e["content"].get("body") or "") for e in self.bot_messages(bot, since))


def says(needle):
    return lambda content: needle in (content.get("body") or "")


class Desk:
    """A browser on the desk: its own cookie jar, Origin and CSRF header on writes, the shown revision."""

    def __init__(self, batch):
        self.batch = batch
        self.jar = http.cookiejar.CookieJar()
        self.http = urllib.request.build_opener(urllib.request.ProxyHandler({}),
                                                urllib.request.HTTPCookieProcessor(self.jar))
        self.csrf, self.revision = None, None
        self.http.open(f"{ORIGIN}/desk/b/{batch}", timeout=30).read()

    def state(self):
        state = json.load(self.http.open(f"{ORIGIN}/desk/api/b/{self.batch}/state", timeout=30))
        if state.get("paired"):
            self.csrf, self.revision = state["csrf"], state["batch"]["revision"]
        return state

    def post(self, path, body, revision=None):
        body = dict(body, expected_revision=self.revision if revision is None else revision)
        request = urllib.request.Request(f"{ORIGIN}/desk/api/b/{self.batch}/{path}", data=json.dumps(body).encode(),
                                         method="POST", headers={"Content-Type": "application/json", "Origin": ORIGIN,
                                                                 "X-CSRF-Token": self.csrf or ""})
        try:
            with self.http.open(request, timeout=300) as response:
                return response.status
        except urllib.error.HTTPError as error:
            return error.code

    def download(self, name):
        with self.http.open(f"{ORIGIN}/desk/api/b/{self.batch}/files/{quote(name)}", timeout=60) as response:
            return response.read()


class Bot:
    def __init__(self, binary, config, log):
        self.binary, self.config, self.log, self.process = binary, config, log, None

    def start(self):
        environment = dict(os.environ, REIMB_BOT_CONFIG=str(self.config), NO_PROXY="127.0.0.1,localhost",
                           no_proxy="127.0.0.1,localhost")
        self.process = subprocess.Popen([str(self.binary)], env=environment, stdout=self.log, stderr=self.log)
        started = time.time()
        while time.time() - started < 120:
            if self.process.poll() is not None:
                raise Failed(f"reimb-bot exited with {self.process.returncode}; see {self.log.name}")
            try:
                HTTP.open(f"{ORIGIN}/health", timeout=2).read()
                time.sleep(3)  # margin for the first sync to start before the script writes
                return
            except OSError:
                time.sleep(0.5)
        raise Failed("reimb-bot did not come up")

    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.SIGINT)
            try:
                self.process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()


def build():
    subprocess.run(["cargo", "build", "--locked", "--quiet", "--manifest-path", str(ROOT / "bot/Cargo.toml"),
                    "--bin", "reimb-bot"], check=True)
    meta = json.loads(subprocess.run(["cargo", "metadata", "--format-version", "1", "--no-deps", "--manifest-path",
                                      str(ROOT / "bot/Cargo.toml")], check=True, capture_output=True, text=True).stdout)
    return Path(meta["target_directory"]) / "debug" / "reimb-bot"


def other_bot_running():
    found = subprocess.run(["pgrep", "-f", "/debug/reimb-bot$|/release/reimb-bot$"], capture_output=True, text=True)
    return bool(found.stdout.strip())


def run(steps, work):
    credentials = json.loads((DATA / "credentials.json").read_text())
    link = json.loads((EXPECTED / "link.json").read_text())
    demo = json.loads((EXPECTED / "demo.json").read_text())
    claim = json.loads((EXPECTED / "claim.json").read_text())
    (work / "data").mkdir(parents=True, mode=0o700)
    policy = work / "policy.yaml"
    policy.write_text((ROOT / "policy/example.yaml").read_text(encoding="utf-8"), encoding="utf-8")
    clock = work / "clock"
    private_write(clock, f"{MONTH_END}\n")
    config = work / "bot.json"
    linyi = Person(credentials, "reimb-linyi", "applicant_bot")
    zhoumin = Person(credentials, "reimb-zhoumin", "finance_bot")
    intruder = Person(credentials, "reimb-intruder", "intruder_bot")
    bot_user = credentials["accounts"]["reimb-bot"]["user_id"]
    private_json(config, {
        "homeserver": BASE, "bot_user": bot_user, "credentials": str(DATA / "credentials.json"),
        "credentials_account": "reimb-bot", "data_root": str(work / "data"), "policy": str(policy),
        "python": str(ROOT / ".venv/bin/python"), "core_dir": str(ROOT / "core"),
        "history": str(DEMO / "history.json"), "desk_bind": f"127.0.0.1:{DESK_PORT}",
        "desk_origin": ORIGIN, "agent": "replay:" + str(ROOT / "fixtures/agent-replay/demo"),
        "applicants": {linyi.user: linyi.room}, "finance_rooms": {zhoumin.user: zhoumin.room}, "clock_file": str(clock)})
    log = open(work / "bot.log", "ab")
    bot = Bot(build(), config, log)
    since = int(time.time() * 1000)
    step = steps.step
    bot.start()
    try:
        with step("1 month-end reminder"):
            linyi.wait(bot_user, since, "month-end reminder", says("2026-10 最后一天"))

        with step("2 files in chat; a stranger is refused"):
            intruder.file(DEMO / "F01.pdf")
            intruder.wait(bot_user, since, "stranger refusal", says("你不在报销名单里"))
            files = sorted(p for p in DEMO.iterdir() if p.name != "history.json")
            for path in files:
                linyi.file(path)
            linyi.wait(bot_user, since, "all files", says(f"本批次共 {len(files)} 个文件"))

        with step("3-6 reading, gates, linking, report 6/4/2 and missing candidates"):
            linyi.text("开始对账")
            report = linyi.wait(bot_user, since, "report", says("待你判断"))["body"]
            summary = link["demo_first_pass"]["summary"]
            for text in (f"自动匹配 {summary['automatic']['count']} 张，合计 {yuan(summary['automatic']['cents'])}",
                         f"待你判断 {summary['needs_decision']['count']} 张，合计 {yuan(summary['needs_decision']['cents'])}",
                         f"拒收 {summary['rejected']['count']} 张"):
                steps.expect(text in report, f"report says {text}")
            wanted = "、".join(f"{m['merchant']} {yuan(m['amount_cents'])}（{m['payment_date']}）"
                              for m in link["demo_first_pass"]["missing_candidates"])
            steps.expect(f"可能漏票 2 笔：{wanted}" in report, "report lists the two missing-invoice candidates")
            steps.note("agent", "rules mode" if "规则模式" in report else "recorded agent")
            card = linyi.wait(bot_user, since, "desk card", lambda c: c.get("msgtype") == "rs.robius.robrix.mini_app")
            url = card["mini_app"]["url"]
            steps.expect(url.startswith(ORIGIN + "/desk/b/") and "?" not in url, "card URL carries only the batch id")
            batch = url.rsplit("/", 1)[1]

        with step("7 pairing in chat; a stranger cannot use the code"):
            desk = Desk(batch)
            intruder.text(desk.state()["code"])
            intruder.wait(bot_user, since, "stranger pairing refusal", says("不属于你"))
            linyi.text(desk.state()["code"])
            linyi.wait(bot_user, since, "pairing", says("配对成功"))
            state = desk.state()
            steps.expect(state["role"] == "applicant", "desk paired as the applicant")

        with step("8-9 visual check, screenshot evidence and decisions at the current revision"):
            items = {item["file"]: item["item_id"] for item in state["report"]["items"]}
            steps.expect(desk.post("confirm-visual", {"item_id": items["F09.pdf"]}) == 200, "F09 reading confirmed")
            shot = desk.state()["screenshots"][0]["source_id"]
            steps.expect(desk.post("confirm-screenshot", {"source_id": shot}) == 200, "F06 screenshot confirmed")
            explanation = next(d for d in demo["decisions"] if d["item"] == "F08")["payload"]["explanation"]
            replaced = next(d for d in demo["decisions"] if d["item"] == "F10")["payload"]["invoice_no"]
            for file, kind, payload in (("F08.pdf", "explain_over_limit", {"explanation": explanation}),
                                        ("F09.pdf", "explain_over_limit", {"explanation": "客户接待"}),
                                        ("F10.pdf", "replace_unpaid_invoice", {"invoice_no": replaced})):
                desk.state()
                steps.expect(desk.post("decide", {"item_id": items[file], "kind": kind, "payload": payload}) == 200,
                             f"{file} decided")
            linyi.wait(bot_user, since, "all decided", says("所有待判断项都处理完了"))

        with step("10-11 confirmation, package, final review, publication"):
            desk.state()
            steps.expect(desk.post("confirm", {}, revision=desk.revision - 1) == 409, "a stale revision is refused")
            # New short names (not in the policy table) are shown on the confirmation card; the applicant corrects
            # the ones that differ from the names the company uses, as a person would, and confirms the rest.
            review = desk.state()["short_names"]
            edits = {}
            for row in review:
                want = demo["package_items"][row["file"].removesuffix(".pdf")]["short_name"]
                if row["from_policy"]:
                    steps.expect(row["short_name"] == want, f"{row['file']} policy short name")
                elif row["short_name"] != want:
                    edits[row["item_id"]] = want
            steps.note("short_names_edited", sorted({demo["package_items"][r["file"].removesuffix(".pdf")]["short_name"]
                                                     for r in review if r["item_id"] in edits}))
            steps.expect(desk.post("confirm", {"short_names": edits}) == 200, "confirmed at the current revision")
            result = linyi.wait(bot_user, since, "result", says("终审全部通过"))["body"]
            total = sum(row["amount_cents"] for row in demo["ledger"]["rows"])
            for text in (f"（{len(demo['verify']['checks'])}/{len(demo['verify']['checks'])}）",
                         f"{len(demo['ledger']['rows'])} 张发票", yuan(total), demo["ledger"]["name"]):
                steps.expect(text in result, f"result post says {text}")
            state = desk.state()
            ledger = demo["ledger"]["name"]
            steps.expect(ledger in state["published"], "the ledger is published")
            steps.expect(desk.download(ledger).startswith(b"PK"), "the ledger downloads as a workbook")
            pdfs = sorted(name for name in state["published"] if name.endswith(".pdf"))
            wanted = sorted(row["file"] for row in demo["ledger"]["rows"])
            steps.expect(pdfs == wanted, "published invoice files match the expected names",
                         {"missing": sorted(set(wanted) - set(pdfs)), "extra": sorted(set(pdfs) - set(wanted))})

        with step("12 share with finance; finance pairs with her own account"):
            steps.expect(desk.post("submit", {}) == 200, "submitted")
            linyi.wait(bot_user, since, "submitted", says("已提交给财务"))
            zhoumin.wait(bot_user, since, "finance notice", says("请在对账台审核"))
            finance = Desk(batch)
            zhoumin.text(finance.state()["code"])
            zhoumin.wait(bot_user, since, "finance pairing", says("配对成功"))
            steps.expect(finance.state()["role"] == "finance", "desk paired as finance")
            steps.expect(finance.post("confirm", {}) == 403, "finance has none of the applicant's rights")

        with step("13 finance returns one item: new revision"):
            before = finance.revision
            steps.expect(finance.post("return", {"items": {items["F08.pdf"]: "请补充住宿的事由和人数"}}) == 200,
                         "F08 returned")
            linyi.wait(bot_user, since, "return notice", says("财务退回了 1 项"))
            desk.state()
            steps.expect(desk.revision > before, "the return made a new revision")

        with step("14 supplement, rebuild with the rest unchanged, resubmit, approve"):
            steps.expect(desk.post("supplement", {"item_id": items["F08.pdf"], "people": 3, "purpose": "客户会展接待"})
                         == 200, "supplement confirmed")
            desk.state()
            steps.expect(desk.post("confirm", {}) == 200, "rebuilt revision confirmed")
            started = time.time()
            while linyi.count(bot_user, since, "终审全部通过") < 2 and time.time() - started < 180:
                time.sleep(1)
            steps.expect(linyi.count(bot_user, since, "终审全部通过") == 2, "the rebuilt revision passes the final review")
            desk.state()
            steps.expect(desk.post("submit", {}) == 200, "resubmitted")
            finance.state()
            steps.expect(finance.post("approve", {}) == 200, "approved by finance")
            linyi.wait(bot_user, since, "approval", says("财务已审批通过"))

        with step("15 missing-invoice follow-up: billing details, reminder, claims in chat"):
            linyi.wait(bot_user, since, "follow-up notice", says("可能漏票要跟进"))
            follow_ups = desk.state()["follow_ups"]
            steps.expect([(f["merchant"], f["amount_cents"]) for f in follow_ups]
                         == [(s["merchant"], s["amount_cents"]) for s in claim["spends"]], "one follow-up per candidate")
            for follow in follow_ups:
                steps.expect(desk.post("follow-up", {"spend_id": follow["id"], "action": "business"}) == 200,
                             f"{follow['merchant']} is business spending")
                steps.expect(desk.post("follow-up", {"spend_id": follow["id"], "action": "method",
                                                     "payload": {"method": "platform"}}) == 200,
                             f"{follow['merchant']}: invoice from the platform")
            linyi.wait(bot_user, since, "billing details", says("税号：91440300XXXXXXXX0A"))
            private_write(clock, f"{NEXT_MONDAY}\n")
            linyi.wait(bot_user, since, "weekly reminder", says("漏票提醒"), timeout=120)
            linyi.file(CLAIM / "C03.pdf")
            linyi.wait(bot_user, since, "personal title refused", says("抬头不是公司"))
            linyi.file(CLAIM / "C04.pdf")
            # The clock is in November now: claimed invoices go into the next period's batch.
            linyi.wait(bot_user, since, "京东 claimed into the next batch", says("这是漏票 京东商城 ¥459.00（2026-10-22） 的发票，已认领，归入下一批次（2026-11）"))
            linyi.file(CLAIM / "C01.pdf")
            linyi.wait(bot_user, since, "悦途 claimed", says("这是漏票 悦途酒店"))
            # Two days later the one-hour pairing has expired: the page asks for a new code, as it would for a person.
            steps.expect(desk.state()["paired"] is False, "the old pairing expired with time")
            desk = Desk(batch)
            linyi.text(desk.state()["code"])
            started = time.time()
            while not desk.state().get("paired") and time.time() - started < 60:
                time.sleep(1)
            steps.expect([f["status"] for f in desk.state()["follow_ups"]] == ["claimed", "claimed"],
                         "both follow-ups claimed")

        with step("17 failures and repeats"):
            linyi.file(CLAIM / "C04.pdf", "C04-again.pdf")
            linyi.wait(bot_user, since, "same file again", says("已处理过，未重复入账"))
            for attempt in range(5):
                intruder.text(f"{attempt:06d}")
            intruder.text("123456")
            intruder.wait(bot_user, since, "wrong codes limit", says("错误次数过多"))
            steps.expect(desk.post("follow-up", {"spend_id": follow_ups[0]["id"], "action": "abandon"},
                                   revision=desk.revision - 1) == 409, "a stale page cannot act")
            bot.stop()
            linyi.text("重试")  # arrives while the bot is down
            bot.start()
            linyi.wait(bot_user, since, "reply to a message sent during the restart", says("当前没有需要重试的步骤"))
            time.sleep(5)
            for needle, times in (("终审全部通过", 2), ("财务已审批通过", 1), ("可能漏票要跟进", 1), ("2026-10 最后一天", 1),
                                  ("这是漏票 京东商城", 1), ("这是漏票 悦途酒店", 1)):
                steps.expect(linyi.count(bot_user, since, needle) == times, f"「{needle}」 posted exactly {times}x")
    finally:
        bot.stop()
        log.close()


class Steps:
    def __init__(self):
        self.results, self.notes, self.current = [], {}, None

    def step(self, name):
        steps = self

        class Context:
            def __enter__(self):
                steps.current = {"step": name, "checks": []}
                print(f"… {name}", flush=True)

            def __exit__(self, kind, error, _):
                steps.current["ok"] = kind is None
                if kind is not None:
                    steps.current["error"] = str(error)
                steps.results.append(steps.current)
                print(("  ok" if kind is None else f"  FAILED: {error}"), flush=True)
                return False

        return Context()

    def expect(self, condition, what, detail=None):
        self.current["checks"].append({"check": what, "ok": bool(condition), **({"detail": detail} if detail else {})})
        if not condition:
            raise Failed(what + ("" if detail is None else f" {json.dumps(detail, ensure_ascii=False)}"))

    def note(self, key, value):
        self.notes[key] = value


def main():
    if other_bot_running():
        print("another reimb-bot is running on this machine; stop it first (it would answer the same rooms)")
        return 2
    provision()
    steps = Steps()
    work = DATA / "e2e" / time.strftime("%Y%m%dT%H%M%S")
    try:
        run(steps, work)
    except Exception as error:  # a failed step is already recorded; anything else is reported, not raised
        if not isinstance(error, Failed) and (not steps.results or steps.results[-1]["ok"]):
            print(f"unexpected error: {type(error).__name__}: {error}")
    ok = bool(steps.results) and all(r["ok"] for r in steps.results) and len(steps.results) == STEPS
    summary = {"status": "passed" if ok else "failed", "steps": len(steps.results), "notes": steps.notes}
    if work.exists():
        private_json(work / "result.json", {"summary": summary, "steps": steps.results})
        summary["result"] = str(work / "result.json")
    print(json.dumps(summary, ensure_ascii=False))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
