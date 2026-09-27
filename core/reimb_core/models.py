"""Strict wire records; untrusted facts remain candidates until confirmed."""
from typing import Annotated, Literal
from pydantic import BaseModel, ConfigDict, Field, StrictInt, StrictStr, field_validator, model_validator
from .values import cents, invoice_number, local_date, safe_name


class Record(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True)


class Source(Record):
    file_sha256: Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]
    method: Literal["text", "parser", "vision", "user"]
    locator: Annotated[str, Field(min_length=1, max_length=512)]


class Fact(Record):
    id: str
    value: StrictStr | StrictInt
    level: Literal["extracted", "candidate", "confirmed"]
    source: Source
    validation_results: list[str] = Field(default_factory=list)
    candidate_id: str | None = None
    confirmed_by: str | None = None
    confirmation_event: str | None = None
    confirmed_at: str | None = None

    @model_validator(mode="after")
    def provenance(self):
        if self.level == "confirmed":
            if not all((self.candidate_id, self.confirmed_by, self.confirmation_event, self.confirmed_at)):
                raise ValueError("Confirmation provenance is required")
            from datetime import datetime
            if not self.confirmed_at.endswith("Z"):
                raise ValueError("UTC confirmation time required")
            datetime.fromisoformat(self.confirmed_at.replace("Z", "+00:00"))
            if self.source.method != "user":
                raise ValueError("Confirmed source must be user")
        elif self.level == "extracted" and self.source.method not in ("text", "parser"):
            raise ValueError("Extracted source must be deterministic")
        elif self.level == "candidate" and self.source.method != "vision":
            raise ValueError("Candidate source must be vision")
        return self


class SourceFile(Record):
    id: str
    sha256: Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]
    byte_length: Annotated[int, Field(ge=0, le=20*1024*1024)]
    detected_type: Literal["invoice_pdf", "image_invoice", "image", "wechat_csv", "trip_csv", "unsupported"]
    original_name: Annotated[str, Field(max_length=255)]
    storage_object_id: Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]


class ServicePeriod(Record):
    check_in: str
    check_out: str
    nights: Annotated[int, Field(gt=0, le=366)]

    @model_validator(mode="after")
    def consistent(self):
        from datetime import date
        local_date(self.check_in)
        local_date(self.check_out)
        if (date.fromisoformat(self.check_out) - date.fromisoformat(self.check_in)).days != self.nights:
            raise ValueError("Stay duration differs from dates")
        return self


class Invoice(Record):
    id: str
    source_file_id: str
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
        from .values import cn_upper_to_cents
        import re
        invoice_number(self.invoice_no.value)
        local_date(self.issue_date.value)
        cents(self.amount_cents.value)
        if cn_upper_to_cents(self.amount_upper.value) != self.amount_cents.value:
            raise ValueError("Uppercase and decimal amounts differ")
        if not isinstance(self.buyer_tax_id.value, str) or not re.fullmatch(r"[0-9A-Z]{18}", self.buyer_tax_id.value):
            raise ValueError("Invalid tax identifier")
        for field in (self.buyer_name, self.seller_name, self.project):
            if not isinstance(field.value, str) or not field.value or len(field.value) > 512:
                raise ValueError("Invalid invoice text")
        if not isinstance(self.remark.value, str) or len(self.remark.value) > 4096:
            raise ValueError("Invalid invoice remark")
        return self

    def trusted(self):
        return all(getattr(self, name).level != "candidate" for name in
                   ("invoice_no", "issue_date", "amount_cents", "amount_upper", "buyer_name",
                    "buyer_tax_id", "seller_name", "project", "remark"))


class HistoryEvent(Record):
    status: Literal["submitted", "approved", "paid", "voided"]
    actor: str
    at: str
    note: str

    @field_validator("at")
    @classmethod
    def utc(cls, value):
        from datetime import datetime
        if not value.endswith("Z"):
            raise ValueError("UTC time required")
        datetime.fromisoformat(value.replace("Z", "+00:00"))
        return value


class HistoryEntry(Record):
    invoice_no: str
    order_ref: str | None
    seller_name: str
    service_date: str
    amount_cents: int
    batch_id: str
    revision: Annotated[int, Field(ge=0)]
    current_status: Literal["submitted", "approved", "paid", "voided", "unknown"]
    events: list[HistoryEvent]
    events_complete: bool = False
    initial_paid: bool | None = None
    replaces_invoice_no: str | None = None

    @model_validator(mode="after")
    def values(self):
        invoice_number(self.invoice_no)
        local_date(self.service_date)
        cents(self.amount_cents)
        if self.replaces_invoice_no:
            invoice_number(self.replaces_invoice_no)
        if self.events and self.current_status != self.events[-1].status:
            raise ValueError("History projection differs from final event")
        if any(a.at > b.at for a, b in zip(self.events, self.events[1:])):
            raise ValueError("History events out of order")
        return self

    def payment_knowledge(self):
        if self.initial_paid is True or any(e.status == "paid" for e in self.events):
            return "paid"
        if self.events_complete and self.initial_paid is False:
            return "unpaid"
        return "unknown"


class PackageItem(Record):
    id: str
    invoice: Invoice
    service_date: str
    category: str
    short_name: str
    expense_detail: str
    decision_ids: list[str]
    replaces_invoice_no: str | None = None

    @model_validator(mode="after")
    def values(self):
        safe_name(self.id)
        local_date(self.service_date)
        safe_name(self.category)
        safe_name(self.short_name)
        if len(self.expense_detail) > 2048:
            raise ValueError("Expense detail too long")
        if self.replaces_invoice_no:
            invoice_number(self.replaces_invoice_no)
        return self


class Snapshot(Record):
    batch_id: str
    revision: Annotated[int, Field(ge=0)]
    applicant: str
    period: Annotated[str, Field(pattern=r"^[0-9]{4}-[0-9]{2}$")]
    policy_hash: Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]
    history_hash: Annotated[str, Field(pattern=r"^[0-9a-f]{64}$")]
    items: Annotated[list[PackageItem], Field(min_length=1, max_length=100)]
    decisions: list[dict]

    @model_validator(mode="after")
    def values(self):
        safe_name(self.batch_id)
        safe_name(self.applicant)
        local_date(self.period + "-01")
        if len({i.id for i in self.items}) != len(self.items):
            raise ValueError("Duplicate item identifiers")
        return self
