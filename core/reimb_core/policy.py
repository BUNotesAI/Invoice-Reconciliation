"""Validated policies are hashed by semantics, not YAML layout."""
from pydantic import Field, ValidationError, model_validator
from typing import Annotated, Literal
import yaml
from .models import Record
from .values import safe_name, digest
from .errors import CoreError, require


class Company(Record):
    name: str
    tax_id: Annotated[str, Field(pattern=r"^[A-Z0-9]{18}$")]


class Limits(Record):
    hotel_per_night_cents: Annotated[int, Field(ge=0, le=2**63-1)]
    meal_per_day_cents: Annotated[int, Field(ge=0, le=2**63-1)]
    local_taxi_per_day_cents: Annotated[int, Field(ge=0, le=2**63-1)]


class Category(Record):
    btype: str
    summary: str


class OverLimit(Record):
    explain_by: Literal["applicant"]
    approve_by: Literal["finance"]


class Naming(Record):
    pdf: Literal["{invoice_no}_{short}_{amount}_{category}.pdf"]
    package_dir: Literal["{date}{applicant}报销"]


class Policy(Record):
    company: Company
    timezone: Literal["Asia/Shanghai"]
    finance: list[Annotated[str, Field(pattern=r"^@[^: ]+:[^ ]+$")]]
    cycle: Literal["monthly"]
    remind_at: str
    limits: Limits
    over_limit: OverLimit
    categories: dict[str, Category]
    short_names: dict[str, str]
    naming: Naming
    evidence_window_days: Annotated[int, Field(ge=0, le=366)] = 7
    ignored_merchants: list[str] = Field(default_factory=list)

    @model_validator(mode="after")
    def names(self):
        safe_name(self.company.name)
        require(bool(self.categories) and bool(self.finance), "INVALID_POLICY", "Empty policy rules")
        for key, category in self.categories.items():
            safe_name(key)
            safe_name(category.btype)
            safe_name(category.summary)
        for value in self.short_names.values():
            safe_name(value)
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
        # No aliases: prevents recursive structures and expansion bombs.
        text = path.read_text()
        require(not any(isinstance(t, (yaml.tokens.AliasToken, yaml.tokens.AnchorToken))
                        for t in yaml.scan(text)), "INVALID_POLICY", "Policy aliases are not supported")
        return Policy.model_validate(yaml.load(text, Loader=UniqueLoader))
    except (yaml.YAMLError, ValidationError, ValueError, RecursionError, TypeError):
        raise CoreError("INVALID_POLICY", "Invalid policy") from None
