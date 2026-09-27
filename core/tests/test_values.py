"""Exact value parsing. Answers are written by hand, never produced by the renderer under test."""
import pytest

from reimb_core.errors import CoreError
from reimb_core.values import amount_text, cents_to_cn_upper, cn_upper_to_cents, decimal_cents, safe_name, strict_json

UPPER = [
    ("零圆整", 0),
    ("伍分", 5),
    ("伍角", 50),
    ("壹圆零伍分", 105),
    ("叁拾圆捌角", 3080),
    ("贰拾柒圆伍角", 2750),
    ("壹拾伍圆整", 1500),
    ("伍拾贰圆整", 5200),
    ("叁佰捌拾陆圆整", 38600),
    ("壹仟零捌拾圆整", 108000),
    ("壹仟零伍圆整", 100500),
    ("壹仟贰佰捌拾圆整", 128000),
    ("肆仟玖佰零壹圆肆角", 490140),
    ("壹万圆整", 1000000),
    ("壹万零壹拾圆整", 1001000),
    ("壹万零伍佰圆整", 1050000),
    ("壹拾万零伍圆整", 10000500),
    ("壹佰万零伍圆整", 100000500),
    ("壹仟零壹万圆整", 1001000000),
    ("壹亿零伍圆整", 10000000500),
    ("壹亿零壹仟圆整", 10000100000),
    ("壹亿贰仟万圆整", 12000000000),
    ("玖仟玖佰玖拾玖圆玖角玖分", 999999),
]

VARIANTS = [
    ("人民币叁拾圆捌角", 3080),
    ("叁拾元捌角", 3080),
    ("叁拾圆捌角整", 3080),
    ("伍拾贰元正", 5200),
]

REJECTED = [
    "", "叁拾", "叁拾圆零捌角", "壹仟捌拾圆整", "壹仟零零捌拾圆整", "伍零圆整", "拾伍圆整", "壹拾伍圆零整",
    "叁拾圆捌角伍", "叁拾圆捌分角", "壹佰壹仟圆整", "壹万万圆整", "叁拾圆捌角伍分整", "三十元", "30.80",
    "壹拾伍圆零分", "零伍圆整", "壹仟零圆整", "叁拾圆捌角零分",
]


@pytest.mark.parametrize("text, value", UPPER)
def test_canonical_uppercase(text, value):
    assert cn_upper_to_cents(text) == value
    assert cents_to_cn_upper(value) == text


@pytest.mark.parametrize("text, value", VARIANTS)
def test_accepted_variants(text, value):
    assert cn_upper_to_cents(text) == value


@pytest.mark.parametrize("text", REJECTED)
def test_rejected_uppercase(text):
    with pytest.raises(CoreError):
        cn_upper_to_cents(text)


@pytest.mark.parametrize("text, value", [("0", 0), ("30.8", 3080), ("30.80", 3080), ("1460.00", 146000), ("0.05", 5)])
def test_decimal_cents(text, value):
    assert decimal_cents(text) == value


@pytest.mark.parametrize("text", ["", "-1", "01", "1.234", "1e3", " 1", "1,000", "NaN", "１２"])
def test_decimal_rejections(text):
    with pytest.raises(CoreError):
        decimal_cents(text)


@pytest.mark.parametrize("value, text", [(3080, "30.8"), (5200, "52"), (146000, "1460"), (5, "0.05"), (490140, "4901.4")])
def test_amount_text_drops_trailing_zeros(value, text):
    assert amount_text(value) == text


@pytest.mark.parametrize("value", [True, 1.5, -1, 2**63, "1"])
def test_amount_text_rejects_non_cents(value):
    with pytest.raises(CoreError):
        amount_text(value)


@pytest.mark.parametrize("name", ["a/b", "a\\b", "..", " x", "x ", "a:b", "a\x00b", "", "x" * 81])
def test_unsafe_names(name):
    with pytest.raises(CoreError):
        safe_name(name)


@pytest.mark.parametrize("raw", [b'{"a":1,"a":2}', b'{"a":1.0}', b'{"a":Infinity}', b'{"a":-NaN}', b"\xff"])
def test_strict_json_rejections(raw):
    with pytest.raises(CoreError):
        strict_json(raw)
