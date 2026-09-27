"""Conservative text-layer extraction. Ambiguous fields never silently win."""
import re
from datetime import date

from pydantic import ValidationError

from .errors import CoreError, require
from .models import INVOICE_FIELDS, Fact, Invoice, PdfPage, ImageRegion, ServicePeriod, Source
from .storage import pdf_pages
from .values import cn_upper_to_cents, decimal_cents

LABELS = {
    "invoice_no": "发票号码", "issue_date": "开票日期", "amount_cents": "价税合计小写",
    "amount_upper": "价税合计大写", "buyer_name": "购买方名称", "buyer_tax_id": "购买方税号",
    "seller_name": "销售方名称", "project": "项目", "remark": "备注", "order_ref": "订单号",
}
OPTIONAL = {"order_ref", "remark", "buyer_tax_id"}
STAY = re.compile(r"入住\s*([0-9]{4}-[0-9]{2}-[0-9]{2})\s*离店\s*([0-9]{4}-[0-9]{2}-[0-9]{2})")


def conflict(message):
    return CoreError("FIELD_CONFLICT", message, 3)


def text_fields(pages):
    """Labelled values and the 1-based page each came from."""
    fields, located = {}, {}
    for number, text in enumerate(pages, 1):
        for key, label in LABELS.items():
            for match in re.finditer(r"(?:^|\n)[ \t]*" + label + r"[ \t]*[:：][ \t]*([^\n]*)", text):
                if key in fields:
                    raise conflict("Multiple values for invoice field")
                fields[key], located[key] = match[1].strip(), number
    for name in set(LABELS) - OPTIONAL:
        if not fields.get(name):
            raise conflict("Missing invoice field")
    for name in OPTIONAL - {"order_ref"}:
        fields.setdefault(name, "")
        located.setdefault(name, located["invoice_no"])
    if "order_ref" in fields and not fields["order_ref"]:
        del fields["order_ref"], located["order_ref"]
    # Dual channel: the labelled number must be the only 20-digit value that is not the order reference.
    candidates = set(re.findall(r"(?<![0-9])[0-9]{20}(?![0-9])", "\n".join(pages)))
    if fields["invoice_no"] not in candidates or not candidates <= {fields["invoice_no"], fields.get("order_ref")}:
        raise conflict("Invoice number cross-check failed")
    amount = fields["amount_cents"].removeprefix("¥").removeprefix("￥").strip()
    try:
        fields["amount_cents"] = decimal_cents(amount)
    except CoreError:
        raise conflict("Invalid decimal amount") from None
    return fields, located


def service_period(remark):
    stay = STAY.search(remark)
    if stay is None:
        return None
    try:
        nights = (date.fromisoformat(stay[2]) - date.fromisoformat(stay[1])).days
        explicit = re.search(r"([0-9]+)\s*晚", remark)
        if explicit and int(explicit[1]) != nights:
            raise conflict("Stay nights differ from dates")
        return ServicePeriod(check_in=stay[1], check_out=stay[2], nights=nights)
    except (ValueError, ValidationError):
        raise conflict("Invalid service period") from None


def build_invoice(source, fields, locate, level, method):
    try:
        upper = cn_upper_to_cents(fields["amount_upper"])
    except CoreError:
        raise conflict("Invalid uppercase amount") from None
    if upper != fields["amount_cents"]:
        raise conflict("Invoice amount cross-check failed")
    try:
        facts = {key: Fact(id=f"{source.id[:16]}.{key}.{level}", value=value, level=level,
                           source=Source(file_sha256=source.sha256, method=method, locator=locate(key)),
                           validation_results=["format_checked", "amount_cross_checked"])
                 for key, value in fields.items()}
    except ValidationError:
        raise conflict("Invoice field is not valid text") from None
    # Stay dates come only from deterministic text; a candidate remark cannot set nights.
    period = service_period(fields["remark"]) if level == "extracted" else None
    try:
        return Invoice(id="invoice-" + source.id[:16], source_file_id=source.id, service_period=period, **facts)
    except (ValidationError, CoreError):
        # A document reading that fails format checks goes to review, it is not a malformed request.
        raise conflict("Invoice fields failed validation") from None


def parse_text_invoice(source, data):
    fields, located = text_fields(pdf_pages(data))
    return build_invoice(source, fields, lambda key: PdfPage(type="pdf_page", page=located[key], field=key),
                         "extracted", "text")


def extract(store, source_file_id, vision_candidate=None):
    source, path = store.source(source_file_id)
    if source.detected_type == "invoice_pdf":
        invoice = parse_text_invoice(source, path.read_bytes())
    elif source.detected_type in ("image_invoice_pdf", "image"):
        if vision_candidate is None:
            return {"invoice": None, "issues": ["VISION_REQUIRED"]}
        require(isinstance(vision_candidate, dict)
                and set(LABELS) - OPTIONAL <= set(vision_candidate) <= set(LABELS)
                and all(type(value) is str for value in vision_candidate.values()),
                message="Invalid vision candidate fields")
        fields = {key: value.strip() for key, value in vision_candidate.items()}
        for name in OPTIONAL - {"order_ref"}:
            fields.setdefault(name, "")
        if not fields.get("order_ref"):
            fields.pop("order_ref", None)
        try:
            fields["amount_cents"] = decimal_cents(fields["amount_cents"].removeprefix("¥").removeprefix("￥"))
        except CoreError:
            raise conflict("Invalid decimal amount") from None
        invoice = build_invoice(source, fields, lambda key: ImageRegion(type="image_region", field=key),
                                "candidate", "vision")
    else:
        raise CoreError("UNSUPPORTED_FILE", "Source is not a supported invoice")
    issues = []
    if not invoice.trusted():
        issues.append("FACT_UNCONFIRMED")
    if "住宿" in invoice.project.value and invoice.service_period is None:
        issues.append("STAY_PERIOD_MISSING")
    return {"invoice": invoice.model_dump(), "issues": issues}


def same_business_values(stored, parsed):
    return all(getattr(stored, name).value == getattr(parsed, name).value for name in INVOICE_FIELDS) and (
        (stored.order_ref.value if stored.order_ref else None) == (parsed.order_ref.value if parsed.order_ref else None)
        and stored.service_period == parsed.service_period)
