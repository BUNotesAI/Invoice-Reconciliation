"""Payment and trip evidence parsed from bills, trip lists and confirmed screenshots."""
import re
from datetime import datetime
from decimal import Decimal, InvalidOperation
from typing import Annotated, Literal

from pydantic import Field, ValidationError, model_validator

from .errors import CoreError, require
from .models import Identifier, Record, Sha256, Source
from .render import vision_image
from .storage import WECHAT_HEADER, pdf_pages, wechat_header_row, xlsx_rows
from .values import cents, decimal_cents, local_date, utc_instant

PAID = {"支付成功", "已转账", "对方已收钱", "朋友已收钱", "已存入零钱", "已收钱"}
TRIP_ROW = re.compile(r"^\s*([0-9]{1,3})\s+(\S+)\s+([0-9]{4}-[0-9]{2}-[0-9]{2})\s+([0-9]{2}:[0-9]{2})\s+(\S+)\s+(\S+)\s+(\S+)"
                      r"\s+([0-9]+(?:\.[0-9]+)?)\s+([0-9]+(?:\.[0-9]{1,2})?)\s*$")
TRIP_TOTAL = re.compile(r"共\s*([0-9]+)\s*笔行程，?\s*合计\s*([0-9]+(?:\.[0-9]{1,2})?)\s*元")
SCREENSHOT_FIELDS = {"merchant", "amount_cents", "service_date", "order_ref"}


class Evidence(Record):
    id: Identifier
    source_file_id: Sha256
    kind: Literal["wechat_payment", "trip", "order_screenshot"]
    level: Literal["extracted", "candidate", "confirmed"]
    merchant: Annotated[str, Field(min_length=1, max_length=255)]
    goods: Annotated[str, Field(max_length=512)] = ""
    trade_type: Annotated[str, Field(max_length=64)] | None = None
    flow: Literal["支出", "收入", "中性"] | None = None
    amount_cents: int
    refund_cents: int = 0
    service_date: str | None = None
    payment_date: str | None = None
    transaction_ref: Annotated[str, Field(min_length=1, max_length=128)]
    payment_status: Literal["paid", "full_refund", "partial_refund", "unknown"] | None = None
    provenance: Source

    @model_validator(mode="after")
    def values(self):
        cents(self.amount_cents)
        cents(self.refund_cents)
        for day in (self.service_date, self.payment_date):
            if day is not None:
                local_date(day)
        if self.service_date is None and self.payment_date is None:
            raise ValueError("Evidence needs a service or payment date")
        if self.refund_cents > self.amount_cents:
            raise ValueError("Refund exceeds amount")
        is_payment = self.kind != "trip"
        if is_payment != (self.payment_status is not None):
            raise ValueError("Only payment evidence carries a payment status")
        if (self.level == "extracted") != (self.provenance.method in ("text", "parser")):
            raise ValueError("Evidence level differs from its provenance")
        return self

    def usable(self):
        """Candidates from vision never occupy a payment or prove a date."""
        return self.level != "candidate"


def _text(value):
    return "" if value is None else str(value).strip()


def _amount(value):
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        try:
            amount = Decimal(str(value)).quantize(Decimal("0.01"))
        except InvalidOperation:
            raise CoreError("FIELD_CONFLICT", "Unreadable bill amount", 3) from None
        return decimal_cents(format(amount, "f"))
    text = _text(value).removeprefix("¥").removeprefix("￥").replace(",", "")
    try:
        return decimal_cents(text)
    except CoreError:
        raise CoreError("FIELD_CONFLICT", "Unreadable bill amount", 3) from None


def _day(value):
    if isinstance(value, datetime):
        return value.date().isoformat()
    match = re.fullmatch(r"([0-9]{4}-[0-9]{2}-[0-9]{2})(?:[ T][0-9]{2}:[0-9]{2}(?::[0-9]{2})?)?", _text(value))
    require(match is not None, "FIELD_CONFLICT", "Unreadable bill time", 3)
    return local_date(match[1])


def _status(text):
    if text == "已全额退款":
        return "full_refund", None
    partial = re.fullmatch(r"已退款\s*[(（]\s*[¥￥]?\s*([0-9]+(?:\.[0-9]{1,2})?)\s*[)）]", text)
    if partial:
        return "partial_refund", decimal_cents(partial[1])
    return ("paid", None) if text in PAID else ("unknown", None)


def wechat_evidence(source, data):
    rows = xlsx_rows(data)
    header = wechat_header_row(rows)
    require(header is not None, "UNSUPPORTED_FILE", "Bill header not found")
    result, seen = [], set()
    for offset, row in enumerate(rows[header + 1:], header + 2):
        cells = [_text(value) for value in row[:len(WECHAT_HEADER)]] + [""] * (len(WECHAT_HEADER) - len(row))
        if not any(cells):
            continue
        _at, trade_type, merchant, goods, flow, _amount_text, _method, status, reference = cells[:9]
        require(flow in ("支出", "收入", "中性", "/"), "FIELD_CONFLICT", "Unknown bill direction", 3)
        require(bool(reference) and reference not in seen, "FIELD_CONFLICT", "Missing or repeated bill reference", 3)
        seen.add(reference)
        payment_status, refund = _status(status)
        amount_cents = _amount(row[5])
        try:
            result.append(Evidence(
                id=f"ev-{source.id[:12]}-r{offset}", source_file_id=source.id, kind="wechat_payment", level="extracted",
                merchant=merchant or "未知", goods=goods, trade_type=trade_type, flow="中性" if flow == "/" else flow,
                amount_cents=amount_cents, refund_cents=amount_cents if payment_status == "full_refund" else refund or 0,
                payment_date=_day(row[0]), transaction_ref=reference, payment_status=payment_status,
                provenance=Source(file_sha256=source.sha256, method="parser",
                                  locator={"type": "sheet_cell", "sheet": "微信支付账单明细", "cell": f"A{offset}"})))
        except ValidationError:
            raise CoreError("FIELD_CONFLICT", "Bill row failed validation", 3) from None
    return result


