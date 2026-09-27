"""Strict wire records; untrusted facts remain candidates until confirmed."""
import re
from datetime import date
from typing import Annotated, Literal, Union

from pydantic import BaseModel, ConfigDict, Field, StrictBool, StrictInt, StrictStr, model_validator

from .values import cents, cn_upper_to_cents, invoice_number, local_date, safe_name, utc_instant

Sha256 = Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]
Identifier = Annotated[str, Field(pattern=r"^[A-Za-z0-9][A-Za-z0-9._:-]{0,159}$")]
ShortText = Annotated[str, Field(max_length=512)]
DECISION_KINDS = ("confirm_visual", "choose_evidence", "explain_over_limit", "replace_unpaid_invoice",
                  "receipt_only", "reject", "manual_evidence")
INVOICE_FIELDS = ("invoice_no", "issue_date", "amount_cents", "amount_upper", "buyer_name",
                  "buyer_tax_id", "seller_name", "project", "remark")


class Record(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True, frozen=True)


class PdfPage(Record):
    type: Literal["pdf_page"]
    page: Annotated[int, Field(ge=1, le=20)]
    field: Identifier
    bbox: tuple[StrictInt, StrictInt, StrictInt, StrictInt] | None = None


class SheetCell(Record):
    type: Literal["sheet_cell"]
    sheet: ShortText
    cell: Annotated[str, Field(pattern=r"^[A-Z]{1,3}[1-9][0-9]{0,6}$")]


class ImageRegion(Record):
    type: Literal["image_region"]
    field: Identifier
    region: tuple[StrictInt, StrictInt, StrictInt, StrictInt] | None = None


class JsonPointer(Record):
    type: Literal["json_pointer"]
    pointer: Annotated[str, Field(pattern=r"^(/[^/]*)*$", max_length=512)]


class UserConfirmation(Record):
    type: Literal["user_confirmation"]
    event_id: Identifier
    actor: Identifier | Annotated[str, Field(pattern=r"^@[^: ]+:[^ ]+$")]
    confirmed_at: str
    original_fact_id: Identifier

    @model_validator(mode="after")
    def instant(self):
        utc_instant(self.confirmed_at)
        return self


Locator = Annotated[Union[PdfPage, SheetCell, ImageRegion, JsonPointer, UserConfirmation], Field(discriminator="type")]


class Source(Record):
    file_sha256: Sha256
    method: Literal["text", "parser", "vision", "user"]
    locator: Locator


class Fact(Record):
    id: Identifier
    value: StrictStr | StrictInt
    level: Literal["extracted", "candidate", "confirmed"]
    source: Source
    validation_results: list[Identifier] = Field(default_factory=list)
    candidate_id: Identifier | None = None
    confirmed_by: str | None = None
    confirmation_event: Identifier | None = None
    confirmed_at: str | None = None

    @model_validator(mode="after")
    def provenance(self):
        confirmation = (self.candidate_id, self.confirmed_by, self.confirmation_event, self.confirmed_at)
        if self.level == "confirmed":
            if not all(confirmation):
                raise ValueError("Confirmation provenance is required")
            utc_instant(self.confirmed_at)
            locator = self.source.locator
            if self.source.method != "user" or not isinstance(locator, UserConfirmation):
                raise ValueError("Confirmed source must be a user confirmation")
            if (locator.event_id, locator.actor, locator.confirmed_at, locator.original_fact_id) != (
                    self.confirmation_event, self.confirmed_by, self.confirmed_at, self.candidate_id):
                raise ValueError("Confirmation locator differs from fact provenance")
        else:
            if any(item is not None for item in confirmation):
                raise ValueError("Only confirmed facts carry confirmation provenance")
            if self.level == "extracted" and self.source.method not in ("text", "parser"):
                raise ValueError("Extracted source must be deterministic")
            if self.level == "candidate" and self.source.method != "vision":
                raise ValueError("Candidate source must be vision")
        return self


class SourceFile(Record):
    id: Sha256
    sha256: Sha256
    byte_length: Annotated[int, Field(ge=0, le=20 * 1024 * 1024)]
    detected_type: Literal["invoice_pdf", "image_invoice_pdf", "didi_trip_pdf", "wechat_bill", "image", "unsupported"]
    original_name: Annotated[str, Field(max_length=255)]
    storage_object_id: Sha256

    @model_validator(mode="after")
    def content_addressed(self):
        if not self.id == self.sha256 == self.storage_object_id:
            raise ValueError("Source identity must be its content hash")
        return self


class ServicePeriod(Record):
    check_in: str
    check_out: str
    nights: Annotated[int, Field(gt=0, le=366)]

    @model_validator(mode="after")
    def consistent(self):
        local_date(self.check_in)
        local_date(self.check_out)
        if (date.fromisoformat(self.check_out) - date.fromisoformat(self.check_in)).days != self.nights:
            raise ValueError("Stay duration differs from dates")
        return self


