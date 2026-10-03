"""History import and event projection through the real CLI."""
import pytest

from conftest import FIXTURES

FINANCE = "@reimb-zhoumin:reimb.local"
APPLICANT = "@reimb-linyi:reimb.local"
NUMBER = "26112000000900000001"


def submitted(number=NUMBER, at="2026-10-01T01:00:00Z"):
    return {"invoice_no": number, "status": "submitted", "actor": APPLICANT, "at": at, "note": "提交",
            "expense": {"order_ref": "AIR-ORD-0001", "seller_name": "中国国际航空股份有限公司", "service_date": "2026-09-10",
                        "amount_cents": 50000, "batch_id": "batch-202609-linyi", "revision": 2}}


def later(status, at, number=NUMBER):
    return {"invoice_no": number, "status": status, "actor": FINANCE, "at": at, "note": status}


def project(runtime, events):
    return runtime.ok("history", {"action": "project", "events": events})


def test_projection_keeps_payment_after_void(runtime):
    result = project(runtime, [submitted(), later("approved", "2026-10-02T01:00:00Z"),
                               later("paid", "2026-10-03T01:00:00Z"), later("voided", "2026-10-04T01:00:00Z")])
    entry = result["validated_snapshot"][0]
    assert entry["current_status"] == "voided" and [event["status"] for event in entry["events"]] == [
        "submitted", "approved", "paid", "voided"]
    assert entry["events_complete"] is True and result["issues"] == []
    # The projected history is what gates consult: the re-issue must not add money.
    stored = runtime.batch / "history" / (result["history_hash"] + ".json")
    assert stored.exists()


def test_voided_unpaid_projection_allows_replacement_review(runtime):
    result = project(runtime, [submitted(), later("voided", "2026-10-02T01:00:00Z")])
    assert result["validated_snapshot"][0]["current_status"] == "voided" and result["issues"] == []


@pytest.mark.parametrize("events", [
    [later("approved", "2026-10-02T01:00:00Z")],
    [submitted(), later("paid", "2026-10-02T01:00:00Z")],
    [submitted(), submitted(at="2026-10-02T01:00:00Z")],
    [submitted(), later("voided", "2026-10-02T01:00:00Z"), later("approved", "2026-10-03T01:00:00Z")],
    [dict(later("approved", "2026-10-02T01:00:00Z"), expense=submitted()["expense"])],
    [dict(submitted(), at="2026-10-01 01:00:00")],
    [dict(submitted(), invoice_no="123")],
])
def test_invalid_event_logs(runtime, events):
    runtime.fails("history", {"action": "project", "events": events}, "INVALID_SCHEMA", 2)


def test_import_without_complete_events_is_unknown(runtime):
    entries = [{"invoice_no": NUMBER, "order_ref": None, "seller_name": "某商户", "service_date": "2026-09-10",
                "amount_cents": 100, "batch_id": "batch-x", "revision": 0, "current_status": "voided",
                "events": [], "events_complete": False, "initial_paid": None, "replaces_invoice_no": None}]
    result = runtime.ok("history", {"action": "validate_import", "entries": entries})
    assert result["issues"] == [{"invoice_no": NUMBER, "reason": "HISTORY_UNKNOWN"}]


@pytest.mark.parametrize("change", [
    {"current_status": "paid"},
    {"amount_cents": -1},
    {"amount_cents": True},
    {"events_complete": True, "events": []},
    {"unexpected": 1},
])
def test_invalid_imports(runtime, change):
    import json
    entry = json.loads((FIXTURES / "demo" / "history.json").read_text())[1]
    entry.update(change)
    runtime.fails("history", {"action": "validate_import", "entries": [entry]}, "INVALID_SCHEMA", 2)


def test_duplicate_history_invoice_is_refused(runtime):
    import json
    entry = json.loads((FIXTURES / "demo" / "history.json").read_text())[0]
    runtime.fails("history", {"action": "validate_import", "entries": [entry, entry]}, "INVALID_SCHEMA", 2)


def test_history_input_must_match_action(runtime):
    runtime.fails("history", {"action": "project", "entries": []}, "INVALID_SCHEMA", 2)
    runtime.fails("history", {"action": "rewrite", "events": []}, "INVALID_SCHEMA", 2)
