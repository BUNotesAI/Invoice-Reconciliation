"""Input hardening (design §5.8) and the CLI envelope contract, through the real subprocess."""
import json
import os
import struct
import zlib

import pytest

from conftest import FIXTURES


def png_header(width, height):
    """A PNG whose header claims the given size; the pixel limit must fire before any decoding."""
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(b"\x00" * 16)) + chunk(b"IEND", b""))


def ingest_fails(runtime, path, code, exit_code=2, name="x"):
    return runtime.fails("ingest", {"source_path": str(path), "original_name": name}, code, exit_code)


def test_oversized_file_is_refused(runtime):
    path = runtime.uploads / "big.pdf"
    with path.open("wb") as handle:
        handle.write(b"%PDF-1.4\n")
        handle.truncate(20 * 1024 * 1024 + 1)
    ingest_fails(runtime, path, "INPUT_TOO_LARGE")


def test_file_at_size_limit_is_read(runtime):
    path = runtime.uploads / "limit.bin"
    with path.open("wb") as handle:
        handle.truncate(20 * 1024 * 1024)
    assert runtime.ingest(path)["detected_type"] == "unsupported"


def test_image_pixel_limit(runtime):
    path = runtime.uploads / "huge.png"
    path.write_bytes(png_header(8000, 5001))
    ingest_fails(runtime, path, "PARSE_LIMIT")


def test_small_broken_png_is_unsupported_not_crash(runtime):
    path = runtime.uploads / "broken.png"
    path.write_bytes(b"\x89PNG\r\n\x1a\n" + b"junk")
    ingest_fails(runtime, path, "UNSUPPORTED_FILE")


def test_pdf_page_limit(runtime):
    ingest_fails(runtime, runtime.upload(FIXTURES / "edge" / "pages21.pdf"), "PARSE_LIMIT")


def test_workbook_zip_bomb_is_refused(runtime):
    import zipfile
    path = runtime.uploads / "bomb.xlsx"
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("xl/worksheets/sheet1.xml", b"\x00" * (40 * 1024 * 1024))
    ingest_fails(runtime, path, "PARSE_LIMIT")


def test_source_outside_runtime_root_is_refused(runtime, tmp_path):
    outside = tmp_path / "outside.pdf"
    outside.write_bytes((FIXTURES / "demo" / "F01.pdf").read_bytes())
    ingest_fails(runtime, outside, "INVALID_PATH")


def test_parent_segments_are_refused(runtime):
    runtime.upload(FIXTURES / "demo" / "F01.pdf")
    ingest_fails(runtime, runtime.uploads / ".." / "uploads" / "F01.pdf", "INVALID_PATH")


def test_symlinked_upload_is_refused(runtime):
    target = runtime.upload(FIXTURES / "demo" / "F01.pdf")
    link = runtime.uploads / "link.pdf"
    link.symlink_to(target)
    ingest_fails(runtime, link, "INVALID_PATH")


def test_symlinked_batch_directory_is_refused(runtime, tmp_path):
    elsewhere = tmp_path / "elsewhere"
    elsewhere.mkdir()
    (runtime.root / "linked").symlink_to(elsewhere)
    runtime.batch = runtime.root / "linked" / "batch"
    ingest_fails(runtime, runtime.upload(FIXTURES / "demo" / "F01.pdf"), "INVALID_PATH")


def test_batch_outside_root_is_refused(runtime, tmp_path):
    runtime.batch = tmp_path / "not-in-root"
    ingest_fails(runtime, runtime.upload(FIXTURES / "demo" / "F01.pdf"), "INVALID_PATH")


def test_traversal_original_name_is_display_only(runtime):
    record = runtime.ingest(runtime.upload(FIXTURES / "demo" / "F01.pdf"), name="../../etc/passwd")
    assert record["original_name"] == "../../etc/passwd"
    stored = sorted(path.name for path in (runtime.batch / "objects").iterdir())
    assert stored == [record["sha256"], record["sha256"] + ".json"]


def test_same_file_twice_is_not_duplicated(runtime):
    path = runtime.upload(FIXTURES / "demo" / "F01.pdf")
    first = runtime.ok("ingest", {"source_path": str(path), "original_name": "a.pdf"})
    second = runtime.ok("ingest", {"source_path": str(path), "original_name": "b.pdf"})
    assert (first["duplicate"], second["duplicate"]) == (False, True)
    assert second["source_file"] == first["source_file"]


