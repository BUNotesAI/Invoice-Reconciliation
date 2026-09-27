"""Independent verifier: reread the workbook and disk, never cached totals."""
import hashlib
import re
import zipfile
from decimal import Decimal, InvalidOperation
from openpyxl import load_workbook

from .errors import CoreError, require
from .models import Snapshot, HistoryEntry
from .storage import inside
from .values import digest, strict_json, decimal_cents, sum_cents

CHECKS = ("row_completeness", "date_order", "unique_invoices", "table_disk", "filename_fields", "category_totals")


def actual_cents(cell):
    require(cell.data_type == "n" and type(cell.value) in (int, float), "VERIFY_FAILED", "Amount cell must be numeric", 3)
    try:
        amount = Decimal(str(cell.value)) * 100
        require(amount.is_finite() and amount == amount.to_integral_value() and amount >= 0,
                "VERIFY_FAILED", "Amount cell has fractional cents", 3)
        return int(amount)
    except InvalidOperation:
        raise CoreError("VERIFY_FAILED", "Invalid amount cell", 3) from None


def text_value(value):
    value = str(value)
    return "'" + value if value.startswith(("=", "+", "-", "@")) else value


def verify(store, policy, snapshot_hash, manifest_object_id, history_snapshot):
    require(re.fullmatch(r"[0-9a-f]{64}", snapshot_hash or "") and manifest_object_id == snapshot_hash,
            "SNAPSHOT_MISMATCH", "Invalid manifest binding", 3)
    directory = inside(store.batch / "staging" / manifest_object_id, store.root)
    failures = {key: [] for key in CHECKS}
    def check(condition, group, issue):
        if not condition:
            failures[group].append(issue)
    try:
        raw_snapshot = strict_json(inside(directory / "snapshot.json", store.root).read_bytes())
        require(digest(raw_snapshot) == snapshot_hash, "SNAPSHOT_MISMATCH", "Staged snapshot changed", 3)
        snapshot = Snapshot.model_validate(raw_snapshot)
        manifest = strict_json(inside(directory / "manifest.json", store.root).read_bytes())
        require(manifest["snapshot_hash"] == snapshot_hash and manifest["policy_hash"] == policy.sha()
                and snapshot.policy_hash == policy.sha() and manifest["history_hash"] == snapshot.history_hash,
                "SNAPSHOT_MISMATCH", "Manifest binding changed", 3)
        history = [HistoryEntry.model_validate(entry) for entry in history_snapshot]
        require(digest([entry.model_dump() for entry in sorted(history, key=lambda e: e.invoice_no)]) == snapshot.history_hash,
                "SNAPSHOT_MISMATCH", "History binding changed", 3)
        path = inside(directory / "交接清单.xlsx", store.root)
        with zipfile.ZipFile(path) as archive:
            require(sum(entry.file_size for entry in archive.infolist()) <= 32 * 1024 * 1024 and len(archive.infolist()) < 200,
                    "VERIFY_FAILED", "Workbook archive too large", 3)
        book = load_workbook(path, data_only=False, keep_links=False)
        require(book.sheetnames == ["汇总", "明细"], "VERIFY_FAILED", "Unexpected workbook sheets", 3)
        detail, summary = book["明细"], book["汇总"]
        ordered = sorted(snapshot.items, key=lambda item: (item.service_date, item.id))
        check(detail.max_row == len(ordered) + 2 and detail.max_column == 9, "row_completeness", "Unexpected detail extent")
        check([detail.cell(1, column).value for column in range(1, 10)] ==
              ["序号", "报销类型", "项目", "费用明细", "金额", "日期", "发票号", "公司全称", "备注"], "row_completeness", "Header mismatch")
        check(set(str(r) for r in detail.merged_cells.ranges) == {f"A{len(ordered)+2}:D{len(ordered)+2}"},
              "row_completeness", "Unexpected merged cells")
        seen, dates, attachments, calculated = [], [], set(), {}
        manifest_rows = manifest["rows"]
        check(len(manifest_rows) == len(ordered), "row_completeness", "Manifest row count mismatch")
        for index, item in enumerate(ordered, 2):
            invoice, rule = item.invoice, policy.categories[item.category]
            values = [detail.cell(index, column).value for column in range(1, 10)]
            amount = actual_cents(detail.cell(index, 5))
            check(amount == invoice.amount_cents.value, "row_completeness", "Amount differs from snapshot")
            expected_detail = item.expense_detail
            if item.replaces_invoice_no:
                expected_detail += f"；重开票，原票 {item.replaces_invoice_no} 已作废"
            expected = [index-1, text_value(rule.btype), text_value(rule.summary), text_value(expected_detail),
                        None, item.service_date, invoice.invoice_no.value, text_value(invoice.seller_name.value)]
            check(all(values[n] == expected[n] for n in range(8) if n != 4), "row_completeness", "Row differs from snapshot")
            check(all(detail.cell(index, n).data_type == "s" for n in (2, 3, 4, 6, 7, 8, 9)),
                  "row_completeness", "Untrusted text is not a text cell")
            check(detail.cell(index, 7).number_format == "@", "row_completeness", "Invoice number format changed")
            number, filename = values[6], values[8]
            check(isinstance(number, str) and bool(re.fullmatch(r"[0-9]{20}", number)), "unique_invoices", "Invalid invoice cell")
            seen.append(number)
            dates.append(values[5])
            check(isinstance(filename, str) and "/" not in filename and "\\" not in filename,
                  "table_disk", "Invalid attachment name")
            require(isinstance(filename, str) and "/" not in filename and "\\" not in filename,
                    "VERIFY_FAILED", "Unsafe attachment path", 3)
            attachments.add(filename)
            attachment = inside(directory / filename, store.root, must_exist=False)
            check(attachment.is_file(), "table_disk", "Missing attachment")
            if attachment.is_file():
                check(hashlib.sha256(attachment.read_bytes()).hexdigest() == invoice.invoice_no.source.file_sha256,
                      "table_disk", "Attachment content changed")
            match = re.fullmatch(r"([0-9]{20})_(.+)_([0-9]+(?:\.[0-9]{1,2})?)_([^_]+)\.pdf", filename)
            check(bool(match) and match[1] == number and match[2] == item.short_name
                  and decimal_cents(match[3]) == amount and match[4] == item.category,
                  "filename_fields", "Filename differs from cells")
            calculated[rule.summary] = calculated.get(rule.summary, 0) + amount
            if index - 2 < len(manifest_rows):
                row = manifest_rows[index - 2]
                original_hash = row.get("business_hash")
                check(original_hash == digest({key: value for key, value in row.items() if key != "business_hash"}),
                      "row_completeness", "Manifest row hash changed")
                check(row.get("item_id") == item.id and row.get("invoice_no") == number
                      and row.get("amount_cents") == amount and row.get("service_date") == values[5]
                      and row.get("relative_name") == filename and row.get("seller_name") == invoice.seller_name.value
                      and row.get("expense_detail") == expected_detail and row.get("btype") == rule.btype
                      and row.get("summary") == rule.summary and row.get("attachment_hash") == invoice.invoice_no.source.file_sha256,
                      "row_completeness", "Manifest business fields differ")
        check(dates == sorted(dates), "date_order", "Dates are not sorted")
        check(len(seen) == len(set(seen)) and not set(seen) & {entry.invoice_no for entry in history},
              "unique_invoices", "Duplicate invoice number")
        disk = {entry.name for entry in directory.iterdir()}
        check(disk == attachments | {"交接清单.xlsx", "manifest.json", "snapshot.json"}, "table_disk", "Disk inventory differs")
        check(len(manifest["files"]) == len(attachments) + 1 and
              {entry["relative_name"] for entry in manifest["files"]} == attachments | {"交接清单.xlsx"},
              "table_disk", "Manifest inventory differs")
        for entry in manifest["files"]:
            name = entry["relative_name"]
            require(isinstance(name, str) and "/" not in name and "\\" not in name,
                    "VERIFY_FAILED", "Unsafe manifest path", 3)
            file = inside(directory / name, store.root, must_exist=False)
            check(file.is_file() and file.stat().st_size == entry["bytes"]
                  and hashlib.sha256(file.read_bytes()).hexdigest() == entry["sha256"], "table_disk", "Artifact hash differs")
        check(summary.max_row <= max(4, len(calculated)+2) and summary.max_column == 4,
              "category_totals", "Summary extent differs")
        check(summary["D2"].value == text_value(snapshot.applicant), "row_completeness", "Applicant differs")
        for index, (category, amount) in enumerate(sorted(calculated.items()), 2):
            check(summary.cell(index, 1).value == text_value(category) and actual_cents(summary.cell(index, 2)) == amount,
                  "category_totals", "Category total differs")
        total = sum_cents(calculated.values())
        check(detail.cell(len(ordered)+2, 1).value == "合计" and actual_cents(detail.cell(len(ordered)+2, 5)) == total,
              "category_totals", "Detail total differs")
        check(summary.cell(len(calculated)+2, 1).value == "合计" and actual_cents(summary.cell(len(calculated)+2, 2)) == total
              and manifest["total_cents"] == total and manifest["category_totals"] == calculated,
              "category_totals", "Summary or manifest total differs")
        check(not any(cell.data_type == "f" for sheet in book for row in sheet for cell in row),
              "row_completeness", "Unexpected formula")
        book.close()
    except (CoreError, ValueError, TypeError, KeyError, IndexError, OSError, zipfile.BadZipFile):
        failures["row_completeness"].append("Unreadable or inconsistent artifacts")
    checks = [{"name": name, "passed": not failures[name], "issues": failures[name]} for name in CHECKS]
    return {"checks": checks, "passed": all(check["passed"] for check in checks),
            "issues": [issue for values in failures.values() for issue in values]}
