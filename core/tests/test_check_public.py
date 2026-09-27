"""Rules of the public-repository scan (P0 finding F1)."""
import subprocess
import sys
from pathlib import Path

import pytest

from conftest import REPO

sys.path.insert(0, str(REPO / "scripts"))
from check_public import pattern_violations  # noqa: E402

PLAIN = Path("docs/note.md")
# Real-looking values are assembled at run time so this file itself stays clean under the scan.
TAX = "9111" + "0108MA01ABCD2X"
PHONE_A, PHONE_B = "138" + "12345678", "159" + "00001111"
MAIL = "li.si" + "@" + "gmail.com"


@pytest.mark.parametrize("text, rule", [
    ("税号 " + TAX, "tax_id"),
    ("联系 " + PHONE_A, "phone"),
    ("电话：" + PHONE_B + "。", "phone"),
    ("mail " + MAIL, "email"),
    ("票号 26442000000100010131", "invoice_number"),
])
def test_real_looking_values_are_found(text, rule):
    assert rule in pattern_violations(text.encode(), PLAIN)


@pytest.mark.parametrize("text", [
    "税号 91440300XXXXXXXX0A",
    "财务 @reimb-zhoumin:reimb.local 与 demo@reimb.local",
    "someone@company.example 或 a@b.invalid",
    # Lock-file shapes that produced the false positives: digits glued to hex letters.
    'checksum = "29333c3ea1ba8b17211763463ff24ee84e41c78224c16b001cd907e663a38c68"',
    'hash = "sha256:a' + PHONE_A + 'b' + TAX[:-1] + '2c"',
    "url = https://files.pythonhosted.org/packages/5b/75/5b20dd1e6573a01a08158fe104104fa2c8abf941745596954185726cd46c/x.whl",
    "size = 12345678901",
])
def test_fictional_and_hash_values_pass(text):
    assert pattern_violations(text.encode(), PLAIN) == []


def test_fictional_invoice_numbers_are_allowed_only_in_fixtures_and_tests():
    text = "26442000000100010131".encode()
    assert pattern_violations(text, Path("fixtures/expected/demo.json")) == []
    assert pattern_violations(text, Path("core/tests/test_demo.py")) == []
    assert pattern_violations(text, Path("core/reimb_core/extract.py")) == ["invoice_number"]


def test_pdf_is_scanned_by_its_text_layer():
    pdf = (REPO / "fixtures" / "demo" / "F01.pdf").read_bytes()
    assert pattern_violations(pdf, Path("docs/sample.pdf")) == ["invoice_number"]
    assert pattern_violations(pdf, Path("fixtures/demo/F01.pdf")) == []


def test_repository_scan_passes():
    process = subprocess.run([sys.executable, str(REPO / "scripts" / "check_public.py")], capture_output=True, cwd=REPO)
    assert process.returncode == 0, process.stdout
