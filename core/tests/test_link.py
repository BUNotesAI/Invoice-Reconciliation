"""P2 evidence, linking and missing-invoice detection through the real CLI, against hand-written answers."""
import json

import pytest

from conftest import ACTOR, AT, FIXTURES, Runtime, confirm, decision, expected

LINK = expected("link.json")
DEMO = expected("demo.json")
CODES = [f"F{number:02d}" for number in range(1, 13)]


def link_items(invoices, gates):
    by_invoice = {row["invoice_id"]: row for row in gates["items"]}
    return [{"id": "item-" + code, "invoice": invoice,
             "gate": {"disposition": by_invoice[invoice["id"]]["disposition"],
                      "review_reasons": by_invoice[invoice["id"]]["review_reasons"]}}
            for code, invoice in invoices.items()]


def first_pass(runtime):
    sources = {path.name: runtime.ingest(runtime.upload(path)) for path in sorted((FIXTURES / "demo").iterdir())}
    history = runtime.ok("history", {"action": "validate_import",
                                     "entries": json.loads((FIXTURES / "demo" / "history.json").read_text())})
    invoices = {}
    for code in CODES:
        vision = DEMO["vision_candidates"].get(code)
        payload = {"source_file_id": sources[f"{code}.pdf"]["id"]}
        if vision:
            payload["vision_candidate"] = vision
        invoices[code] = runtime.ok("extract", payload)["invoice"]
    gates = runtime.ok("gates", {"invoices": list(invoices.values()), "history_snapshot": history["validated_snapshot"]})
    evidence = []
    for name in ("wechat_bill.xlsx", "didi_trips.pdf"):
        evidence += runtime.ok("evidence", {"source_file_id": sources[name]["id"]})["evidence"]
    items = link_items(invoices, gates)
    linked = runtime.ok("link", {"items": items, "evidence": evidence, "history_snapshot": history["validated_snapshot"],
                                 "decisions": [], "period": LINK["period"]})
    return dict(sources=sources, history=history, invoices=invoices, gates=gates, evidence=evidence, items=items, linked=linked)


@pytest.fixture(scope="module")
def demo(tmp_path_factory):
    runtime = Runtime(tmp_path_factory.mktemp("link") / "data")
    return runtime, first_pass(runtime)


def rows(linked):
    return {row["item_id"].removeprefix("item-"): row for row in linked["links"]}


def test_report_counts_are_six_four_two(demo):
    _, run = demo
    want = LINK["demo_first_pass"]["summary"]
    assert {key: run["linked"]["summary"][key] for key in want} == want
    assert run["linked"]["summary"]["accepted"] == {"count": 0, "cents": 0}


def test_each_demo_item_links_as_written(demo):
    _, run = demo
    got = rows(run["linked"])
    for code, want in LINK["demo_first_pass"]["items"].items():
        row = got[code]
        assert row["disposition"] == want["disposition"] and row["review_reasons"] == want["reasons"], code
        for key in ("category", "service_date", "resolution"):
            if key in want:
                assert row[key] == want[key], (code, key)


def test_personal_transfer_is_only_a_coincidence(demo):
    _, run = demo
    row = rows(run["linked"])["F06"]
    by_id = {entry["id"]: entry for entry in run["evidence"]}
    assert row["payment_candidates"] == [] and [by_id[e]["merchant"] for e in row["coincidences"]] == [LINK["demo_first_pass"]["coincidence"]["F06"]]


def test_occupancy_is_unique(demo):
    _, run = demo
    claims = [claim["evidence_id"] for claim in run["linked"]["occupancy"]["claims"]]
    assert len(claims) == len(set(claims))


def test_missing_invoices(demo):
    runtime, run = demo
    result = runtime.ok("missing", {"evidence": run["evidence"], "occupancy": run["linked"]["occupancy"], "ignored_transactions": [],
                                    "period": LINK["period"], "history_snapshot": run["history"]["validated_snapshot"]})
    want = LINK["demo_first_pass"]
    assert [{key: row[key] for key in ("merchant", "amount_cents", "payment_date", "likelihood")}
            for row in result["candidates"]] == want["missing_candidates"]
    assert len(result["not_included"]) == want["not_included_count"]
    assert {row["deadline"] for row in result["candidates"]} == {want["missing_deadline"]}


