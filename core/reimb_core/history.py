"""History projection preserves payment evidence across later void events."""
from .models import HistoryEntry
from .errors import require
from .values import digest


def validated_history(entries):
    require(isinstance(entries, list) and len(entries) <= 10000, message="Invalid history entries")
    records = [HistoryEntry.model_validate(entry) for entry in entries]
    require(len({entry.invoice_no for entry in records}) == len(records), message="Duplicate history invoice")
    rows = [entry.model_dump() for entry in sorted(records, key=lambda entry: entry.invoice_no)]
    issues = [{"invoice_no": entry.invoice_no, "reason": "HISTORY_UNKNOWN"} for entry in records
              if entry.payment_knowledge() == "unknown"]
    return {"validated_snapshot": rows, "history_hash": digest(rows), "issues": issues}


def replacement_candidates(invoice, entries, service_date=None):
    result = []
    for entry in entries:
        same_order = invoice.order_ref and invoice.order_ref.level != "candidate" and entry.order_ref and invoice.order_ref.value == entry.order_ref
        same_expense = (service_date is not None and entry.service_date == service_date
                        and entry.seller_name == invoice.seller_name.value and entry.amount_cents == invoice.amount_cents.value)
        if entry.invoice_no != invoice.invoice_no.value and (same_order or same_expense):
            knowledge = entry.payment_knowledge()
            reason = ("HISTORY_ALREADY_PAID" if knowledge == "paid" else
                      "REPLACEMENT_REQUIRES_DECISION" if knowledge == "unpaid" and entry.current_status == "voided" else
                      "HISTORY_UNKNOWN")
            result.append({"invoice_no": entry.invoice_no, "reason": reason, "service_date": entry.service_date})
    return result
