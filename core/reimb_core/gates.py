"""Batch gates return per-item decisions without aborting the whole batch."""
from collections import Counter
from .models import Invoice, HistoryEntry
from .history import validated_history, replacement_candidates


def gates(invoices, history_snapshot, policy):
    invoices = [Invoice.model_validate(raw) for raw in invoices]
    history = [HistoryEntry.model_validate(raw) for raw in validated_history(history_snapshot)["validated_snapshot"]]
    counts = Counter(invoice.invoice_no.value for invoice in invoices if invoice.trusted())
    old_numbers = {entry.invoice_no for entry in history}
    results = []
    for invoice in invoices:
        reasons, replacements = [], []
        disposition = "accepted"
        if not invoice.trusted():
            reasons.append("FACT_UNCONFIRMED")
            disposition = "needs_decision"
        else:
            if (invoice.buyer_name.value != policy.company.name or invoice.buyer_tax_id.value != policy.company.tax_id):
                reasons.append("WRONG_BUYER")
            if counts[invoice.invoice_no.value] > 1 or invoice.invoice_no.value in old_numbers:
                reasons.append("DUPLICATE_INVOICE")
            if reasons:
                disposition = "rejected"
            else:
                replacements = replacement_candidates(invoice, history)
                if replacements:
                    disposition = "needs_decision"
                    reasons.extend(sorted({r["reason"] for r in replacements}))
        results.append({"invoice_id": invoice.id, "disposition": disposition,
                        "review_reasons": reasons, "replacement_candidates": replacements})
    return {"items": results}
