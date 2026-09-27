"""Exact values, canonical hashing, and safe boundary validation."""
import hashlib
import json
import re
from datetime import date
from decimal import Decimal, InvalidOperation

from .errors import CoreError, require

MAX_CENTS = 2**63 - 1
DIGITS = dict(zip("零壹贰叁肆伍陆柒捌玖", range(10)))
UNITS = {"拾": 10, "佰": 100, "仟": 1000}
GROUPS = {"万": 10000, "亿": 100000000}


def cents(value):
    require(type(value) is int and 0 <= value <= MAX_CENTS, message="Invalid amount cents")
    return value


def sum_cents(values):
    total = 0
    for value in values:
        total = cents(total + cents(value))
    return total


def decimal_cents(text):
    require(isinstance(text, str) and re.fullmatch(r"(?:0|[1-9][0-9]*)(?:\.[0-9]{1,2})?", text),
            message="Invalid decimal amount")
    try:
        return cents(int(Decimal(text) * 100))
    except (InvalidOperation, OverflowError):
        raise CoreError("INVALID_SCHEMA", "Invalid decimal amount") from None


def amount_text(value):
    value = cents(value)
    return f"{value // 100}.{value % 100:02d}".rstrip("0").rstrip(".")


def cn_upper_to_cents(text):
    require(isinstance(text, str), message="Invalid uppercase amount")
    text = text.removeprefix("人民币").replace("圆", "元")
    require(bool(re.fullmatch(r"[零壹贰叁肆伍陆柒捌玖拾佰仟万亿]+元(?:整|正|[零壹贰叁肆伍陆柒捌玖角分]+)", text)),
            message="Invalid uppercase amount")
    integer, fraction = text.split("元")
    total = section = number = 0
    last_unit = 10000
    for char in integer:
        if char in DIGITS:
            require(number == 0, message="Adjacent uppercase digits")
            number = DIGITS[char]
        elif char in UNITS:
            unit = UNITS[char]
            require(unit < last_unit and number > 0, message="Invalid uppercase unit order")
            section += number * unit
            number = 0
            last_unit = unit
        else:
            group = GROUPS[char]
            section += number
            require(section > 0, message="Empty uppercase group")
            if group == 100000000:
                total = (total + section) * group
            else:
                total += section * group
            section = number = 0
            last_unit = 10000
    result = (total + section + number) * 100
    if fraction not in ("整", "正"):
        match = re.fullmatch(r"(?:([壹贰叁肆伍陆柒捌玖])角)?(?:零?([壹贰叁肆伍陆柒捌玖])分)?", fraction)
        require(match is not None and any(match.groups()), message="Invalid uppercase fraction")
        result += DIGITS.get(match[1], 0) * 10 + DIGITS.get(match[2], 0)
    return cents(result)


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


def safe_name(value, limit=80):
    require(isinstance(value, str) and 0 < len(value) <= limit and value == value.strip()
            and not re.search(r"[\\/:\x00-\x1f\x7f]", value) and value not in (".", ".."),
            "INVALID_PATH", "Unsafe name")
    return value


def canonical(value):
    def visit(item):
        require(type(item) in (dict, list, str, int, bool, type(None)), message="Non-canonical JSON value")
        if isinstance(item, dict):
            require(all(isinstance(key, str) for key in item), message="Invalid JSON key")
            for child in item.values():
                visit(child)
        elif isinstance(item, list):
            for child in item:
                visit(child)
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
    try:
        return json.loads(data, object_pairs_hook=pairs, parse_constant=invalid)
    except (ValueError, UnicodeError, RecursionError):
        raise CoreError("INVALID_JSON", "Invalid JSON") from None
