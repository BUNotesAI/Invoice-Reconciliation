"""Exact values, canonical hashing, and safe boundary validation."""
import hashlib
import json
import re
from datetime import date

from .errors import CoreError, require

MAX_CENTS = 2**63 - 1
# Uppercase amounts above this are not produced by invoices; the bound keeps the grammar finite.
MAX_UPPER_CENTS = 10**12 * 100 - 1
NUMERALS = "零壹贰叁肆伍陆柒捌玖"
DIGITS = {char: index for index, char in enumerate(NUMERALS)}
SECTION_UNITS = ("仟", "佰", "拾", "")
GROUP_UNITS = ("亿", "万", "")


def cents(value):
    require(type(value) is int and 0 <= value <= MAX_CENTS, message="Invalid amount cents")
    return value


def sum_cents(values):
    total = 0
    for value in values:
        total = cents(total + cents(value))
    return total


def decimal_cents(text):
    require(isinstance(text, str) and len(text) <= 24
            and re.fullmatch(r"(?:0|[1-9][0-9]*)(?:\.[0-9]{1,2})?", text), message="Invalid decimal amount")
    whole, _, fraction = text.partition(".")
    return cents(int(whole) * 100 + int(fraction.ljust(2, "0")))


def amount_text(value):
    """Plain yuan text with trailing zeros removed: 3080 -> 30.8, 5200 -> 52."""
    value = cents(value)
    return f"{value // 100}.{value % 100:02d}".rstrip("0").rstrip(".")


def _section(value):
    # One four-digit section; runs of zeros collapse to one 零 and trailing zeros vanish.
    text, pending_zero = "", False
    for unit, digit in zip(SECTION_UNITS, f"{value:04d}"):
        if digit == "0":
            pending_zero = bool(text)
            continue
        if pending_zero:
            text += "零"
            pending_zero = False
        text += NUMERALS[int(digit)] + unit
    return text


def cents_to_cn_upper(value):
    """Canonical invoice spelling, e.g. 108000 -> 壹仟零捌拾圆整, 3080 -> 叁拾圆捌角."""
    value = cents(value)
    require(value <= MAX_UPPER_CENTS, message="Amount too large for uppercase form")
    yuan, jiao, fen = value // 100, value // 10 % 10, value % 10
    text = ""
    if yuan:
        groups = [yuan // 10**8, yuan // 10**4 % 10**4, yuan % 10**4]
        for index, (unit, group) in enumerate(zip(GROUP_UNITS, groups)):
            if not group:
                continue
            higher = any(groups[:index])
            if higher and (group < 1000 or not groups[index - 1]):
                text += "零"
            text += _section(group) + unit
        text += "圆"
    if jiao:
        text += NUMERALS[jiao] + "角"
    if fen:
        text += ("零" if yuan and not jiao else "") + NUMERALS[fen] + "分"
    if not jiao and not fen:
        text = (text or "零圆") + "整"
    return text


def _loose_upper(text):
    # Reads the numeric value only; strictness comes from comparing with the canonical spelling.
    total = section = number = 0
    result_fraction = 0
    integer, sep, fraction = text.partition("圆")
    if not sep:
        integer, fraction = "", text
    for char in integer:
        if char in DIGITS:
            number = DIGITS[char]
        elif char in ("仟", "佰", "拾"):
            section += number * {"仟": 1000, "佰": 100, "拾": 10}[char]
            number = 0
        elif char == "万":
            total += (section + number) * 10**4
            section = number = 0
        elif char == "亿":
            total = (total + section + number) * 10**8
            section = number = 0
        else:
            return None
    fraction = fraction.removesuffix("整").removesuffix("正")
    match = re.fullmatch(r"(?:([零壹贰叁肆伍陆柒捌玖])角)?(?:零?([零壹贰叁肆伍陆柒捌玖])分)?", fraction)
    if match is None:
        return None
    result_fraction = DIGITS.get(match[1], 0) * 10 + DIGITS.get(match[2], 0)
    return (total + section + number) * 100 + result_fraction


def cn_upper_to_cents(text):
    """Accept only the canonical spelling (with 元/圆, 整/正 and 人民币 variants)."""
    require(isinstance(text, str) and 0 < len(text) <= 64, message="Invalid uppercase amount")
    normalized = text.strip().removeprefix("人民币").removeprefix("¥").replace("元", "圆").replace("正", "整")
    require(bool(re.fullmatch(r"[零壹贰叁肆伍陆柒捌玖拾佰仟万亿圆角分整]+", normalized)), message="Invalid uppercase amount")
    value = _loose_upper(normalized)
    require(value is not None and value <= MAX_UPPER_CENTS, message="Invalid uppercase amount")
    canonical_text = cents_to_cn_upper(value)
    # Invoices sometimes append 整 after 角; that is the only tolerated deviation.
    accepted = {canonical_text}
    if canonical_text.endswith("角"):
        accepted.add(canonical_text + "整")
    require(normalized in accepted, message="Non-canonical uppercase amount")
    return value


def invoice_number(value):
    require(isinstance(value, str) and re.fullmatch(r"[0-9]{20}", value), message="Invalid invoice number")
    return value


def local_date(value):
    require(isinstance(value, str) and re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}", value), message="Invalid date")
    try:
        date.fromisoformat(value)
    except ValueError:
        raise CoreError("INVALID_SCHEMA", "Invalid date") from None
    return value


def utc_instant(value):
    require(isinstance(value, str) and re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]{1,6})?Z", value),
            message="Invalid UTC instant")
    from datetime import datetime
    try:
        datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        raise CoreError("INVALID_SCHEMA", "Invalid UTC instant") from None
    return value


def safe_name(value, limit=80):
    require(isinstance(value, str) and 0 < len(value) <= limit and value == value.strip()
            and not re.search(r"[\\/:\x00-\x1f\x7f]", value) and value not in (".", ".."),
            "INVALID_PATH", "Unsafe name")
    return value


def canonical(value):
    def visit(item, depth=0):
        require(depth < 64, message="JSON nesting too deep")
        require(type(item) in (dict, list, str, int, bool, type(None)), message="Non-canonical JSON value")
        if isinstance(item, dict):
            require(all(isinstance(key, str) for key in item), message="Invalid JSON key")
            for child in item.values():
                visit(child, depth + 1)
        elif isinstance(item, list):
            for child in item:
                visit(child, depth + 1)
    visit(value)
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"), allow_nan=False).encode()


def digest(value):
    return hashlib.sha256(canonical(value)).hexdigest()


def strict_json(data):
    def pairs(entries):
        result = {}
        for key, value in entries:
            require(key not in result, "INVALID_JSON", "Duplicate JSON key")
            result[key] = value
        return result

    def invalid(_):
        raise CoreError("INVALID_JSON", "Non-finite JSON number")

    def no_float(_):
        raise CoreError("INVALID_JSON", "Floating point numbers are not accepted")
    try:
        return json.loads(data, object_pairs_hook=pairs, parse_constant=invalid, parse_float=no_float)
    except (ValueError, UnicodeError, RecursionError):
        raise CoreError("INVALID_JSON", "Invalid JSON") from None
