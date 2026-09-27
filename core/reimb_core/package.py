"""Build reproducible staging packages from a validated immutable snapshot."""
import hashlib
from collections import defaultdict

from .errors import CoreError, require
from .extract import parse_text_invoice, same_business_values
from .history import replacement_candidates, validated_history
from .ledger import workbook
from .models import HistoryEntry, Snapshot
from .rules import check_category, stay_over_limit
from .storage import atomic_write, inside
from .values import amount_text, canonical, digest, safe_name, strict_json, sum_cents

STAGED_RECORDS = ("manifest.json", "snapshot.json")


def replacement_note(invoice_no):
    return f"重开票，原票 {invoice_no} 已作废"


def expense_detail(item):
    return item.expense_detail + ("；" + replacement_note(item.replaces_invoice_no) if item.replaces_invoice_no else "")


def attachment_name(item):
    invoice = item.invoice
    return safe_name(f"{invoice.invoice_no.value}_{item.short_name}_{amount_text(invoice.amount_cents.value)}_{item.category}.pdf", 240)


def ledger_name(policy, applicant, total_cents):
    return safe_name(policy.naming.ledger.replace("{applicant}", applicant).replace("{total}", amount_text(total_cents)), 240)


def load_history(store, history_hash):
    path = inside(store.batch / "history" / (history_hash + ".json"), store.root)
    raw = strict_json(path.read_bytes())
    checked = validated_history(raw)
    require(checked["history_hash"] == history_hash and raw == checked["validated_snapshot"],
            "SNAPSHOT_MISMATCH", "History changed", 3)
    return [HistoryEntry.model_validate(entry) for entry in checked["validated_snapshot"]]


def check_facts(store, item, decisions):
    """Snapshot facts must still match the attachment: re-parse text layers, and demand bound user confirmations."""
    invoice = item.invoice
    source, path = store.source(invoice.source_file_id)
    if source.detected_type == "invoice_pdf":
        require(all(fact.level == "extracted" for _, fact in invoice.facts()),
                "SNAPSHOT_MISMATCH", "Text invoice facts must be extracted", 3)
        try:
            parsed = parse_text_invoice(source, path.read_bytes())
        except CoreError:
            raise CoreError("SNAPSHOT_MISMATCH", "Attachment no longer parses", 3) from None
        require(same_business_values(invoice, parsed), "SNAPSHOT_MISMATCH", "Snapshot facts differ from attachment", 3)
    elif source.detected_type in ("image_invoice_pdf", "image"):
        require(source.detected_type == "image_invoice_pdf", "UNSUPPORTED_FILE", "Package requires PDF invoices", 3)
        require(invoice.trusted(), "FACT_UNCONFIRMED", "Unconfirmed facts cannot be packaged", 3)
        confirmations = [decision for decision in decisions if decision.kind == "confirm_visual"]
        for _, fact in invoice.facts():
            require(fact.level == "confirmed" and any(
                decision.source_event_id == fact.confirmation_event and decision.actor == fact.confirmed_by
                and fact.id in decision.typed().fact_ids for decision in confirmations),
                "FACT_UNCONFIRMED", "Visual confirmation decision required", 3)
    else:
        raise CoreError("UNSUPPORTED_FILE", "Package requires PDF invoices", 3)
    return source, path


def normalized_snapshot(raw):
    """The validated snapshot with every optional field spelled out; its hash is the execution key.

    Spelling a default explicitly or leaving it out therefore names the same snapshot and staging directory."""
    snapshot = Snapshot.model_validate(raw)
    return snapshot, snapshot.model_dump()