def test_ignoring_one_transaction_and_a_merchant(demo, tmp_path):
    runtime, run = demo
    first = next(row for row in run["evidence"] if row["merchant"] == "京东商城")
    result = runtime.ok("missing", {"evidence": run["evidence"], "occupancy": run["linked"]["occupancy"],
                                    "ignored_transactions": [first["transaction_ref"]], "period": LINK["period"],
                                    "history_snapshot": run["history"]["validated_snapshot"]})
    assert [row["merchant"] for row in result["candidates"]] == ["悦途酒店"] and result["ignored_transactions"] == [first["id"]]
    other = Runtime(tmp_path / "data")
    other.policy.write_text(other.policy.read_text(encoding="utf-8").replace("ignore_merchants: []", "ignore_merchants: [悦途酒店]"),
                            encoding="utf-8")
    result = other.ok("missing", {"evidence": run["evidence"], "occupancy": run["linked"]["occupancy"], "ignored_transactions": [],
                                  "period": LINK["period"], "history_snapshot": run["history"]["validated_snapshot"]})
    assert [row["merchant"] for row in result["candidates"]] == ["京东商城"]
    assert result["ignored_merchants"] == ["悦途酒店"]


def test_second_pass_after_confirmations(demo):
    runtime, run = demo
    want = LINK["demo_after_confirmation"]
    shot = LINK["demo_screenshot"]
    confirmation = {"confirmed_by": ACTOR, "confirmation_event": "event-confirm-f06", "confirmed_at": AT}
    screenshot = runtime.ok("evidence", {"source_file_id": run["sources"]["F06_screenshot.png"]["id"],
                                         "confirmed_visual_facts": {"fields": shot, "confirmation": confirmation}})["evidence"]
    invoices = dict(run["invoices"])
    invoices["F09"] = confirm(invoices["F09"], {}, "event-confirm-f09")
    gates = runtime.ok("gates", {"invoices": list(invoices.values()), "history_snapshot": run["history"]["validated_snapshot"]})
    items = link_items(invoices, gates)
    evidence = run["evidence"] + screenshot
    payload = {"items": items, "evidence": evidence, "history_snapshot": run["history"]["validated_snapshot"],
               "decisions": [], "period": LINK["period"]}
    before = rows(runtime.ok("link", payload))
    for code, (state, reasons) in want["before_explanations"].items():
        assert (before[code]["disposition"], before[code]["review_reasons"]) == (state, reasons), code
    payload["decisions"] = [
        decision("d-f08", "item-F08", "explain_over_limit", {"explanation": "会展期间协议价上浮"}),
        decision("d-f09", "item-F09", "explain_over_limit", {"explanation": "客户接待，人数较多"}),
        decision("d-f10", "item-F10", "replace_unpaid_invoice", {"invoice_no": "26112000000300002208"}),
    ]
    after = runtime.ok("link", payload)
    done = want["after_decisions"]
    accepted = [row for row in after["links"] if row["disposition"] == "accepted"]
    assert len(accepted) == done["accepted_count"]
    assert after["summary"]["automatic"]["cents"] + after["summary"]["accepted"]["cents"] == done["accepted_cents"]
    got = rows(after)
    assert (got["F06"]["service_date"], got["F06"]["resolution"]) == (done["F06_service_date"], done["F06_resolution"])
    assert got["F08"]["notes"] == done["F08_notes"] and got["F09"]["notes"] == done["F09_notes"]
    assert got["F10"]["resolution"] == done["F10_resolution"]


def test_unconfirmed_screenshot_cannot_occupy(demo):
    runtime, run = demo
    candidate = runtime.ok("evidence", {"source_file_id": run["sources"]["F06_screenshot.png"]["id"],
                                        "vision_candidate": LINK["demo_screenshot"]})
    assert candidate["issues"] == ["FACT_UNCONFIRMED"]
    linked = runtime.ok("link", {"items": run["items"], "evidence": run["evidence"] + candidate["evidence"],
                                 "history_snapshot": run["history"]["validated_snapshot"], "decisions": [], "period": LINK["period"]})
    assert rows(linked)["F06"]["review_reasons"] == ["EVIDENCE_MISSING"]