def test_crash_between_object_and_metadata_is_repaired(runtime):
    path = runtime.upload(FIXTURES / "demo" / "F01.pdf")
    record = runtime.ingest(path)
    (runtime.batch / "objects" / (record["sha256"] + ".json")).unlink()
    again = runtime.ok("ingest", {"source_path": str(path), "original_name": "F01.pdf"})
    assert again == {"source_file": record, "duplicate": False}


def test_changed_stored_object_is_detected(runtime):
    record = runtime.ingest(runtime.upload(FIXTURES / "demo" / "F01.pdf"))
    (runtime.batch / "objects" / record["sha256"]).write_bytes(b"%PDF-1.4 changed")
    runtime.fails("extract", {"source_file_id": record["id"]}, "SNAPSHOT_MISMATCH", 3)


def test_batch_file_limit(runtime):
    for index in range(100):
        path = runtime.uploads / f"n{index}.txt"
        path.write_bytes(f"note {index}\n".encode())
        runtime.ingest(path)
    extra = runtime.uploads / "extra.txt"
    extra.write_bytes(b"one more\n")
    ingest_fails(runtime, extra, "FILE_LIMIT")


def test_extract_refuses_non_invoice(runtime):
    record = runtime.ingest(runtime.upload(FIXTURES / "demo" / "wechat_bill.xlsx"))
    runtime.fails("extract", {"source_file_id": record["id"]}, "UNSUPPORTED_FILE", 2)


def test_object_identifier_must_be_a_hash(runtime):
    runtime.fails("extract", {"source_file_id": "../objects/x"}, "INVALID_PATH", 2)


# --- envelope contract -------------------------------------------------------------------------


def envelope(runtime, **changes):
    value = {"schema_version": 1, "command": "ingest", "request_id": "req-9", "batch_dir": str(runtime.batch),
             "policy_path": str(runtime.policy), "input": {"source_path": str(runtime.uploads / "x"), "original_name": "x"}}
    value.update(changes)
    return json.dumps(value, ensure_ascii=False).encode()


@pytest.mark.parametrize("raw, code", [
    (b"{not json", "INVALID_JSON"),
    (b'{"schema_version":1,"schema_version":1}', "INVALID_JSON"),
    (b'{"schema_version":1.0}', "INVALID_JSON"),
    (b'{"schema_version":NaN}', "INVALID_JSON"),
    (b"[]", "INVALID_SCHEMA"),
])
def test_malformed_requests(runtime, raw, code):
    status, response = runtime.call("ingest", None, raw=raw)
    assert (status, response["ok"], response["error"]["code"]) == (2, False, code)
    assert response["request_id"] is None


@pytest.mark.parametrize("changes, code", [
    ({"schema_version": True}, "UNSUPPORTED_VERSION"),
    ({"schema_version": 2}, "UNSUPPORTED_VERSION"),
    ({"command": "extract"}, "UNKNOWN_COMMAND"),
    ({"extra": 1}, "INVALID_SCHEMA"),
    ({"input": {"source_path": "x", "original_name": "x", "more": 1}}, "INVALID_SCHEMA"),
    ({"request_id": ""}, "INVALID_SCHEMA"),
])
def test_invalid_envelopes_keep_request_id(runtime, changes, code):
    status, response = runtime.call("ingest", None, raw=envelope(runtime, **changes))
    assert (status, response["ok"], response["error"]["code"]) == (2, False, code)


def test_unknown_commands(runtime):
    # `claim` was refused as a later-phase command until P4c; it now exists and checks its fields.
    status, response = runtime.call("claim", None, raw=envelope(runtime, command="claim"), argv=["claim"])
    assert (status, response["error"]["code"]) == (2, "INVALID_SCHEMA")
    status, response = runtime.call("drop", None, raw=envelope(runtime, command="drop"), argv=["drop"])
    assert (status, response["error"]["code"]) == (2, "UNKNOWN_COMMAND")
    status, response = runtime.call("ingest", None, raw=envelope(runtime), argv=[])
    assert (status, response["error"]["code"]) == (2, "UNKNOWN_COMMAND")


def test_request_size_limit(runtime):
    raw = envelope(runtime, input={"source_path": "x" * (4 * 1024 * 1024), "original_name": "x"})
    status, response = runtime.call("ingest", None, raw=raw)
    assert (status, response["error"]["code"]) == (2, "INPUT_TOO_LARGE")


