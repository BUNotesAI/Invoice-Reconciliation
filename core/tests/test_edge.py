"""Edge set (design §10.2, P1 part) against hand-written answers, through the real CLI."""
import json

import pytest
from openpyxl import load_workbook

from conftest import FIXTURES, Runtime, canonical_hash, confirm, confirmed_fact_ids, decision, expected, item, snapshot

EDGE = expected("edge.json")
EDGE_DIR = FIXTURES / "edge"


@pytest.fixture(scope="module")
def edge(tmp_path_factory):
    runtime = Runtime(tmp_path_factory.mktemp("edge") / "data")
    history = runtime.ok("history", {"action": "validate_import", "entries": json.loads((EDGE_DIR / "history.json").read_text())})
    return runtime, history


def source(runtime, name):
    return runtime.ingest(runtime.upload(EDGE_DIR / name))


@pytest.mark.parametrize("name", sorted(EDGE["ingest"]))
def test_ingest(edge, name):
    runtime, _ = edge
    want = EDGE["ingest"][name]
    if want["ok"]:
        assert source(runtime, name)["detected_type"] == want["detected_type"]
    else:
        runtime.fails("ingest", {"source_path": str(runtime.upload(EDGE_DIR / name)), "original_name": name},
                      want["code"], want["exit"])


@pytest.mark.parametrize("name", sorted(EDGE["extract"]))
def test_extract(edge, name):
    runtime, _ = edge
    want = EDGE["extract"][name]
    payload = {"source_file_id": source(runtime, name)["id"]}
    if not want["ok"]:
        runtime.fails("extract", payload, want["code"], want["exit"])
        return
    result = runtime.ok("extract", payload)
    if "issues" in want:
        assert result["issues"] == want["issues"] and result["invoice"]["service_period"] == want["service_period"]
    if "seller_name" in want:
        assert result["invoice"]["seller_name"]["value"] == want["seller_name"]


def test_history_keeps_unknown_and_paid_then_voided(edge):
    _, history = edge
    assert len(history["validated_snapshot"]) == EDGE["history"]["entries"]
    assert history["issues"] == EDGE["history"]["issues"]


def vision_case():
    return EDGE["vision"]["E03.pdf"]


def test_vision_candidate_with_wrong_digit_is_only_a_candidate(edge):
    runtime, history = edge
    case = vision_case()
    result = runtime.ok("extract", {"source_file_id": source(runtime, "E03.pdf")["id"], "vision_candidate": case["candidate"]})
    assert result["issues"] == case["candidate_issues"]
    gates = runtime.ok("gates", {"invoices": [result["invoice"]], "history_snapshot": history["validated_snapshot"]})
    assert gates["items"][0]["disposition"] == "needs_decision"


@pytest.mark.parametrize("key", ["bad_candidate", "mismatched_candidate"])
def test_vision_candidate_format_failures(edge, key):
    runtime, _ = edge
    case = vision_case()
    broken = dict(case["candidate"])
    broken.update({name: value for name, value in case[key].items() if not name.startswith("_") and name not in ("code", "exit")})
    runtime.fails("extract", {"source_file_id": source(runtime, "E03.pdf")["id"], "vision_candidate": broken},
                  case[key]["code"], case[key]["exit"])


def extracted(runtime, name):
    return runtime.ok("extract", {"source_file_id": source(runtime, name)["id"]})["invoice"]


@pytest.mark.parametrize("name", sorted(EDGE["gates"]))
def test_gates(edge, name):
    runtime, history = edge
    result = runtime.ok("gates", {"invoices": [extracted(runtime, name)], "history_snapshot": history["validated_snapshot"]})
    row = result["items"][0]
    assert {"disposition": row["disposition"], "review_reasons": row["review_reasons"],
            "replacements": [entry["invoice_no"] for entry in row["replacement_candidates"]]} == EDGE["gates"][name]


