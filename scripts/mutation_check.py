#!/usr/bin/env python3
"""Disable each key guard in turn and require the test suite to fail (P1 review finding A5).

Every mutant runs the full `pytest core/` on its own copy of the repository. The only deselected test needs git
metadata, which the copies do not have. Usage:

    uv run python scripts/mutation_check.py [--jobs N] [--only NAME ...] [--out FILE]
"""
import argparse
import concurrent.futures
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CORE = "core/reimb_core/"

# name: (file, exact text, replacement). Each text must occur exactly once.
GUARDS = {
    # Final review: table against snapshot and disk.
    "verify_history_dup": ("verify.py", "check(not set(numbers) & history_numbers,", "check(True or not set(numbers) & history_numbers,"),
    "verify_batch_dup": ("verify.py", "check(len(numbers) == len(set(numbers))", "check(True or len(numbers) == len(set(numbers))"),
    "verify_text_type": ("verify.py", 'check(all(detail.cell(index, n).data_type == "s"', 'check(True or all(detail.cell(index, n).data_type == "s"'),
    "verify_invoice_format": ("verify.py", 'check(detail.cell(index, 7).number_format == "@"', 'check(True or detail.cell(index, 7).number_format == "@"'),
    "verify_merged": ("verify.py", "check({str(merged)", "check(True or {str(merged)"),
    "verify_applicant": ("verify.py", 'check(summary["D2"].value', 'check(True or summary["D2"].value'),
    "verify_short_name": ("verify.py", "and match[2] == item.short_name", "and True"),
    "verify_manifest_fields": ("verify.py", "check((row.item_id,", "check(True or (row.item_id,"),
    "verify_category_row": ("verify.py", "check(summary.cell(index, 1).value == neutral(category) and",
                            "check(True or summary.cell(index, 1).value == neutral(category) and"),
    "verify_date_order": ("verify.py", "check(dates == sorted(dates)", "check(True or dates == sorted(dates)"),
    "verify_amount_vs_snapshot": ("verify.py", 'check(amount == invoice.amount_cents.value, "row_completeness"',
                                  'check(True, "row_completeness"'),
    "verify_attachment_hash": ("verify.py", 'check(attachment.is_file() and hashlib.sha256(attachment.read_bytes()).hexdigest() == invoice.source_file_id,',
                               "check(attachment.is_file(),"),
    "verify_workbook_bytes": ("verify.py", "check(path.read_bytes() == workbook(rows, totals, snapshot.applicant),",
                              "check(True or path.read_bytes() == workbook(rows, totals, snapshot.applicant),"),
    # Final review: snapshot against the originals and the policy.
    "verify_source_attachment_once": ("verify.py", 'check(invoice.source_file_id not in attachments, "source_facts"', 'check(True, "source_facts"'),
    "verify_source_extracted": ("verify.py", 'check(all(fact.level == "extracted" for _, fact in facts), "source_facts"', 'check(True, "source_facts"'),
    "verify_source_text_layer": ("verify.py", 'check(not problems, "source_facts"', 'check(True, "source_facts"'),
    "verify_source_confirmed": ("verify.py", 'check(all(fact.level == "confirmed" and any(', 'check(True or all(fact.level == "confirmed" and any('),
    "verify_source_buyer": ("verify.py", "check(invoice.buyer_name.value == policy.company.name and invoice.buyer_tax_id.value == policy.company.tax_id,\n              \"source_facts\"",
                            "check(True,\n              \"source_facts\""),
    "verify_source_category": ("verify.py", 'check(False, "source_facts", f"{label}: category differs', 'check(True, "source_facts", f"{label}: category differs'),
    "verify_source_stay_limit": ("verify.py", 'check(not stay_over_limit(invoice, policy) or "explain_over_limit" in kinds, "source_facts"',
                                 'check(True, "source_facts"'),
    "verify_source_replacement": ("verify.py", "check(original is not None and original in related", "check(True or original is not None and original in related"),
    "verify_source_replaced_once": ("verify.py", 'check(item.replaces_invoice_no not in replaced, "source_facts"', 'check(True, "source_facts"'),
    "verify_source_unannounced_reissue": ("verify.py", 'check(not related, "source_facts"', 'check(True, "source_facts"'),
    # Packaging.
    "pkg_replace_date": ("package.py", 'and item.service_date == previous["service_date"]', "and True"),
    "pkg_replace_amount": ("package.py", 'and invoice.amount_cents.value == previous["amount_cents"]', "and True"),
    "pkg_replace_once": ("package.py", 'require(previous["invoice_no"] not in replaced', 'require(True or previous["invoice_no"] not in replaced'),
    "pkg_stay_checkin": ("package.py", "require(item.service_date == invoice.service_period.check_in.value,",
                         "require(True or item.service_date == invoice.service_period.check_in.value,"),
    "pkg_stay_limit": ("package.py", 'require(not stay_over_limit(invoice, policy) or "explain_over_limit" in kinds,', "require(True,"),
    "pkg_category_rule": ("package.py", "shown = check_category(invoice, policy.categories[item.category])",
                          'shown = policy.categories[item.category].kind'),
    "pkg_confirm_actor": ("package.py", "and decision.actor == fact.confirmed_by", "and True"),
    "pkg_attachment_once": ("package.py", "require(invoice.source_file_id not in attachments", "require(True or invoice.source_file_id not in attachments"),
    "pkg_snapshot_dup": ("package.py", "invoice.invoice_no.value not in seen and ", ""),
    "pkg_policy_hash": ("package.py", "require(snapshot.policy_hash == policy.sha()", "require(True or snapshot.policy_hash == policy.sha()"),
    "pkg_reparse": ("package.py", 'require(same_business_values(invoice, parsed)', 'require(True or same_business_values(invoice, parsed)'),
    "pkg_normalized_hash": ("package.py", "require(digest(normalized) == expected,", "require(digest(raw) == expected,"),
    # Models, gates, values and storage.
    "model_locator_bind": ("models.py", 'raise ValueError("Confirmation locator differs from fact provenance")', "pass"),
    "model_initial_paid": ("models.py", "if self.initial_paid is True or any(", "if any("),
    "model_event_order": ("models.py", 'raise ValueError("History events out of order")', "pass"),
    "model_paid_survives_void": ("models.py", 'if self.initial_paid is True or any(event.status == "paid" for event in self.events):',
                                 "if self.initial_paid is True:"),
    "gates_batch_dup": ("gates.py", "counts[invoice.invoice_no.value] > 1 or ", ""),
    "gates_history_dup": ("gates.py", " or invoice.invoice_no.value in old_numbers", ""),
    "values_canonical_upper": ("values.py", 'require(normalized in accepted, message="Non-canonical uppercase amount")', "pass"),
    "values_label_name": ("values.py", "and not value[-1].isdigit()", ""),
    "storage_symlink": ("storage.py", 'require(not cursor.is_symlink(), "INVALID_PATH", "Symbolic links are not allowed")', "pass"),
    "storage_single_link": ("storage.py", "and status.st_nlink == 1", ""),
    "storage_form_images": ("storage.py", "            _resource_images(xobject.get(\"/Resources\"), reader, seen, depth + 1)", "            pass"),
    "storage_inline_images": ("storage.py", "            _pixels(settings.get(\"/W\"", "            (settings.get(\"/W\""),
    # P2: linking, refunds, limits and missing-invoice detection.
    "link_unusable_evidence": ("link.py", "if evidence.kind == \"trip\" or not evidence.usable() or", "if evidence.kind == \"trip\" or"),
    "link_coincidence": ("link.py", "(genuine if merchant_ok and is_purchase else coincidences)", "(genuine if True else coincidences)"),
    "link_category_conflict": ("link.py", '                plan["reasons"].append("CATEGORY_CONFLICT")', "                pass"),
    "link_candidate_reserved": ("link.py", '            plan["reasons"].append("FACT_UNCONFIRMED")', "            pass"),
    "link_date_conflict": ("link.py", "elif len(days) > 1:", "elif False:"),
    "link_window": ("link.py", '            plan["reasons"].append("OUT_OF_WINDOW")', "            pass"),
    "link_contested": ("link.py", "contested = {evidence_id for evidence_id, owners in claims.items() if len(owners) > 1}", "contested = set()"),
    "link_full_refund": ("link.py", 'if payment.payment_status == "full_refund":', "if False:"),
    "link_partial_refund": ("link.py", 'elif payment.payment_status == "partial_refund":', "elif False:"),
    "link_daily_limit": ("link.py", 'if sum_cents(plan["item"].invoice.amount_cents.value for plan in group) > limit:', "if False:"),
    "missing_taken": ("missing.py", ' or payment.id in taken):', "):"),
    "missing_ignored_transaction": ("missing.py", "if payment.transaction_ref in ignored:", "if False:"),
    "missing_ignored_merchant": ("missing.py", "if any(name in payment.merchant for name in policy.ignore_merchants):", "if False:"),
    "evidence_trip_total": ("evidence.py", "require(int(total[1]) == len(result)", "require(True or int(total[1]) == len(result)"),
}

