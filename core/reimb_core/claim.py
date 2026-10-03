"""Missing-invoice follow-up: match an invoice that arrived later to an open missing payment, or look for one.

The core only judges facts: buyer title, amount, dates, merchant and duplicates. Who may claim, and what happens to
the batch, is the orchestrator's decision."""
from datetime import date

from .errors import require
from .history import validated_history
from .link import REMARK_DATE, merchant_matches
from .models import HistoryEntry, Invoice
from .values import local_date

OPEN = {"discovered", "business", "waiting"}
SPEND_FIELDS = {"id", "payment_evidence_id", "merchant", "amount_cents", "payment_date", "status", "deadline"}


def spends_of(raw):
    require(isinstance(raw, list) and len(raw) <= 200, message="Invalid missing spends")
    spends = []
    for spend in raw:
        require(isinstance(spend, dict) and SPEND_FIELDS <= set(spend), message="Invalid missing spend")
        require(isinstance(spend["amount_cents"], int) and spend["amount_cents"] > 0, message="Invalid missing amount")
        local_date(spend["payment_date"])
        local_date(spend["deadline"])
        spends.append(spend)
    require(len({spend["id"] for spend in spends}) == len(spends), message="Duplicate missing spends")
    return spends


def service_date(invoice):
    """The day the invoice says the service happened, if it says so."""
    if invoice.service_period is not None:
        return invoice.service_period.check_in.value
    found = REMARK_DATE.search(invoice.remark.value)
    return found.group(1) if found else None


def date_fits(invoice, spend):
    paid = date.fromisoformat(spend["payment_date"])
    if invoice.service_period is not None:
        # A stay is paid on arrival, at checkout or in between.
        start = date.fromisoformat(invoice.service_period.check_in.value)
        end = date.fromisoformat(invoice.service_period.check_out.value)
        return (start - paid).days <= 1 and (paid - end).days <= 1
    served = service_date(invoice)
    if served is not None:
        return abs((date.fromisoformat(served) - paid).days) <= 1
    # Without a service date the invoice can only be for a payment made on or before its issue day.
    return paid <= date.fromisoformat(invoice.issue_date.value) <= date.fromisoformat(spend["deadline"])


def claim(missing_spends, history_snapshot, policy, invoice=None, missing_id=None):
    spends = [spend for spend in spends_of(missing_spends) if spend["status"] in OPEN]
    history = [HistoryEntry.model_validate(row) for row in validated_history(history_snapshot)["validated_snapshot"]]
    if invoice is None:
        return search(spends, history, policy, missing_id)
    invoice = Invoice.model_validate(invoice)
    same_merchant = [spend for spend in spends
                     if merchant_matches(spend["merchant"], invoice.seller_name.value, invoice.remark.value, policy.short_names)]
    if not same_merchant:
        return {"result": "rejected", "reason": "NO_MISSING_MATCH", "missing_id": None, "next_batch_item": None}
    if not invoice.trusted():
        return {"result": "needs_decision", "reason": "FACT_UNCONFIRMED", "missing_id": None, "next_batch_item": None}
    amount = [spend for spend in same_merchant if spend["amount_cents"] == invoice.amount_cents.value]
    if not amount:
        target = same_merchant[0]["id"] if len(same_merchant) == 1 else None
        return {"result": "rejected", "reason": "AMOUNT_MISMATCH", "missing_id": target, "next_batch_item": None}
    dated = [spend for spend in amount if date_fits(invoice, spend)]
    if not dated:
        target = amount[0]["id"] if len(amount) == 1 else None
        return {"result": "rejected", "reason": "DATE_CONFLICT", "missing_id": target, "next_batch_item": None}
    if len(dated) > 1:
        return {"result": "needs_decision", "reason": "MULTIPLE_CANDIDATES", "missing_id": None, "next_batch_item": None}
    spend = dated[0]
    # A matching invoice with the wrong title or a number already reimbursed is refused for that payment.
    if invoice.buyer_name.value != policy.company.name or invoice.buyer_tax_id.value != policy.company.tax_id:
        return {"result": "rejected", "reason": "WRONG_BUYER", "missing_id": spend["id"], "next_batch_item": None}
    if invoice.invoice_no.value in {entry.invoice_no for entry in history}:
        return {"result": "rejected", "reason": "DUPLICATE_INVOICE", "missing_id": spend["id"], "next_batch_item": None}
    return {"result": "match", "reason": "MATCHED", "missing_id": spend["id"],
            "next_batch_item": {"source_file_id": invoice.source_file_id, "invoice_no": invoice.invoice_no.value,
                                "payment_evidence_id": spend["payment_evidence_id"], "service_date": spend["payment_date"]}}


def search(spends, history, policy, missing_id):
    """「先找」: an invoice for this payment may already be in the reimbursement history."""
    require(isinstance(missing_id, str), message="Search needs a missing id")
    spend = next((spend for spend in spends if spend["id"] == missing_id), None)
    require(spend is not None, message="Unknown or closed missing spend")
    found = sorted(entry.invoice_no for entry in history
                   if entry.amount_cents == spend["amount_cents"]
                   and merchant_matches(spend["merchant"], entry.seller_name, "", policy.short_names))
    return {"result": "found" if found else "not_found", "missing_id": missing_id, "invoice_nos": found}
