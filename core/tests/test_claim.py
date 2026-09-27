"""P4c missing-invoice claims through the real CLI, against hand-written answers."""
import copy
import json

import pytest

from conftest import FIXTURES, Runtime, expected

WANT = expected("claim.json")


@pytest.fixture(scope="module")
def claims(tmp_path_factory):
    runtime = Runtime(tmp_path_factory.mktemp("claim") / "data")
    history = runtime.ok("history", {"action": "validate_import",
                                     "entries": json.loads((FIXTURES / "demo" / "history.json").read_text())})
    invoices = {}
    for path in [FIXTURES / "claim" / f"C0{n}.pdf" for n in range(1, 5)] + [FIXTURES / "demo" / "F01.pdf",
                                                                        FIXTURES / "demo" / "F08.pdf"]:
        source = runtime.ingest(runtime.upload(path))
        invoices[path.stem] = runtime.ok("extract", {"source_file_id": source["id"]})["invoice"]
    return runtime, history["validated_snapshot"], invoices


def claim(runtime, history, invoice, spends=None):
    return runtime.ok("claim", {"missing_spends": spends if spends is not None else WANT["spends"],
                                "history_snapshot": history, "invoice": invoice})


@pytest.mark.parametrize("code", sorted(WANT["claims"]))
def test_each_invoice_claims_as_written(claims, code):
    runtime, history, invoices = claims
    want = WANT["claims"][code]
    result = claim(runtime, history, invoices[code])
    assert (result["result"], result["reason"], result["missing_id"]) == (want["result"], want["reason"], want["missing_id"])
    if want["result"] == "match":
        item = result["next_batch_item"]
        assert item["source_file_id"] == invoices[code]["source_file_id"] and item["service_date"] == want["service_date"]
    else:
        assert result["next_batch_item"] is None


def test_closed_spends_take_no_claims(claims):
    runtime, history, invoices = claims
    spends = copy.deepcopy(WANT["spends"])
    for spend in spends:
        spend["status"] = "claimed"
    assert claim(runtime, history, invoices["C01"], spends)["reason"] == "NO_MISSING_MATCH"


def test_two_equal_payments_need_a_person(claims):
    runtime, history, invoices = claims
    spends = copy.deepcopy(WANT["spends"])
    spends.append(dict(spends[1], id="missing-yuetu-2", payment_evidence_id="ev-yuetu-2"))
    result = claim(runtime, history, invoices["C01"], spends)
    assert (result["result"], result["reason"], result["missing_id"]) == ("needs_decision", "MULTIPLE_CANDIDATES", None)


def test_an_invoice_already_reimbursed_is_refused(claims):
    runtime, history, invoices = claims
    entry = copy.deepcopy(history[0])
    entry["invoice_no"] = invoices["C01"]["invoice_no"]["value"]
    result = claim(runtime, history + [entry], invoices["C01"])
    assert (result["result"], result["reason"]) == ("rejected", "DUPLICATE_INVOICE")


def test_search_looks_in_the_history_first(claims):
    runtime, history, _ = claims
    base = {"missing_spends": WANT["spends"], "missing_id": "missing-jd"}
    assert runtime.ok("claim", dict(base, history_snapshot=history)) == {"result": "not_found", "missing_id": "missing-jd",
                                                                         "invoice_nos": []}
    entry = copy.deepcopy(history[0])
    entry.update(invoice_no="26112000000900000999", seller_name="北京京东世纪贸易有限公司", amount_cents=45900)
    found = runtime.ok("claim", dict(base, history_snapshot=history + [entry]))
    assert found == {"result": "found", "missing_id": "missing-jd", "invoice_nos": ["26112000000900000999"]}


def test_claim_input_is_closed(claims):
    runtime, history, invoices = claims
    runtime.fails("claim", {"missing_spends": WANT["spends"], "history_snapshot": history, "invoice": invoices["C01"],
                            "actor": "someone"}, "INVALID_SCHEMA", 2)
    runtime.fails("claim", {"missing_spends": [{"id": "x"}], "history_snapshot": history, "invoice": invoices["C01"]},
                  "INVALID_SCHEMA", 2)


def test_a_stay_matches_a_payment_made_at_checkout(claims):
    # F08: 燕园会展酒店, check-in 2026-10-06, check-out 2026-10-09; paid at checkout on 10-09.
    runtime, history, invoices = claims
    spend = {"id": "missing-yanyuan", "payment_evidence_id": "ev-yanyuan", "merchant": "燕园会展酒店",
             "amount_cents": 156000, "payment_date": "2026-10-09", "status": "waiting", "deadline": "2026-11-30"}
    result = claim(runtime, history, invoices["F08"], [spend])
    assert (result["result"], result["missing_id"]) == ("match", "missing-yanyuan")
    for outside in ("2026-10-04", "2026-10-11"):
        result = claim(runtime, history, invoices["F08"], [dict(spend, payment_date=outside)])
        assert (result["result"], result["reason"]) == ("rejected", "DATE_CONFLICT"), outside