def trip_evidence(source, data):
    pages = pdf_pages(data)
    result = []
    for page_number, text in enumerate(pages, 1):
        for line in text.splitlines():
            match = TRIP_ROW.match(line)
            if not match:
                continue
            result.append(Evidence(
                id=f"ev-{source.id[:12]}-t{int(match[1])}", source_file_id=source.id, kind="trip", level="extracted",
                merchant="滴滴出行", goods=f"{match[2]} {match[5]} {match[6]}→{match[7]}", amount_cents=decimal_cents(match[9]),
                service_date=local_date(match[3]), transaction_ref=f"trip-{int(match[1])}",
                provenance=Source(file_sha256=source.sha256, method="parser",
                                  locator={"type": "pdf_page", "page": page_number, "field": f"row_{int(match[1])}"})))
    require(len({item.transaction_ref for item in result}) == len(result), "FIELD_CONFLICT", "Repeated trip number", 3)
    total = TRIP_TOTAL.search("\n".join(pages))
    # The stated summary line must agree with the rows read; otherwise a row was missed.
    if total is not None:
        require(int(total[1]) == len(result) and decimal_cents(total[2]) == sum(item.amount_cents for item in result),
                "FIELD_CONFLICT", "Trip rows differ from the stated total", 3)
    require(bool(result), "FIELD_CONFLICT", "No trip rows found", 3)
    return result


def screenshot_evidence(source, fields, level, confirmation=None):
    require(isinstance(fields, dict) and set(fields) == SCREENSHOT_FIELDS, message="Invalid screenshot fields")
    require(isinstance(fields["amount_cents"], int) and type(fields["amount_cents"]) is int, message="Invalid screenshot amount")
    if level == "confirmed":
        require(isinstance(confirmation, dict) and set(confirmation) == {"confirmed_by", "confirmation_event", "confirmed_at"},
                message="Invalid confirmation")
        utc_instant(confirmation["confirmed_at"])
        locator = {"type": "user_confirmation", "event_id": confirmation["confirmation_event"],
                   "actor": confirmation["confirmed_by"], "confirmed_at": confirmation["confirmed_at"],
                   "original_fact_id": f"ev-{source.id[:12]}-shot.candidate"}
        provenance = Source(file_sha256=source.sha256, method="user", locator=locator)
    else:
        provenance = Source(file_sha256=source.sha256, method="vision", locator={"type": "image_region", "field": "order"})
    try:
        return [Evidence(id=f"ev-{source.id[:12]}-shot", source_file_id=source.id, kind="order_screenshot", level=level,
                         merchant=fields["merchant"], amount_cents=fields["amount_cents"], service_date=fields["service_date"],
                         payment_date=fields["service_date"], transaction_ref=fields["order_ref"], payment_status="paid",
                         provenance=provenance)]
    except (ValidationError, CoreError):
        raise CoreError("FIELD_CONFLICT", "Screenshot reading failed validation", 3) from None


def evidence(store, source_file_id, vision_candidate=None, confirmed_visual_facts=None):
    source, path = store.source(source_file_id)
    if source.detected_type == "wechat_bill":
        require(vision_candidate is None and confirmed_visual_facts is None, message="Bills take no visual facts")
        return {"evidence": [item.model_dump() for item in wechat_evidence(source, path.read_bytes())], "issues": []}
    if source.detected_type == "didi_trip_pdf":
        require(vision_candidate is None and confirmed_visual_facts is None, message="Trip lists take no visual facts")
        return {"evidence": [item.model_dump() for item in trip_evidence(source, path.read_bytes())], "issues": []}
    if source.detected_type == "image":
        require(vision_candidate is None or confirmed_visual_facts is None, message="Give a candidate or a confirmation, not both")
        if confirmed_visual_facts is not None:
            require(isinstance(confirmed_visual_facts, dict) and set(confirmed_visual_facts) == {"fields", "confirmation"},
                    message="Invalid confirmed visual facts")
            items = screenshot_evidence(source, confirmed_visual_facts["fields"], "confirmed", confirmed_visual_facts["confirmation"])
            return {"evidence": [item.model_dump() for item in items], "issues": []}
        if vision_candidate is not None:
            items = screenshot_evidence(source, vision_candidate, "candidate")
            return {"evidence": [item.model_dump() for item in items], "issues": ["FACT_UNCONFIRMED"]}
        return {"evidence": [], "issues": ["VISION_REQUIRED"], "vision_image": vision_image(store, source)}
    raise CoreError("UNSUPPORTED_FILE", "Source is not evidence")
