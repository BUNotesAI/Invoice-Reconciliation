"""Build reproducible staging packages from a validated immutable snapshot."""
import hashlib
import io
import re
import zipfile
from collections import defaultdict
from datetime import datetime
from decimal import Decimal

from openpyxl import Workbook
from openpyxl.styles import Alignment, Border, Font, PatternFill, Side

from .errors import CoreError, require
from .extract import parse_text_invoice, same_business_values
from .history import replacement_candidates, validated_history
from .models import HistoryEntry, Snapshot
from .storage import atomic_write, inside
from .values import amount_text, canonical, digest, safe_name, strict_json, sum_cents

HEADERS = ["序号", "报销类型", "项目", "费用明细", "金额", "日期", "发票号", "公司全称", "备注"]
SUMMARY_HEADERS = ["报销类型", "金额"]
FIXED_TIME = datetime(2026, 1, 1)
STAGED_RECORDS = ("manifest.json", "snapshot.json")


def spreadsheet_text(value):
    """Text written by users or agents is neutralised so no spreadsheet treats it as a formula."""
    value = str(value)
    return "'" + value if value.startswith(("=", "+", "-", "@")) else value


def replacement_note(invoice_no):
    return f"重开票，原票 {invoice_no} 已作废"


def expense_detail(item):
    return item.expense_detail + ("；" + replacement_note(item.replaces_invoice_no) if item.replaces_invoice_no else "")


def attachment_name(item):
    invoice = item.invoice
    return safe_name(f"{invoice.invoice_no.value}_{item.short_name}_{amount_text(invoice.amount_cents.value)}_{item.category}.pdf", 240)


def ledger_name(policy, applicant, total_cents):
    return safe_name(policy.naming.ledger.replace("{applicant}", applicant).replace("{total}", amount_text(total_cents)), 240)


