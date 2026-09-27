"""Independent verifier: reread the workbook and disk, never cached totals or package helpers."""
import hashlib
import re
import zipfile
from decimal import Decimal, InvalidOperation

from openpyxl import load_workbook

from .errors import CoreError, require
from .history import validated_history
from .models import Manifest, Snapshot
from .storage import inside
from .values import amount_text, decimal_cents, digest, strict_json, sum_cents

CHECKS = ("row_completeness", "date_order", "unique_invoices", "table_disk", "filename_fields", "category_totals")
HEADERS = ["序号", "报销类型", "项目", "费用明细", "金额", "日期", "发票号", "公司全称", "备注"]
RECORDS = {"manifest.json", "snapshot.json"}


class Failure(Exception):
    pass


def actual_cents(cell):
    if cell.data_type != "n" or type(cell.value) not in (int, float):
        raise Failure("Amount cell must be numeric")
    try:
        amount = Decimal(str(cell.value)) * 100
    except InvalidOperation:
        raise Failure("Invalid amount cell") from None
    if not amount.is_finite() or amount != amount.to_integral_value() or amount < 0:
        raise Failure("Amount cell has fractional cents")
    return int(amount)


def neutral(value):
    value = str(value)
    return "'" + value if value.startswith(("=", "+", "-", "@")) else value


