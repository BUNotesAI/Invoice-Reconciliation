"""Build reproducible staging packages from a validated immutable snapshot."""
import hashlib
import io
import zipfile
from collections import defaultdict
from datetime import datetime
from decimal import Decimal
from pathlib import Path

from openpyxl import Workbook
from openpyxl.styles import Alignment, Border, Font, PatternFill, Side

from .errors import CoreError, require
from .history import validated_history, replacement_candidates
from .models import Snapshot, HistoryEntry
from .storage import atomic_write, inside
from .values import canonical, digest, amount_text, safe_name, sum_cents, strict_json

HEADERS = ["序号", "报销类型", "项目", "费用明细", "金额", "日期", "发票号", "公司全称", "备注"]
FIXED_TIME = datetime(2026, 1, 1)


def spreadsheet_text(value):
    value = str(value)
    return "'" + value if value.startswith(("=", "+", "-", "@")) else value


def normalize_xlsx(data):
    output = io.BytesIO()
    with zipfile.ZipFile(io.BytesIO(data)) as source, zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as target:
        for name in sorted(source.namelist()):
            content = source.read(name)
            if name == "docProps/core.xml":
                import re
                content = re.sub(rb"(<dcterms:modified[^>]*>)[^<]+", rb"\g<1>2026-01-01T00:00:00Z", content)
            info = zipfile.ZipInfo(name, (2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o600 << 16
            target.writestr(info, content)
    return output.getvalue()


def workbook(rows, totals, applicant):
    book = Workbook()
    summary = book.active
    summary.title = "汇总"
    summary.append(["报销类型", "金额"])
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
        detail.cell(row, 5).number_format = '#,##0.00'
    for row in range(2, len(totals) + 3):
        summary.cell(row, 2).number_format = '#,##0.00'
    book.properties.created = FIXED_TIME
    book.properties.modified = FIXED_TIME
    book.properties.creator = "Invoice Reconciliation"
    stream = io.BytesIO()
    book.save(stream)
    return normalize_xlsx(stream.getvalue())


def validate_snapshot(raw, expected, policy, store):
    snapshot = Snapshot.model_validate(raw)
    require(digest(raw) == expected, "SNAPSHOT_MISMATCH", "Snapshot hash mismatch", 3)
    require(snapshot.policy_hash == policy.sha(), "SNAPSHOT_MISMATCH", "Policy changed", 3)
    history_path = inside(store.batch / "history" / (snapshot.history_hash + ".json"), store.root)
    history_raw = strict_json(history_path.read_bytes())
    require(digest(history_raw) == snapshot.history_hash, "SNAPSHOT_MISMATCH", "History changed", 3)
    history = [HistoryEntry.model_validate(raw) for raw in validated_history(history_raw)["validated_snapshot"]]
    seen, occupied_replacements = set(), set()
    decisions = {}
    for decision in snapshot.decisions:
        required = {"id", "item_id", "kind", "payload", "actor", "at", "expected_revision", "source_event_id"}
        require(set(decision) == required and type(decision["expected_revision"]) is int
                and decision["expected_revision"] <= snapshot.revision
                and bool(decision["actor"]) and bool(decision["source_event_id"]), message="Invalid decision record")
        require(decision["id"] not in decisions, message="Duplicate decision identifier")
        require(decision["kind"] in {"confirm_visual", "choose_evidence", "explain_over_limit", "replace_unpaid_invoice",
                                     "receipt_only", "reject", "manual_evidence"}, message="Unknown decision kind")
        decisions[decision["id"]] = decision
    for item in snapshot.items:
        invoice = item.invoice
        require(invoice.trusted(), "FACT_UNCONFIRMED", "Unconfirmed facts cannot be packaged", 3)
        require(invoice.buyer_name.value == policy.company.name and invoice.buyer_tax_id.value == policy.company.tax_id,
                "WRONG_BUYER", "Buyer does not match policy", 3)
        require(invoice.invoice_no.value not in seen and all(entry.invoice_no != invoice.invoice_no.value for entry in history),
                "DUPLICATE_INVOICE", "Invoice already present", 3)
        seen.add(invoice.invoice_no.value)
        require(item.category in policy.categories, "INVALID_POLICY", "Unknown category")
        require(all(key in decisions and decisions[key]["item_id"] == item.id for key in item.decision_ids),
                message="Decision is not bound to item")
        applicable = [decisions[key] for key in item.decision_ids]
        replacement = replacement_candidates(invoice, history, item.service_date)
        if replacement:
            require(len(replacement) == 1, "HISTORY_UNKNOWN", "Ambiguous replacement history", 3)
            previous = replacement[0]
            require(previous["reason"] == "REPLACEMENT_REQUIRES_DECISION", previous["reason"], "Replacement cannot add reimbursement", 3)
            require(item.replaces_invoice_no == previous["invoice_no"] and item.service_date == previous["service_date"]
                    and any(d["kind"] == "replace_unpaid_invoice" and d["payload"] == {"invoice_no": previous["invoice_no"]} for d in applicable),
                    "HISTORY_UNKNOWN", "Replacement decision required", 3)
            require(previous["invoice_no"] not in occupied_replacements, "LINK_CONFLICT", "History expense already occupied", 3)
            occupied_replacements.add(previous["invoice_no"])
        else:
            require(item.replaces_invoice_no is None, "HISTORY_UNKNOWN", "Replacement history not found", 3)
        for name in ("invoice_no", "amount_cents", "amount_upper", "buyer_name", "buyer_tax_id", "seller_name", "issue_date", "project", "remark"):
            fact = getattr(invoice, name)
            if fact.level == "confirmed":
                require(any(d["kind"] == "confirm_visual" and d["source_event_id"] == fact.confirmation_event
                            and d["actor"] == fact.confirmed_by for d in applicable),
                        "FACT_UNCONFIRMED", "Visual confirmation decision required", 3)
        if item.category == "住宿":
            require(invoice.service_period is not None, "EVIDENCE_MISSING", "Stay duration required", 3)
            over = invoice.amount_cents.value > invoice.service_period.nights * policy.limits.hotel_per_night_cents
            require(not over or any(d["kind"] == "explain_over_limit" and isinstance(d["payload"], dict)
                                   and isinstance(d["payload"].get("explanation"), str) and d["payload"]["explanation"].strip() for d in applicable),
                    "EVIDENCE_MISSING", "Over-limit explanation required", 3)
        source, _ = store.source(invoice.source_file_id)
        require(source.detected_type in ("invoice_pdf", "image_invoice"), "UNSUPPORTED_FILE", "Package requires PDF invoices")
        require(all(getattr(invoice, field).source.file_sha256 == source.sha256 for field in
                    ("invoice_no", "amount_cents", "amount_upper", "buyer_name", "buyer_tax_id", "seller_name", "issue_date", "project", "remark")),
                "SNAPSHOT_MISMATCH", "Fact source differs from attachment", 3)
    sum_cents(item.invoice.amount_cents.value for item in snapshot.items)
    return snapshot


def package(store, policy, confirmed_snapshot, expected_snapshot_hash):
    snapshot = validate_snapshot(confirmed_snapshot, expected_snapshot_hash, policy, store)
    staging = inside(store.batch / "staging" / expected_snapshot_hash, store.root, must_exist=False)
    staging.mkdir(parents=True, exist_ok=True)
    rows, files = [], []
    totals = defaultdict(int)
    for item in sorted(snapshot.items, key=lambda row: (row.service_date, row.id)):
        invoice = item.invoice
        name = f"{invoice.invoice_no.value}_{item.short_name}_{amount_text(invoice.amount_cents.value)}_{item.category}.pdf"
        safe_name(name, 240)
        source, path = store.source(invoice.source_file_id)
        content = path.read_bytes()
        target = inside(staging / name, store.root, must_exist=False)
        if target.exists():
            require(target.read_bytes() == content, "SNAPSHOT_MISMATCH", "Existing staged attachment differs", 3)
        else:
            atomic_write(target, content)
        category = policy.categories[item.category]
        detail = item.expense_detail
        if item.replaces_invoice_no:
            detail += f"；重开票，原票 {item.replaces_invoice_no} 已作废"
        row = dict(item_id=item.id, invoice_no=invoice.invoice_no.value, amount_cents=invoice.amount_cents.value,
                   service_date=item.service_date, btype=category.btype, summary=category.summary,
                   expense_detail=detail, seller_name=invoice.seller_name.value, relative_name=name,
                   attachment_hash=source.sha256)
        row["business_hash"] = digest(row)
        rows.append(row)
        files.append(dict(relative_name=name, sha256=source.sha256, bytes=len(content)))
        totals[category.summary] += invoice.amount_cents.value
    content = workbook(rows, dict(totals), snapshot.applicant)
    files.append(dict(relative_name="交接清单.xlsx", sha256=hashlib.sha256(content).hexdigest(), bytes=len(content)))
    manifest = dict(schema_version=1, batch_id=snapshot.batch_id, revision=snapshot.revision,
                    snapshot_hash=expected_snapshot_hash, policy_hash=snapshot.policy_hash, history_hash=snapshot.history_hash,
                    files=files, rows=rows, total_cents=sum_cents(r["amount_cents"] for r in rows), category_totals=dict(totals))
    expected_files = {file["relative_name"] for file in files} | {"manifest.json", "snapshot.json"}
    require(all(path.name in expected_files for path in staging.iterdir()), "SNAPSHOT_MISMATCH", "Unexpected staged file", 3)
    for name, data in (("交接清单.xlsx", content), ("snapshot.json", canonical(confirmed_snapshot)), ("manifest.json", canonical(manifest))):
        path = inside(staging / name, store.root, must_exist=False)
        if path.exists():
            require(path.read_bytes() == data, "SNAPSHOT_MISMATCH", "Existing staged artifact differs", 3)
        else:
            atomic_write(path, data)
    return {"staging_manifest": manifest, "manifest_object_id": expected_snapshot_hash}