def test_agent_category_that_contradicts_the_rule_is_not_applied(demo):
    runtime, run = demo
    items = [dict(item, category="餐饮") if item["id"] == "item-F07" else item for item in run["items"]]
    linked = runtime.ok("link", {"items": items, "evidence": run["evidence"], "history_snapshot": run["history"]["validated_snapshot"],
                                 "decisions": [], "period": LINK["period"]})
    assert "CATEGORY_CONFLICT" in rows(linked)["F07"]["review_reasons"]


@pytest.fixture(scope="module")
def edge(tmp_path_factory):
    runtime = Runtime(tmp_path_factory.mktemp("edge-link") / "data")
    folder = FIXTURES / "edge" / "link"
    sources = {path.name: runtime.ingest(runtime.upload(path)) for path in sorted(folder.iterdir())}
    history = runtime.ok("history", {"action": "validate_import",
                                     "entries": json.loads((FIXTURES / "demo" / "history.json").read_text())})
    invoices = {name.split("_")[0].removesuffix(".pdf"): runtime.ok("extract", {"source_file_id": source["id"]})["invoice"]
                for name, source in sources.items() if name.startswith("R")}
    gates = runtime.ok("gates", {"invoices": list(invoices.values()), "history_snapshot": history["validated_snapshot"]})
    evidence = []
    for name in ("wechat_bill.xlsx", "didi_trips.pdf"):
        evidence += runtime.ok("evidence", {"source_file_id": sources[name]["id"]})["evidence"]
    items = link_items(invoices, gates)
    for item in items:
        declared = LINK["edge"][item["id"].removeprefix("item-")].get("declared_service_date")
        if declared:
            item["declared_service_date"] = declared
    return runtime.ok("link", {"items": items, "evidence": evidence, "history_snapshot": history["validated_snapshot"],
                               "decisions": [], "period": LINK["period"]})


@pytest.mark.parametrize("code", sorted(LINK["edge"]))
def test_edge_linking(edge, code):
    row = rows(edge)[code]
    want = LINK["edge"][code]
    assert (row["disposition"], row["review_reasons"]) == (want["disposition"], want["reasons"])
    if "service_date" in want:
        assert row["service_date"] == want["service_date"]


def test_contested_payment_is_occupied_by_nobody(edge):
    got = rows(edge)
    assert got["R04"]["payment_evidence_id"] is None and got["R05"]["payment_evidence_id"] is None


@pytest.mark.parametrize("payload", [
    {"items": "x", "evidence": [], "history_snapshot": [], "decisions": [], "period": "2026-10"},
    {"items": [], "evidence": [], "history_snapshot": [], "decisions": [], "period": "2026-13x"},
    {"items": [], "evidence": [{"id": "e"}], "history_snapshot": [], "decisions": [], "period": "2026-10"},
])
def test_invalid_link_input(runtime, payload):
    runtime.fails("link", payload, "INVALID_SCHEMA", 2)


def test_service_date_outside_the_window(demo):
    runtime, run = demo
    items = [dict(item, declared_service_date="2026-08-01") if item["id"] == "item-F01" else item for item in run["items"]]
    linked = runtime.ok("link", {"items": items, "evidence": run["evidence"], "history_snapshot": run["history"]["validated_snapshot"],
                                 "decisions": [], "period": LINK["period"]})
    assert rows(linked)["F01"]["review_reasons"] == ["OUT_OF_WINDOW"]


def test_trip_list_must_agree_with_its_stated_total(runtime):
    import sys
    sys.path.insert(0, str(FIXTURES))
    import generate
    lines = ["滴滴出行-行程单", "共2笔行程，合计99.00元", "序号 车型 上车时间 城市 起点 终点 里程(公里) 金额(元)",
             "1 快车 2026-10-10 18:52 深圳 科技园 深圳北站 18.6 46.20"]
    path = runtime.uploads / "trips.pdf"
    path.write_bytes(generate.text_pdf(lines))
    source = runtime.ingest(path)
    assert source["detected_type"] == "didi_trip_pdf"
    runtime.fails("evidence", {"source_file_id": source["id"]}, "FIELD_CONFLICT", 3)