def verify(store, policy, snapshot_hash, manifest_object_id, history_snapshot):
    require(isinstance(snapshot_hash, str) and re.fullmatch(r"[0-9a-f]{64}", snapshot_hash)
            and manifest_object_id == snapshot_hash, "SNAPSHOT_MISMATCH", "Invalid manifest binding", 3)
    directory = inside(store.batch / "staging" / manifest_object_id, store.root)
    failures = {key: [] for key in CHECKS}

    def check(condition, group, issue):
        if not condition:
            failures[group].append(issue)

    try:
        raw_snapshot = strict_json(inside(directory / "snapshot.json", store.root).read_bytes())
        if digest(raw_snapshot) != snapshot_hash:
            raise Failure("Staged snapshot changed")
        snapshot = Snapshot.model_validate(raw_snapshot)
        manifest = Manifest.model_validate(strict_json(inside(directory / "manifest.json", store.root).read_bytes()))
        if not (manifest.snapshot_hash == snapshot_hash and manifest.policy_hash == policy.sha() == snapshot.policy_hash
                and manifest.history_hash == snapshot.history_hash and manifest.batch_id == snapshot.batch_id
                and manifest.revision == snapshot.revision):
            raise Failure("Manifest binding changed")
        history = validated_history(history_snapshot)
        if history["history_hash"] != snapshot.history_hash:
            raise Failure("History binding changed")
        history_numbers = {entry["invoice_no"] for entry in history["validated_snapshot"]}

        ordered = sorted(snapshot.items, key=lambda item: (item.service_date, item.id))
        expected_total = sum_cents(item.invoice.amount_cents.value for item in ordered)
        ledger = policy.naming.ledger.replace("{applicant}", snapshot.applicant).replace("{total}", amount_text(expected_total))
        check(manifest.ledger_name == ledger, "table_disk", "Ledger name differs from policy")
        path = inside(directory / manifest.ledger_name, store.root)
        with zipfile.ZipFile(path) as archive:
            entries = archive.infolist()
            if len(entries) >= 200 or sum(entry.file_size for entry in entries) > 32 * 1024 * 1024:
                raise Failure("Workbook archive too large")
        book = load_workbook(path, data_only=False, keep_links=False)
        try:
            if book.sheetnames != ["汇总", "明细"]:
                raise Failure("Unexpected workbook sheets")
            detail, summary = book["明细"], book["汇总"]
            end = len(ordered) + 2
            check(detail.max_row == end and detail.max_column == 9, "row_completeness", "Unexpected detail extent")
            check([detail.cell(1, column).value for column in range(1, 10)] == HEADERS, "row_completeness", "Header mismatch")
            check({str(merged) for merged in detail.merged_cells.ranges} == {f"A{end}:D{end}"},
                  "row_completeness", "Unexpected merged cells")
            check(len(manifest.rows) == len(ordered), "row_completeness", "Manifest row count mismatch")
            numbers, dates, attachments, calculated = [], [], set(), {}
            for index, item in enumerate(ordered, 2):
                invoice, rule = item.invoice, policy.categories[item.category]
                values = [detail.cell(index, column).value for column in range(1, 10)]
                amount = actual_cents(detail.cell(index, 5))
                check(amount == invoice.amount_cents.value, "row_completeness", "Amount differs from snapshot")
                detail_text = item.expense_detail
                if item.replaces_invoice_no:
                    detail_text += f"；重开票，原票 {item.replaces_invoice_no} 已作废"
                expected = [index - 1, neutral(rule.btype), neutral(rule.summary), neutral(detail_text), None,
                            item.service_date, invoice.invoice_no.value, neutral(invoice.seller_name.value)]
                check(all(values[n] == expected[n] for n in range(8) if n != 4), "row_completeness", "Row differs from snapshot")
                check(all(detail.cell(index, n).data_type == "s" for n in (2, 3, 4, 6, 7, 8, 9)),
                      "row_completeness", "Untrusted text is not a text cell")
                check(detail.cell(index, 7).number_format == "@", "row_completeness", "Invoice number format changed")
                number, filename = values[6], values[8]
                check(isinstance(number, str) and re.fullmatch(r"[0-9]{20}", number) is not None,
                      "unique_invoices", "Invalid invoice cell")
                numbers.append(number)
                dates.append(values[5])
                if not isinstance(filename, str) or "/" in filename or "\\" in filename or filename in RECORDS or filename.startswith("."):
                    raise Failure("Unsafe attachment name")
                attachments.add(filename)
                attachment = inside(directory / filename, store.root, must_exist=False)
                check(attachment.is_file() and hashlib.sha256(attachment.read_bytes()).hexdigest() == invoice.source_file_id,
                      "table_disk", "Attachment missing or changed")
                match = re.fullmatch(r"([0-9]{20})_(.+)_([0-9]+(?:\.[0-9]{1,2})?)_([^_]+)\.pdf", filename)
                check(match is not None and match[1] == number == invoice.invoice_no.value and match[2] == item.short_name
                      and decimal_cents(match[3]) == amount and match[4] == item.category
                      and match[3] == amount_text(amount), "filename_fields", "Filename differs from cells")
                calculated[rule.summary] = sum_cents([calculated.get(rule.summary, 0), amount])
                row = manifest.rows[index - 2] if index - 2 < len(manifest.rows) else None
                if row is None:
                    continue
                fields = row.model_dump()
                check(row.business_hash == digest({key: value for key, value in fields.items() if key != "business_hash"}),
                      "row_completeness", "Manifest row hash changed")
                check((row.item_id, row.invoice_no, row.amount_cents, row.service_date, row.relative_name, row.seller_name,
                       row.expense_detail, row.btype, row.summary, row.attachment_hash) ==
                      (item.id, number, amount, values[5], filename, invoice.seller_name.value, detail_text, rule.btype,
                       rule.summary, invoice.source_file_id), "row_completeness", "Manifest business fields differ")
            check(dates == sorted(dates), "date_order", "Dates are not sorted")
            check(len(numbers) == len(set(numbers)), "unique_invoices", "Duplicate invoice number in batch")
            check(not set(numbers) & history_numbers, "unique_invoices", "Invoice number already in history")
            disk = {entry.name for entry in directory.iterdir()}
            check(disk == attachments | {manifest.ledger_name} | RECORDS, "table_disk", "Disk inventory differs")
            check(len(manifest.files) == len(attachments) + 1 and
                  {entry.relative_name for entry in manifest.files} == attachments | {manifest.ledger_name},
                  "table_disk", "Manifest inventory differs")
            for entry in manifest.files:
                file = inside(directory / entry.relative_name, store.root, must_exist=False)
                check(file.is_file() and file.stat().st_size == entry.bytes
                      and hashlib.sha256(file.read_bytes()).hexdigest() == entry.sha256, "table_disk", "Artifact hash differs")
            total = sum_cents(calculated.values())
            check(summary.max_row <= max(4, len(calculated) + 2) and summary.max_column == 4,
                  "category_totals", "Summary extent differs")
            check(summary["D2"].value == neutral(snapshot.applicant), "row_completeness", "Applicant differs")
            check([summary.cell(1, 1).value, summary.cell(1, 2).value] == ["报销类型", "金额"],
                  "category_totals", "Summary header differs")
            for index, (category, amount) in enumerate(sorted(calculated.items()), 2):
                check(summary.cell(index, 1).value == neutral(category) and actual_cents(summary.cell(index, 2)) == amount,
                      "category_totals", "Category total differs")
            check(detail.cell(end, 1).value == "合计" and actual_cents(detail.cell(end, 5)) == total,
                  "category_totals", "Detail total differs")
            check(summary.cell(len(calculated) + 2, 1).value == "合计"
                  and actual_cents(summary.cell(len(calculated) + 2, 2)) == total,
                  "category_totals", "Summary total differs")
            check(manifest.total_cents == total == expected_total and manifest.category_totals == calculated,
                  "category_totals", "Manifest totals differ")
            check(not any(cell.data_type == "f" for sheet in book for row in sheet.iter_rows() for cell in row),
                  "row_completeness", "Unexpected formula")
        finally:
            book.close()
    except Failure as failure:
        failures["row_completeness"].append(str(failure))
    except (CoreError, ValueError, TypeError, KeyError, IndexError, OSError, zipfile.BadZipFile):
        # pydantic ValidationError is a ValueError; any unreadable artifact fails closed.
        failures["row_completeness"].append("Unreadable or inconsistent artifacts")
    checks = [{"name": name, "passed": not failures[name], "issues": failures[name]} for name in CHECKS]
    return {"checks": checks, "passed": all(item["passed"] for item in checks),
            "issues": [issue for values in failures.values() for issue in values]}