class Invoice(Record):
    id: Identifier
    source_file_id: Sha256
    invoice_no: Fact
    issue_date: Fact
    amount_cents: Fact
    amount_upper: Fact
    buyer_name: Fact
    buyer_tax_id: Fact
    seller_name: Fact
    project: Fact
    remark: Fact
    order_ref: Fact | None = None
    service_period: ServicePeriod | None = None

    @model_validator(mode="after")
    def values(self):
        invoice_number(self.invoice_no.value)
        local_date(self.issue_date.value)
        cents(self.amount_cents.value)
        if cn_upper_to_cents(self.amount_upper.value) != self.amount_cents.value:
            raise ValueError("Uppercase and decimal amounts differ")
        # Personal-title invoices carry no tax identifier; the buyer gate rejects them.
        if not isinstance(self.buyer_tax_id.value, str) or not re.fullmatch(r"(?:[0-9A-Z]{18})?", self.buyer_tax_id.value):
            raise ValueError("Invalid tax identifier")
        for field in (self.buyer_name, self.seller_name, self.project):
            if not isinstance(field.value, str) or not field.value or len(field.value) > 512 or field.value != field.value.strip():
                raise ValueError("Invalid invoice text")
        if not isinstance(self.remark.value, str) or len(self.remark.value) > 4096:
            raise ValueError("Invalid invoice remark")
        if self.order_ref is not None and (not isinstance(self.order_ref.value, str)
                                           or not re.fullmatch(r"[A-Za-z0-9-]{1,64}", self.order_ref.value)):
            raise ValueError("Invalid order reference")
        for name in INVOICE_FIELDS + ("order_ref",):
            fact = getattr(self, name)
            if fact is not None and fact.source.file_sha256 != self.source_file_id:
                raise ValueError("Fact source differs from invoice source")
        return self

    def facts(self):
        return [(name, getattr(self, name)) for name in INVOICE_FIELDS + ("order_ref",) if getattr(self, name) is not None]

    def trusted(self):
        return all(fact.level != "candidate" for _, fact in self.facts())


class HistoryEvent(Record):
    status: Literal["submitted", "approved", "paid", "voided"]
    actor: Annotated[str, Field(min_length=1, max_length=255)]
    at: str
    note: Annotated[str, Field(max_length=1024)]

    @model_validator(mode="after")
    def instant(self):
        utc_instant(self.at)
        return self


# Allowed predecessor of each appended history status; voided keeps earlier payment facts.
HISTORY_TRANSITIONS = {"submitted": {None}, "approved": {"submitted"}, "paid": {"approved"},
                       "voided": {"submitted", "approved", "paid"}}


class HistoryEntry(Record):
    invoice_no: str
    order_ref: Annotated[str, Field(pattern=r"^[A-Za-z0-9-]{1,64}$")] | None
    seller_name: Annotated[str, Field(min_length=1, max_length=512)]
    service_date: str
    amount_cents: StrictInt
    batch_id: Identifier
    revision: Annotated[int, Field(ge=0)]
    current_status: Literal["submitted", "approved", "paid", "voided", "unknown"]
    events: list[HistoryEvent]
    events_complete: StrictBool = False
    initial_paid: StrictBool | None = None
    replaces_invoice_no: str | None = None

    @model_validator(mode="after")
    def values(self):
        invoice_number(self.invoice_no)
        local_date(self.service_date)
        cents(self.amount_cents)
        if self.replaces_invoice_no is not None:
            invoice_number(self.replaces_invoice_no)
        if any(a.at > b.at for a, b in zip(self.events, self.events[1:])):
            raise ValueError("History events out of order")
        if self.events:
            if self.current_status != self.events[-1].status:
                raise ValueError("History projection differs from final event")
            # A complete stream starts at submission; a partial import is only checked between its events.
            previous = None if self.events_complete else self.events[0].status
            for event in (self.events if self.events_complete else self.events[1:]):
                if previous not in HISTORY_TRANSITIONS[event.status]:
                    raise ValueError("Invalid history transition")
                previous = event.status
        elif self.events_complete:
            raise ValueError("A complete history needs its submission event")
        return self

    def payment_knowledge(self):
        """paid / unpaid / unknown. A void never erases an earlier payment."""
        if self.initial_paid is True or any(event.status == "paid" for event in self.events):
            return "paid"
        if self.events_complete and self.initial_paid is False:
            return "unpaid"
        return "unknown"


class ConfirmVisual(Record):
    fact_ids: Annotated[list[Identifier], Field(min_length=1, max_length=16)]


class ChooseEvidence(Record):
    evidence_id: Identifier


