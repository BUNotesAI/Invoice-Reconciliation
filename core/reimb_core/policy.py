"""Validated policies are hashed by semantics, not YAML layout."""
import re
from typing import Annotated, Literal

import yaml
from pydantic import Field, ValidationError, model_validator

from .errors import CoreError, require
from .models import Record
from .values import digest, safe_name

Cents = Annotated[int, Field(ge=0, le=2**63 - 1)]


class Company(Record):
    name: str
    tax_id: Annotated[str, Field(pattern=r"^[0-9A-Z]{18}$")]


class Limits(Record):
    hotel_per_night_cents: Cents
    meal_per_day_cents: Cents
    local_taxi_per_day_cents: Cents


class Category(Record):
    btype: str
    summary: str


class OverLimit(Record):
    explain_by: Literal["applicant"]
    approve_by: Literal["finance"]


class Naming(Record):
    pdf: Literal["{invoice_no}_{short}_{amount}_{category}.pdf"]
    package_dir: Literal["{date}{applicant}报销"]
    ledger: str

    @model_validator(mode="after")
    def ledger_template(self):
        fields = re.findall(r"\{([^{}]*)\}", self.ledger)
        literal = re.sub(r"\{(applicant|total)\}", "", self.ledger)
        require(set(fields) <= {"applicant", "total"} and "{" not in literal and "}" not in literal
                and self.ledger.endswith(".xlsx"), "INVALID_POLICY", "Invalid ledger naming template")
        safe_name(literal, 120)
        return self


class MissingInvoice(Record):
    deadline: Literal["next_month_end"]
    remind: list[Annotated[str, Field(pattern=r"^(on_detect|weekly_(mon|tue|wed|thu|fri|sat|sun)_[0-2][0-9]:[0-5][0-9]|deadline_minus_[1-9][0-9]?d)$")]]


class Policy(Record):
    company: Company
    timezone: Literal["Asia/Shanghai"]
    finance: Annotated[list[Annotated[str, Field(pattern=r"^@[^: ]+:[^ ]+$")]], Field(min_length=1)]
    cycle: Literal["monthly"]
    remind_at: Annotated[str, Field(pattern=r"^last_day [0-2][0-9]:[0-5][0-9]$")]
    limits: Limits
    over_limit: OverLimit
    categories: Annotated[dict[str, Category], Field(min_length=1)]
    short_names: dict[str, str]
    naming: Naming
    evidence_window_days: Annotated[int, Field(ge=0, le=366)]
    late_invoice: Literal["next_batch"]
    missing_invoice: MissingInvoice
    ignore_merchants: list[Annotated[str, Field(min_length=1, max_length=255)]]

    @model_validator(mode="after")
    def names(self):
        safe_name(self.company.name)
        for key, category in self.categories.items():
            safe_name(key)
            safe_name(category.btype)
            safe_name(category.summary)
        for seller, short in self.short_names.items():
            require(0 < len(seller) <= 255, "INVALID_POLICY", "Invalid seller name")
            safe_name(short)
        return self

    def sha(self):
        return digest(self.model_dump())


class UniqueLoader(yaml.SafeLoader):
    pass


def unique_mapping(loader, node, deep=False):
    result = {}
    for key_node, value_node in node.value:
        key = loader.construct_object(key_node, deep=deep)
        require(isinstance(key, str) and key not in result, "INVALID_POLICY", "Duplicate or invalid policy key")
        result[key] = loader.construct_object(value_node, deep=deep)
    return result


UniqueLoader.add_constructor(yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, unique_mapping)


def load_policy(path):
    require(path.stat().st_size <= 65536, "INVALID_POLICY", "Policy too large")
    try:
        text = path.read_text(encoding="utf-8")
        # No aliases: prevents recursive structures and expansion bombs.
        require(not any(isinstance(token, (yaml.tokens.AliasToken, yaml.tokens.AnchorToken)) for token in yaml.scan(text)),
                "INVALID_POLICY", "Policy aliases are not supported")
        return Policy.model_validate(yaml.load(text, Loader=UniqueLoader))
    except CoreError as error:
        raise CoreError("INVALID_POLICY", error.message) from None
    except (yaml.YAMLError, ValidationError, ValueError, RecursionError, TypeError, UnicodeError):
        raise CoreError("INVALID_POLICY", "Invalid policy") from None
