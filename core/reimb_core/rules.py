"""Expense rules keyed off invoice facts. Labels chosen by people or models never switch a rule off."""
from .errors import CoreError

LODGING_SELLER_WORDS = ("酒店", "宾馆", "旅馆", "客栈", "民宿")


def fact_kind(invoice):
    """The expense kind the invoice facts themselves show; the project line decides before the seller name."""
    project = invoice.project.value
    if "住宿" in project or invoice.service_period is not None:
        return "lodging"
    if "餐饮" in project:
        return "meal"
    if "航空" in project:
        return "air"
    if "客运" in project:
        return "local_taxi"
    if any(word in invoice.seller_name.value for word in LODGING_SELLER_WORDS):
        return "lodging"
    return "other"


def check_category(invoice, category):
    """A category must agree with the facts in both directions for any kind that carries a rule."""
    shown = fact_kind(invoice)
    if (shown != "other" or category.kind != "other") and shown != category.kind:
        raise CoreError("FIELD_CONFLICT", "Category differs from invoice facts", 3)
    return shown


def stay_over_limit(invoice, policy):
    period = invoice.service_period
    return period is not None and invoice.amount_cents.value > period.nights.value * policy.limits.hotel_per_night_cents