def package_one(runtime, history, policy_hash, name, spec, decisions=()):
    invoice = extracted(runtime, name)
    ids = [entry["id"] for entry in decisions]
    raw = snapshot(policy_hash, history["history_hash"],
                   [item("item-" + name[:3], invoice, spec["service_date"], spec["category"], spec["short_name"], "边界",
                         ids, spec.get("replaces_invoice_no"))], list(decisions))
    return raw


def policy_hash(runtime, history):
    return runtime.ok("gates", {"invoices": [], "history_snapshot": history["validated_snapshot"]})["policy_hash"]


def test_reissue_of_paid_expense_cannot_add_money(edge):
    runtime, history = edge
    spec = EDGE["package"]["E01.pdf"]
    chosen = decision("decision-e01", "item-E01", spec["decision"]["kind"], spec["decision"]["payload"])
    raw = package_one(runtime, history, policy_hash(runtime, history), "E01.pdf", spec, [chosen])
    runtime.fails("package", {"confirmed_snapshot": raw, "expected_snapshot_hash": canonical_hash(raw)}, spec["code"], spec["exit"])


def test_hotel_without_nights_cannot_be_packaged(edge):
    runtime, history = edge
    spec = EDGE["package"]["E05.pdf"]
    raw = package_one(runtime, history, policy_hash(runtime, history), "E05.pdf", spec)
    runtime.fails("package", {"confirmed_snapshot": raw, "expected_snapshot_hash": canonical_hash(raw)}, spec["code"], spec["exit"])


def test_corrected_vision_and_hostile_names_package_and_verify(edge):
    runtime, history = edge
    case, batch = vision_case(), EDGE["package"]["accepted_batch"]
    candidate = runtime.ok("extract", {"source_file_id": source(runtime, "E03.pdf")["id"],
                                       "vision_candidate": case["candidate"]})["invoice"]
    confirmed = confirm(candidate, case["corrected"], "event-confirm-e03")
    confirmation = decision("decision-e03", "item-E03", "confirm_visual", {"fact_ids": confirmed_fact_ids(confirmed)},
                            event="event-confirm-e03")
    invoices = {"E03.pdf": confirmed, "E07.pdf": extracted(runtime, "E07.pdf"), "E08.pdf": extracted(runtime, "E08.pdf")}
    items = [item("item-" + name[:3], invoices[name], spec["service_date"], spec["category"], spec["short_name"], "边界",
                  ["decision-e03"] if name == "E03.pdf" else [])
             for name, spec in batch["items"].items()]
    raw = snapshot(policy_hash(runtime, history), history["history_hash"], items, [confirmation])
    digest = canonical_hash(raw)
    package = runtime.ok("package", {"confirmed_snapshot": raw, "expected_snapshot_hash": digest})
    assert package["staging_manifest"]["total_cents"] == batch["total_cents"]
    assert "26442000000700000013_潮海居_128_餐饮.pdf" in {entry["relative_name"] for entry in package["staging_manifest"]["files"]}
    result = runtime.ok("verify", {"snapshot_hash": digest, "manifest_object_id": digest,
                                   "history_snapshot": history["validated_snapshot"]})
    assert result["passed"] is batch["verify_passed"]
    book = load_workbook(runtime.batch / "staging" / digest / package["staging_manifest"]["ledger_name"])
    sellers = {book["明细"].cell(row, 8).value: book["明细"].cell(row, 8) for row in range(2, 5)}
    for text in batch["seller_cells"].values():
        assert sellers[text].data_type == "s"
    assert not any(cell.data_type == "f" for row in book["明细"].iter_rows() for cell in row)


def test_unconfirmed_correction_is_rejected_as_format_error(edge):
    runtime, history = edge
    case = vision_case()
    candidate = runtime.ok("extract", {"source_file_id": source(runtime, "E03.pdf")["id"],
                                       "vision_candidate": case["candidate"]})["invoice"]
    # A user correction still passes the invoice format checks: a 19-digit number is refused.
    confirmed = confirm(candidate, {"invoice_no": "2644200000070000001"}, "event-bad")
    runtime.fails("gates", {"invoices": [confirmed], "history_snapshot": history["validated_snapshot"]}, "INVALID_SCHEMA", 2)