def validate_snapshot(raw, expected, policy, store):
    require(isinstance(raw, dict) and isinstance(expected, str), message="Invalid snapshot")
    snapshot, normalized = normalized_snapshot(raw)
    require(digest(normalized) == expected, "SNAPSHOT_MISMATCH", "Snapshot hash mismatch", 3)
    require(snapshot.policy_hash == policy.sha(), "SNAPSHOT_MISMATCH", "Policy changed", 3)
    history = load_history(store, snapshot.history_hash)
    decisions = {decision.id: decision for decision in snapshot.decisions}
    history_numbers = {entry.invoice_no for entry in history}
    seen, replaced, attachments, sources = set(), set(), set(), {}
    for item in snapshot.items:
        invoice = item.invoice
        applicable = [decisions[key] for key in item.decision_ids]
        kinds = {decision.kind for decision in applicable}
        # Rejected and receipt-only items never become ledger rows; their presence means a malformed snapshot.
        require(not kinds & {"reject", "receipt_only"}, message="Item is not a new reimbursement")
        require(invoice.buyer_name.value == policy.company.name and invoice.buyer_tax_id.value == policy.company.tax_id,
                "WRONG_BUYER", "Buyer does not match policy", 3)
        require(invoice.invoice_no.value not in seen and invoice.invoice_no.value not in history_numbers,
                "DUPLICATE_INVOICE", "Invoice already present", 3)
        seen.add(invoice.invoice_no.value)
        require(item.category in policy.categories, "INVALID_POLICY", "Unknown category")
        # The facts, not the chosen label, decide which rules apply; the label must agree with them.
        shown = check_category(invoice, policy.categories[item.category])
        require(invoice.source_file_id not in attachments, "LINK_CONFLICT", "Attachment used by two items", 3)
        attachments.add(invoice.source_file_id)
        sources[item.id] = check_facts(store, item, applicable)
        candidates = replacement_candidates(invoice, history, item.service_date)
        if candidates:
            require(len(candidates) == 1, "HISTORY_UNKNOWN", "Ambiguous replacement history", 3)
            previous = candidates[0]
            require(previous["reason"] == "REPLACEMENT_REQUIRES_DECISION", previous["reason"],
                    "Replacement cannot add reimbursement", 3)
            require(item.replaces_invoice_no == previous["invoice_no"] and item.service_date == previous["service_date"]
                    and invoice.amount_cents.value == previous["amount_cents"]
                    and any(d.kind == "replace_unpaid_invoice" and d.typed().invoice_no == previous["invoice_no"] for d in applicable),
                    "HISTORY_UNKNOWN", "Replacement decision required", 3)
            require(previous["invoice_no"] not in replaced, "LINK_CONFLICT", "History expense already occupied", 3)
            replaced.add(previous["invoice_no"])
        else:
            require(item.replaces_invoice_no is None, "HISTORY_UNKNOWN", "Replacement history not found", 3)
        if shown == "lodging":
            require(invoice.service_period is not None, "EVIDENCE_MISSING", "Stay duration required", 3)
            require(item.service_date == invoice.service_period.check_in.value, "FIELD_CONFLICT",
                    "Stay date differs from check-in", 3)
            require(not stay_over_limit(invoice, policy) or "explain_over_limit" in kinds, "EVIDENCE_MISSING",
                    "Over-limit explanation required", 3)
    sum_cents(item.invoice.amount_cents.value for item in snapshot.items)
    return snapshot, sources


def package(store, policy, confirmed_snapshot, expected_snapshot_hash):
    snapshot, sources = validate_snapshot(confirmed_snapshot, expected_snapshot_hash, policy, store)
    _, normalized = normalized_snapshot(confirmed_snapshot)
    staging = inside(store.batch / "staging" / expected_snapshot_hash, store.root, must_exist=False)
    rows, files, planned = [], [], {}
    totals = defaultdict(int)
    for item in sorted(snapshot.items, key=lambda row: (row.service_date, row.id)):
        invoice = item.invoice
        name = attachment_name(item)
        require(name not in planned, "LINK_CONFLICT", "Two items share an attachment name", 3)
        source, path = sources[item.id]
        planned[name] = path.read_bytes()
        category = policy.categories[item.category]
        row = dict(item_id=item.id, invoice_no=invoice.invoice_no.value, amount_cents=invoice.amount_cents.value,
                   service_date=item.service_date, btype=category.btype, summary=category.summary,
                   expense_detail=expense_detail(item), seller_name=invoice.seller_name.value, relative_name=name,
                   attachment_hash=source.sha256)
        row["business_hash"] = digest(row)
        rows.append(row)
        files.append(dict(relative_name=name, sha256=source.sha256, bytes=len(planned[name])))
        totals[category.summary] += invoice.amount_cents.value
    total = sum_cents(row["amount_cents"] for row in rows)
    ledger = ledger_name(policy, snapshot.applicant, total)
    require(ledger not in planned and ledger not in STAGED_RECORDS, "INVALID_POLICY", "Ledger name collides with an attachment")
    planned[ledger] = workbook(rows, dict(totals), snapshot.applicant)
    files.append(dict(relative_name=ledger, sha256=hashlib.sha256(planned[ledger]).hexdigest(), bytes=len(planned[ledger])))
    manifest = dict(schema_version=1, batch_id=snapshot.batch_id, revision=snapshot.revision,
                    snapshot_hash=expected_snapshot_hash, policy_hash=snapshot.policy_hash, history_hash=snapshot.history_hash,
                    ledger_name=ledger, files=files, rows=rows, total_cents=total, category_totals=dict(sorted(totals.items())))
    planned["snapshot.json"] = canonical(normalized)
    planned["manifest.json"] = canonical(manifest)
    # Idempotent: an earlier run of the same snapshot must have produced exactly these bytes.
    if staging.exists():
        present = {entry.name for entry in staging.iterdir()}
        require(present <= set(planned), "SNAPSHOT_MISMATCH", "Unexpected staged file", 3)
    for name, data in planned.items():
        path = inside(staging / name, store.root, must_exist=False)
        if path.exists():
            require(path.is_file() and path.read_bytes() == data, "SNAPSHOT_MISMATCH", "Existing staged artifact differs", 3)
        else:
            atomic_write(path, data)
    return {"staging_manifest": manifest, "manifest_object_id": expected_snapshot_hash}