class ExplainOverLimit(Record):
    explanation: Annotated[str, Field(min_length=1, max_length=1024)]

    @model_validator(mode="after")
    def meaningful(self):
        if not self.explanation.strip():
            raise ValueError("Explanation is empty")
        return self


class ReplaceUnpaidInvoice(Record):
    invoice_no: str

    @model_validator(mode="after")
    def number(self):
        invoice_number(self.invoice_no)
        return self


class ReceiptOnly(Record):
    replaces_invoice_no: str

    @model_validator(mode="after")
    def number(self):
        invoice_number(self.replaces_invoice_no)
        return self


class Reject(Record):
    reason: Annotated[str, Field(min_length=1, max_length=512)]


class ManualEvidence(Record):
    service_date: str
    note: Annotated[str, Field(min_length=1, max_length=1024)]

    @model_validator(mode="after")
    def day(self):
        local_date(self.service_date)
        return self


PAYLOADS = {"confirm_visual": ConfirmVisual, "choose_evidence": ChooseEvidence, "explain_over_limit": ExplainOverLimit,
            "replace_unpaid_invoice": ReplaceUnpaidInvoice, "receipt_only": ReceiptOnly, "reject": Reject,
            "manual_evidence": ManualEvidence}


class Decision(Record):
    id: Identifier
    item_id: Identifier
    kind: Literal[DECISION_KINDS]
    payload: dict
    actor: Annotated[str, Field(min_length=1, max_length=255)]
    at: str
    expected_revision: Annotated[int, Field(ge=0)]
    source_event_id: Identifier

    @model_validator(mode="after")
    def typed_payload(self):
        utc_instant(self.at)
        PAYLOADS[self.kind].model_validate(self.payload)
        return self

    def typed(self):
        return PAYLOADS[self.kind].model_validate(self.payload)


class PackageItem(Record):
    id: Identifier
    invoice: Invoice
    service_date: str
    category: str
    short_name: str
    expense_detail: Annotated[str, Field(max_length=2048)]
    decision_ids: list[Identifier]
    replaces_invoice_no: str | None = None

    @model_validator(mode="after")
    def values(self):
        local_date(self.service_date)
        safe_name(self.category)
        safe_name(self.short_name)
        if self.replaces_invoice_no is not None:
            invoice_number(self.replaces_invoice_no)
        if len(set(self.decision_ids)) != len(self.decision_ids):
            raise ValueError("Duplicate decision reference")
        return self


class Snapshot(Record):
    batch_id: Identifier
    revision: Annotated[int, Field(ge=0)]
    applicant: str
    period: Annotated[str, Field(pattern=r"^[0-9]{4}-[0-9]{2}$")]
    policy_hash: Sha256
    history_hash: Sha256
    items: Annotated[list[PackageItem], Field(min_length=1, max_length=100)]
    decisions: Annotated[list[Decision], Field(max_length=1000)]

    @model_validator(mode="after")
    def values(self):
        safe_name(self.applicant, 40)
        local_date(self.period + "-01")
        items = {item.id for item in self.items}
        if len(items) != len(self.items):
            raise ValueError("Duplicate item identifiers")
        if len({decision.id for decision in self.decisions}) != len(self.decisions):
            raise ValueError("Duplicate decision identifiers")
        referenced = [key for item in self.items for key in item.decision_ids]
        by_id = {decision.id: decision for decision in self.decisions}
        # Every decision is referenced exactly once, by the item it names, and within this revision.
        if sorted(referenced) != sorted(by_id):
            raise ValueError("Decisions and item references differ")
        for item in self.items:
            for key in item.decision_ids:
                if by_id[key].item_id != item.id:
                    raise ValueError("Decision is bound to another item")
        if any(decision.expected_revision > self.revision for decision in self.decisions):
            raise ValueError("Decision is newer than the snapshot")
        return self


class ManifestFile(Record):
    relative_name: str
    sha256: Sha256
    bytes: Annotated[int, Field(ge=0)]

    @model_validator(mode="after")
    def relative(self):
        safe_name(self.relative_name, 240)
        return self


class ManifestRow(Record):
    item_id: Identifier
    invoice_no: str
    amount_cents: StrictInt
    service_date: str
    btype: str
    summary: str
    expense_detail: str
    seller_name: str
    relative_name: str
    attachment_hash: Sha256
    business_hash: Sha256


class Manifest(Record):
    schema_version: Literal[1]
    batch_id: Identifier
    revision: Annotated[int, Field(ge=0)]
    snapshot_hash: Sha256
    policy_hash: Sha256
    history_hash: Sha256
    ledger_name: str
    files: list[ManifestFile]
    rows: list[ManifestRow]
    total_cents: StrictInt
    category_totals: dict[str, StrictInt]