# Mutants no test can tell apart from the original, each with the reason.
EQUIVALENT = {}


def run(name, python, keep):
    file, old, new = GUARDS[name]
    work = Path(tempfile.mkdtemp(prefix=f"mut-{name}-"))
    try:
        shutil.copytree(ROOT, work / "repo", ignore=shutil.ignore_patterns(".git", ".venv", "__pycache__", "target"))
        path = work / "repo" / CORE / file
        text = path.read_text(encoding="utf-8")
        if text.count(old) != 1:
            return {"mutant": name, "error": f"guard text found {text.count(old)} times"}
        path.write_text(text.replace(old, new), encoding="utf-8")
        (work / "tmp").mkdir()
        process = subprocess.run(
            [python, "-m", "pytest", "core/", "-q", "-x", "-p", "no:cacheprovider",
             "--deselect", "core/tests/test_check_public.py::test_repository_scan_passes"],
            cwd=work / "repo", capture_output=True, text=True, timeout=1200,
            env={"PATH": os.environ.get("PATH", ""), "TMPDIR": str(work / "tmp"), "PYTHONDONTWRITEBYTECODE": "1"})
        failed = [line.split(" - ")[0] for line in process.stdout.splitlines() if line.startswith("FAILED")]
        return {"mutant": name, "killed": process.returncode != 0, "by": failed[:1]}
    finally:
        if not keep:
            shutil.rmtree(work, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--jobs", type=int, default=6)
    parser.add_argument("--only", nargs="*")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    names = args.only or sorted(GUARDS)
    python = str(ROOT / ".venv" / "bin" / "python")
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        results = list(pool.map(lambda name: run(name, python, args.keep), names))
    survivors = [r["mutant"] for r in results if not r.get("killed") and r["mutant"] not in EQUIVALENT]
    report = {"total": len(results), "killed": sum(1 for r in results if r.get("killed")), "survivors": survivors,
              "equivalent": {name: EQUIVALENT[name] for name in names if name in EQUIVALENT}, "results": results}
    text = json.dumps(report, ensure_ascii=False, indent=2)
    if args.out:
        args.out.write_text(text + "\n", encoding="utf-8")
    print(text)
    return 1 if survivors or any("error" in r for r in results) else 0


if __name__ == "__main__":
    sys.exit(main())
