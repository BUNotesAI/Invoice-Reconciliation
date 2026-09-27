"""Conservative text-layer extraction. Ambiguous fields never silently win."""
import re
from datetime import date
from pydantic import ValidationError

from .errors import CoreError, require
from .models import Fact, Invoice, Source, ServicePeriod
from .storage import pdf_text
from .values import decimal_cents, cn_upper_to_cents

LABELS = {
    "invoice_no": "发票号码", "issue_date": "开票日期", "amount_cents": "价税合计小写",
    "amount_upper": "价税合计大写", "buyer_name": "购买方名称", "buyer_tax_id": "购买方税号",
    "seller_name": "销售方名称", "project": "项目", "remark": "备注", "order_ref": "订单号",
}


def text_fields(text):
    fields = {}
    for key, label in LABELS.items():
        matches = re.findall(r"(?:^|\n)\s*" + label + r"\s*[:：]\s*([^\n]*)", text)
        require(len(matches) <= 1, "FIELD_CONFLICT", "Multiple values for invoice field", 3)
        if matches:
            fields[key] = matches[0].strip()
    for name in set(LABELS) - {"order_ref", "remark"}:
        require(name in fields and bool(fields[name]), "FIELD_CONFLICT", "Missing invoice field", 3)
    fields.setdefault("remark", "")
    candidates = set(re.findall(r"(?<![0-9])[0-9]{20}(?![0-9])", text))
    require(fields["invoice_no"] in candidates, "FIELD_CONFLICT", "Invoice number cross-check failed", 3)
    # A second unrelated 20-digit value is ambiguous until a parser can identify its role.
    allowed = {fields["invoice_no"], fields.get("order_ref")}
    require(candidates <= allowed, "FIELD_CONFLICT", "Ambiguous invoice number", 3)
    fields["amount_cents"] = decimal_cents(fields["amount_cents"].removeprefix("¥").removeprefix("￥"))
    return fields


def extract(store, source_file_id, vision_candidate=None):
    source, path = store.source(source_file_id)
    if source.detected_type == "invoice_pdf":
        fields = text_fields(pdf_text(path.read_bytes()))
        level, method = "extracted", "text"
    elif source.detected_type in ("image_invoice", "image"):
        if vision_candidate is None:
            return {"invoice": None, "issues": ["VISION_REQUIRED"]}
        require(isinstance(vision_candidate, dict) and
                set(LABELS) - {"order_ref", "remark"} <= set(vision_candidate) <= set(LABELS),
                message="Invalid vision candidate fields")
        fields = dict(vision_candidate)
        fields.setdefault("remark", "")
        level, method = "candidate", "vision"
    else:
        raise CoreError("UNSUPPORTED_FILE", "Source is not a supported invoice")
    require(cn_upper_to_cents(fields["amount_upper"]) == fields["amount_cents"],
            "FIELD_CONFLICT", "Invoice amount cross-check failed", 3)
    facts = {}
    for key, value in fields.items():
        facts[key] = Fact(id=f"{source.id}:{key}:{level}", value=value, level=level,
                          source=Source(file_sha256=source.sha256, method=method, locator=f"field:{LABELS[key]}"),
                          validation_results=["format_checked", "amount_cross_checked"])
    issues = ["FACT_UNCONFIRMED"] if level == "candidate" else []
    period = None
    remark = fields["remark"]
    stay = re.search(r"入住\s*([0-9]{4}-[0-9]{2}-[0-9]{2}).*?离店\s*([0-9]{4}-[0-9]{2}-[0-9]{2})", remark)
    if stay and level != "candidate":
        try:
            nights = (date.fromisoformat(stay[2]) - date.fromisoformat(stay[1])).days
            explicit = re.search(r"([0-9]+)晚", remark)
            require(not explicit or int(explicit[1]) == nights, "FIELD_CONFLICT", "Stay nights differ from dates", 3)
            period = ServicePeriod(check_in=stay[1], check_out=stay[2], nights=nights)
        except (ValueError, ValidationError):
            raise CoreError("FIELD_CONFLICT", "Invalid service period", 3) from None
    if "住宿" in fields["project"] and period is None:
        issues.append("STAY_PERIOD_MISSING")
    try:
        invoice = Invoice(id="invoice-" + source.id, source_file_id=source.id, service_period=period, **facts)
    except ValidationError:
        raise CoreError("FIELD_CONFLICT", "Invoice fields failed validation", 3) from None
    return {"invoice": invoice.model_dump(), "issues": issues}