def normalize_xlsx(data):
    output = io.BytesIO()
    with zipfile.ZipFile(io.BytesIO(data)) as source, zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as target:
        for name in sorted(source.namelist()):
            content = source.read(name)
            if name == "docProps/core.xml":
                content = re.sub(rb"(<dcterms:(?:created|modified)[^>]*>)[^<]+", rb"\g<1>2026-01-01T00:00:00Z", content)
            info = zipfile.ZipInfo(name, (2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o600 << 16
            target.writestr(info, content)
    return output.getvalue()


def workbook(rows, totals, applicant):
    book = Workbook()
    summary = book.active
    summary.title = "汇总"
    summary.append(SUMMARY_HEADERS)
    for category, value in sorted(totals.items()):
        summary.append([spreadsheet_text(category), Decimal(value) / 100])
    summary.append(["合计", Decimal(sum_cents(totals.values())) / 100])
    summary["D1"] = "申请人"
    summary["D2"] = spreadsheet_text(applicant)
    summary["D4"] = "金额按确认快照生成；修改后须重新终审。"
    detail = book.create_sheet("明细")
    detail.append(HEADERS)
    for index, row in enumerate(rows, 1):
        values = [index, row["btype"], row["summary"], row["expense_detail"], Decimal(row["amount_cents"]) / 100,
                  row["service_date"], row["invoice_no"], row["seller_name"], row["relative_name"]]
        for column, value in enumerate(values, 1):
            cell = detail.cell(index + 1, column, spreadsheet_text(value) if isinstance(value, str) else value)
            if isinstance(value, str):
                cell.data_type = "s"
            if column == 7:
                cell.number_format = "@"
    end = len(rows) + 2
    detail.cell(end, 1, "合计")
    detail.merge_cells(start_row=end, start_column=1, end_row=end, end_column=4)
    detail.cell(end, 5, Decimal(sum_cents(row["amount_cents"] for row in rows)) / 100)
    border = Border(*(Side(style="thin", color="D3DED9"),) * 4)
    for sheet in book:
        sheet.freeze_panes = "A2"
        for cells in sheet:
            for cell in cells:
                cell.font = Font(name="Arial", size=11, bold=cell.row == 1)
                cell.alignment = Alignment(vertical="center", wrap_text=True)
                cell.border = border
                if cell.row == 1:
                    cell.fill = PatternFill("solid", fgColor="D9EAE2")
        sheet.row_dimensions[1].height = 26
    for column, width in {"A": 8, "B": 16, "C": 22, "D": 55, "E": 15, "F": 16, "G": 26, "H": 36, "I": 65}.items():
        detail.column_dimensions[column].width = width
    summary.column_dimensions["A"].width = 24
    summary.column_dimensions["B"].width = 18
    summary.column_dimensions["D"].width = 58
    for row in range(2, end + 1):
        detail.cell(row, 5).number_format = "#,##0.00"
    for row in range(2, len(totals) + 3):
        summary.cell(row, 2).number_format = "#,##0.00"
    book.properties.created = FIXED_TIME
    book.properties.modified = FIXED_TIME
    book.properties.creator = "Invoice Reconciliation"
    stream = io.BytesIO()
    book.save(stream)
    return normalize_xlsx(stream.getvalue())


def load_history(store, history_hash):
    path = inside(store.batch / "history" / (history_hash + ".json"), store.root)
    raw = strict_json(path.read_bytes())
    checked = validated_history(raw)
    require(checked["history_hash"] == history_hash and raw == checked["validated_snapshot"],
            "SNAPSHOT_MISMATCH", "History changed", 3)
    return [HistoryEntry.model_validate(entry) for entry in checked["validated_snapshot"]]


def check_facts(store, item, decisions):
    """Snapshot facts must still match the attachment: re-parse text layers, and demand bound user confirmations."""
    invoice = item.invoice
    source, path = store.source(invoice.source_file_id)
    if source.detected_type == "invoice_pdf":
        require(all(fact.level == "extracted" for _, fact in invoice.facts()),
                "SNAPSHOT_MISMATCH", "Text invoice facts must be extracted", 3)
        try:
            parsed = parse_text_invoice(source, path.read_bytes())
        except CoreError:
            raise CoreError("SNAPSHOT_MISMATCH", "Attachment no longer parses", 3) from None
        require(same_business_values(invoice, parsed), "SNAPSHOT_MISMATCH", "Snapshot facts differ from attachment", 3)
    elif source.detected_type in ("image_invoice_pdf", "image"):
        require(source.detected_type == "image_invoice_pdf", "UNSUPPORTED_FILE", "Package requires PDF invoices", 3)
        require(invoice.trusted(), "FACT_UNCONFIRMED", "Unconfirmed facts cannot be packaged", 3)
        confirmations = [decision for decision in decisions if decision.kind == "confirm_visual"]
        for _, fact in invoice.facts():
            require(fact.level == "confirmed" and any(
                decision.source_event_id == fact.confirmation_event and decision.actor == fact.confirmed_by
                and fact.id in decision.typed().fact_ids for decision in confirmations),
                "FACT_UNCONFIRMED", "Visual confirmation decision required", 3)
    else:
        raise CoreError("UNSUPPORTED_FILE", "Package requires PDF invoices", 3)
    return source, path


def validate_snapshot(raw, expected, policy, store):
    require(isinstance(raw, dict) and isinstance(expected, str) and digest(raw) == expected,
            "SNAPSHOT_MISMATCH", "Snapshot hash mismatch", 3)
    snapshot = Snapshot.model_validate(raw)
    require(snapshot.policy_hash == policy.sha(), "SNAPSHOT_MISMATCH", "Policy changed", 3)
    history = load_history(store, snapshot.history_hash)
    decisions = {decision.id: decision for decision in snapshot.decisions}
    history_numbers = {entry.invoice_no for entry in history}
    seen, replaced, attachments, sources = set(), set(), set(), {}
    for item in snapshot.items:
        invoice = item.invoice
        applicable = [decisions[key] for key in item.decision_ids]
        kinds = {decision.kind for decision in applicable}
        # Rejected and receipt-only items never become ledger rows; their presence means a malformed snapshot.
        require(not kinds & {"reject", "receipt_only"}, message="Item is not a new reimbursement")
        require(invoice.buyer_name.value == policy.company.name and invoice.buyer_tax_id.value == policy.company.tax_id,
                "WRONG_BUYER", "Buyer does not match policy", 3)
        require(invoice.invoice_no.value not in seen and invoice.invoice_no.value not in history_numbers,
                "DUPLICATE_INVOICE", "Invoice already present", 3)
        seen.add(invoice.invoice_no.value)
        require(item.category in policy.categories, "INVALID_POLICY", "Unknown category")
        require(invoice.source_file_id not in attachments, "LINK_CONFLICT", "Attachment used by two items", 3)
        attachments.add(invoice.source_file_id)
        sources[item.id] = check_facts(store, item, applicable)
        candidates = replacement_candidates(invoice, history, item.service_date)
        if candidates:
            require(len(candidates) == 1, "HISTORY_UNKNOWN", "Ambiguous replacement history", 3)
            previous = candidates[0]
            require(previous["reason"] == "REPLACEMENT_REQUIRES_DECISION", previous["reason"],
                    "Replacement cannot add reimbursement", 3)
            require(item.replaces_invoice_no == previous["invoice_no"] and item.service_date == previous["service_date"]
                    and invoice.amount_cents.value == previous["amount_cents"]
                    and any(d.kind == "replace_unpaid_invoice" and d.typed().invoice_no == previous["invoice_no"] for d in applicable),
                    "HISTORY_UNKNOWN", "Replacement decision required", 3)
            require(previous["invoice_no"] not in replaced, "LINK_CONFLICT", "History expense already occupied", 3)
            replaced.add(previous["invoice_no"])
        else:
            require(item.replaces_invoice_no is None, "HISTORY_UNKNOWN", "Replacement history not found", 3)
        if item.category == "住宿":
            require(invoice.service_period is not None, "EVIDENCE_MISSING", "Stay duration required", 3)
            require(item.service_date == invoice.service_period.check_in, "FIELD_CONFLICT", "Stay date differs from check-in", 3)
            over = invoice.amount_cents.value > invoice.service_period.nights * policy.limits.hotel_per_night_cents
            require(not over or "explain_over_limit" in kinds, "EVIDENCE_MISSING", "Over-limit explanation required", 3)
    sum_cents(item.invoice.amount_cents.value for item in snapshot.items)
    return snapshot, sources


def package(store, policy, confirmed_snapshot, expected_snapshot_hash):
    snapshot, sources = validate_snapshot(confirmed_snapshot, expected_snapshot_hash, policy, store)
    staging = inside(store.batch / "staging" / expected_snapshot_hash, store.root, must_exist=False)
    rows, files, planned = [], [], {}
    totals = defaultdict(int)
    for item in sorted(snapshot.items, key=lambda row: (row.service_date, row.id)):
        invoice = item.invoice
        name = attachment_name(item)
        require(name not in planned, "LINK_CONFLICT", "Two items share an attachment name", 3)
        source, path = sources[item.id]
        planned[name] = path.read_bytes()
        category = policy.categories[item.category]
        row = dict(item_id=item.id, invoice_no=invoice.invoice_no.value, amount_cents=invoice.amount_cents.value,
                   service_date=item.service_date, btype=category.btype, summary=category.summary,
                   expense_detail=expense_detail(item), seller_name=invoice.seller_name.value, relative_name=name,
                   attachment_hash=source.sha256)
        row["business_hash"] = digest(row)
        rows.append(row)
        files.append(dict(relative_name=name, sha256=source.sha256, bytes=len(planned[name])))
        totals[category.summary] += invoice.amount_cents.value
    total = sum_cents(row["amount_cents"] for row in rows)
    ledger = ledger_name(policy, snapshot.applicant, total)
    require(ledger not in planned and ledger not in STAGED_RECORDS, "INVALID_POLICY", "Ledger name collides with an attachment")
    planned[ledger] = workbook(rows, dict(totals), snapshot.applicant)
    files.append(dict(relative_name=ledger, sha256=hashlib.sha256(planned[ledger]).hexdigest(), bytes=len(planned[ledger])))
    manifest = dict(schema_version=1, batch_id=snapshot.batch_id, revision=snapshot.revision,
                    snapshot_hash=expected_snapshot_hash, policy_hash=snapshot.policy_hash, history_hash=snapshot.history_hash,
                    ledger_name=ledger, files=files, rows=rows, total_cents=total, category_totals=dict(sorted(totals.items())))
    planned["snapshot.json"] = canonical(confirmed_snapshot)
    planned["manifest.json"] = canonical(manifest)
    # Idempotent: an earlier run of the same snapshot must have produced exactly these bytes.
    if staging.exists():
        present = {entry.name for entry in staging.iterdir()}
        require(present <= set(planned), "SNAPSHOT_MISMATCH", "Unexpected staged file", 3)
    for name, data in planned.items():
        path = inside(staging / name, store.root, must_exist=False)
        if path.exists():
            require(path.is_file() and path.read_bytes() == data, "SNAPSHOT_MISMATCH", "Existing staged artifact differs", 3)
        else:
            atomic_write(path, data)
    return {"staging_manifest": manifest, "manifest_object_id": expected_snapshot_hash}
