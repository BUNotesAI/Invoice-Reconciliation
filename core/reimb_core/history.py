"""History projection preserves payment evidence across later void events."""
from typing import Annotated, Literal

from pydantic import Field, model_validator

from .errors import require
from .models import HISTORY_TRANSITIONS, HistoryEntry, HistoryEvent, Identifier, Record
from .values import digest, invoice_number


class Expense(Record):
    order_ref: Annotated[str, Field(pattern=r"^[A-Za-z0-9-]{1,64}$")] | None
    seller_name: Annotated[str, Field(min_length=1, max_length=512)]
    service_date: str
    amount_cents: int
    batch_id: Identifier
    revision: Annotated[int, Field(ge=0)]
    replaces_invoice_no: str | None = None


class LedgerEvent(Record):
    """One appended history event; a submission also carries the expense it registers."""
    invoice_no: str
    status: Literal["submitted", "approved", "paid", "voided"]
    actor: Annotated[str, Field(min_length=1, max_length=255)]
    at: str
    note: Annotated[str, Field(max_length=1024)]
    expense: Expense | None = None

    @model_validator(mode="after")
    def shape(self):
        invoice_number(self.invoice_no)
        if (self.status == "submitted") != (self.expense is not None):
            raise ValueError("Only a submission carries the expense")
        return self


def snapshot(records):
    require(len({entry.invoice_no for entry in records}) == len(records), message="Duplicate history invoice")
    rows = [entry.model_dump() for entry in sorted(records, key=lambda entry: entry.invoice_no)]
    issues = [{"invoice_no": entry.invoice_no, "reason": "HISTORY_UNKNOWN"}
              for entry in sorted(records, key=lambda entry: entry.invoice_no) if entry.payment_knowledge() == "unknown"]
    return {"validated_snapshot": rows, "history_hash": digest(rows), "issues": issues}


def validated_history(entries):
    require(isinstance(entries, list) and len(entries) <= 10000, message="Invalid history entries")
    return snapshot([HistoryEntry.model_validate(entry) for entry in entries])


def project(events):
    """Fold an ordered event log into entries. The log is complete, so payment knowledge is exact."""
    require(isinstance(events, list) and len(events) <= 50000, message="Invalid history events")
    streams = {}
    for raw in events:
        event = LedgerEvent.model_validate(raw)
        stream = streams.get(event.invoice_no)
        previous = stream["events"][-1].status if stream else None
        require(previous in HISTORY_TRANSITIONS[event.status], message="Invalid history transition")
        if stream is None:
            stream = streams[event.invoice_no] = {"expense": event.expense, "events": []}
        stream["events"].append(HistoryEvent(status=event.status, actor=event.actor, at=event.at, note=event.note))
    records = [HistoryEntry(invoice_no=number, current_status=stream["events"][-1].status, events=stream["events"],
                            events_complete=True, initial_paid=False, **stream["expense"].model_dump())
               for number, stream in streams.items()]
    return snapshot(records)


def replacement_candidates(invoice, entries, service_date=None):
    """History rows that the invoice may re-issue: same order reference, or same expense on the same day."""
    result = []
    for entry in entries:
        same_order = (invoice.order_ref is not None and invoice.order_ref.level != "candidate"
                      and entry.order_ref is not None and invoice.order_ref.value == entry.order_ref)
        same_expense = (service_date is not None and entry.service_date == service_date
                        and entry.seller_name == invoice.seller_name.value and entry.amount_cents == invoice.amount_cents.value)
        if entry.invoice_no != invoice.invoice_no.value and (same_order or same_expense):
            knowledge = entry.payment_knowledge()
            reason = ("HISTORY_ALREADY_PAID" if knowledge == "paid" else
                      "REPLACEMENT_REQUIRES_DECISION" if knowledge == "unpaid" and entry.current_status == "voided" else
                      "HISTORY_UNKNOWN")
            result.append({"invoice_no": entry.invoice_no, "reason": reason, "service_date": entry.service_date,
                           "amount_cents": entry.amount_cents})
    return sorted(result, key=lambda row: row["invoice_no"])