def test_runtime_root_is_required(runtime):
    status, response = runtime.call("ingest", None, raw=envelope(runtime), env={"REIMB_DATA": None})
    assert (status, response["error"]["code"]) == (2, "INVALID_PATH")


def test_errors_do_not_echo_content(runtime):
    secret = "机密内容-不应回显"
    status, response = runtime.call("ingest", None, raw=envelope(runtime, input={"source_path": secret, "original_name": secret}))
    assert status == 2 and secret not in json.dumps(response, ensure_ascii=False)


@pytest.mark.parametrize("edit, message", [
    (lambda text: text + "company: {name: 重复, tax_id: 91440300XXXXXXXX0A}\n", "duplicate"),
    (lambda text: text.replace("evidence_window_days: 45", "evidence_window_days: &w 45\nlate: *w"), "alias"),
    (lambda text: text.replace("late_invoice: next_batch\n", ""), "missing"),
    (lambda text: text.replace("hotel_per_night_cents: 50000", "hotel_per_night_cents: 500.5"), "float"),
    (lambda text: text.replace("住宿: {btype: 差旅费", "住/宿: {btype: 差旅费"), "unsafe name"),
    (lambda text: text.replace('"示例科技-费用报销发票交接清单-{applicant}-CNY{total}.xlsx"', '"{path}.xlsx"'), "ledger"),
])
def test_invalid_policy_is_refused(runtime, edit, message):
    runtime.policy.write_text(edit(runtime.policy.read_text(encoding="utf-8")), encoding="utf-8")
    path = runtime.upload(FIXTURES / "demo" / "F01.pdf")
    runtime.fails("ingest", {"source_path": str(path), "original_name": "x"}, "INVALID_POLICY", 2)


def test_output_files_are_private(runtime):
    record = runtime.ingest(runtime.upload(FIXTURES / "demo" / "F01.pdf"))
    mode = os.stat(runtime.batch / "objects" / record["sha256"]).st_mode & 0o777
    assert mode == 0o600


def minimal_pdf(content, image=None):
    """A strict, well-formed one-page PDF with a chosen content stream and optional image XObject."""
    objects = [b"<< /Type /Catalog /Pages 2 0 R >>", b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>"]
    resources = b"<< /XObject << /Im1 5 0 R >> >>" if image else b"<< >>"
    objects.append(b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources " + resources + b" /Contents 4 0 R >>")
    stream = zlib.compress(content)
    objects.append(b"<< /Length %d /Filter /FlateDecode >>\nstream\n" % len(stream) + stream + b"\nendstream")
    if image:
        width, height = image
        pixels = zlib.compress(b"\x00" * 3)
        objects.append(b"<< /Type /XObject /Subtype /Image /Width %d /Height %d /ColorSpace /DeviceRGB "
                       b"/BitsPerComponent 8 /Length %d /Filter /FlateDecode >>\nstream\n" % (width, height, len(pixels))
                       + pixels + b"\nendstream")
    output, offsets = bytearray(b"%PDF-1.4\n"), []
    for number, body in enumerate(objects, 1):
        offsets.append(len(output))
        output += b"%d 0 obj\n" % number + body + b"\nendobj\n"
    xref = len(output)
    output += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objects) + 1)
    output += b"".join(b"%010d 00000 n \n" % offset for offset in offsets)
    output += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objects) + 1, xref)
    return bytes(output)


def test_minimal_pdf_helper_is_readable(runtime):
    path = runtime.uploads / "ok.pdf"
    path.write_bytes(minimal_pdf(b"q Q", image=(10, 10)))
    assert runtime.ingest(path)["detected_type"] == "image_invoice_pdf"


def test_embedded_image_pixel_limit(runtime):
    path = runtime.uploads / "tall.pdf"
    path.write_bytes(minimal_pdf(b"q Q", image=(8000, 5001)))
    ingest_fails(runtime, path, "PARSE_LIMIT")


def test_content_stream_decompression_bomb(runtime):
    path = runtime.uploads / "bomb.pdf"
    path.write_bytes(minimal_pdf(b" " * (40 * 1024 * 1024)))
    assert path.stat().st_size < 1024 * 1024
    ingest_fails(runtime, path, "PARSE_LIMIT")
