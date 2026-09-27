"""Batch gates return per-item decisions without aborting the whole batch."""
from collections import Counter

from .errors import require
from .history import replacement_candidates, validated_history
from .models import HistoryEntry, Invoice


def gates(invoices, history_snapshot, policy):
    require(isinstance(invoices, list) and len(invoices) <= 100, message="Invalid invoices")
    invoices = [Invoice.model_validate(raw) for raw in invoices]
    require(len({invoice.id for invoice in invoices}) == len(invoices), message="Duplicate invoice identifiers")
    checked = validated_history(history_snapshot)
    history = [HistoryEntry.model_validate(raw) for raw in checked["validated_snapshot"]]
    counts = Counter(invoice.invoice_no.value for invoice in invoices if invoice.trusted())
    old_numbers = {entry.invoice_no for entry in history}
    results = []
    for invoice in invoices:
        reasons, replacements = [], []
        if not invoice.trusted():
            # Buyer and duplicate gates run again once the user has confirmed the reading.
            reasons.append("FACT_UNCONFIRMED")
            disposition = "needs_decision"
        else:
            if invoice.buyer_name.value != policy.company.name or invoice.buyer_tax_id.value != policy.company.tax_id:
                reasons.append("WRONG_BUYER")
            if counts[invoice.invoice_no.value] > 1 or invoice.invoice_no.value in old_numbers:
                reasons.append("DUPLICATE_INVOICE")
            disposition = "rejected" if reasons else "accepted"
            if not reasons:
                replacements = replacement_candidates(invoice, history)
                if replacements:
                    disposition = "needs_decision"
                    reasons.extend(sorted({row["reason"] for row in replacements}))
        results.append({"invoice_id": invoice.id, "invoice_no": invoice.invoice_no.value, "disposition": disposition,
                        "review_reasons": reasons, "replacement_candidates": replacements})
    # The hashes bind the orchestrator's later snapshot to the exact policy and history these gates used.
    return {"items": results, "history_hash": checked["history_hash"], "history_issues": checked["issues"],
            "policy_hash": policy.sha()}
