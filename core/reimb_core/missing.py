"""Rule-graded missing-invoice detection. Nothing is judged business spending automatically."""
from datetime import date, timedelta

from .errors import require
from .evidence import Evidence
from .history import validated_history
from .link import merchant_matches, window
from .values import local_date

KEYWORDS = ("酒店", "航空", "出行", "办公")


def deadline(period):
    """next_month_end: the last day of the month after the batch period."""
    _, end = window(period, 0)
    following = date.fromisoformat(end) + timedelta(days=1)
    return window(following.isoformat()[:7], 0)[1]


def grade(payment, policy, history_sellers, travel):
    merchant = payment.merchant
    known = list(policy.short_names) + list(policy.short_names.values()) + history_sellers
    if any(merchant_matches(merchant, name) or name in merchant for name in known):
        return "high", "KNOWN_MERCHANT"
    if payment.payment_date in travel:
        return "medium", "TRAVEL_WINDOW"
    if any(word in merchant or word in payment.goods for word in KEYWORDS):
        return "medium", "CATEGORY_KEYWORD"
    return "low", "NO_SIGNAL"


def missing(evidence, occupancy, policy, ignored_transactions, period, history_snapshot=None, ignored_merchants=None):
    """`ignored_merchants` are the applicant's own 「以后都不提醒这个商户」 choices, on top of the policy table."""
    require(isinstance(evidence, list) and isinstance(ignored_transactions, list)
            and all(isinstance(ref, str) for ref in ignored_transactions), message="Invalid missing input")
    ignored_merchants = [] if ignored_merchants is None else ignored_merchants
    require(isinstance(ignored_merchants, list) and all(isinstance(name, str) and 0 < len(name) <= 128 for name in ignored_merchants),
            message="Invalid ignored merchants")
    muted = list(policy.ignore_merchants) + [name for name in ignored_merchants if name not in policy.ignore_merchants]
    require(isinstance(occupancy, dict) and set(occupancy) == {"claims", "travel_dates"}, message="Invalid occupancy")
    evidence = [Evidence.model_validate(raw) for raw in evidence]
    taken = {claim["evidence_id"] for claim in occupancy["claims"]}
    travel = set()
    for day in occupancy["travel_dates"]:
        base = date.fromisoformat(local_date(day))
        travel |= {(base + timedelta(days=offset)).isoformat() for offset in (-1, 0, 1)}
    history_sellers = []
    if history_snapshot is not None:
        history_sellers = sorted({row["seller_name"] for row in validated_history(history_snapshot)["validated_snapshot"]})
    ignored = set(ignored_transactions)
    candidates, not_included, ignored_rows, ignored_by_merchant = [], [], [], []
    due = deadline(period)
    for payment in sorted(evidence, key=lambda item: (item.payment_date or "", item.id)):
        # Only completed outgoing merchant purchases can be spending that lacks an invoice.
        if (payment.kind != "wechat_payment" or payment.flow != "支出" or payment.trade_type != "商户消费"
                or payment.payment_status != "paid" or payment.id in taken):
            continue
        if payment.transaction_ref in ignored:
            ignored_rows.append(payment.id)
            continue
        if any(name in payment.merchant for name in muted):
            ignored_by_merchant.append(payment.id)
            continue
        likelihood, reason = grade(payment, policy, history_sellers, travel)
        row = {"id": "missing-" + payment.id, "payment_evidence_id": payment.id, "merchant": payment.merchant,
               "amount_cents": payment.amount_cents, "payment_date": payment.payment_date, "likelihood": likelihood,
               "reason": reason}
        if likelihood == "low":
            not_included.append(row)
        else:
            candidates.append(dict(row, status="discovered", deadline=due, decision_history=[], claimed_invoice_id=None))
    return {"candidates": candidates, "not_included": not_included, "ignored_transactions": ignored_rows,
            "ignored_by_merchant": ignored_by_merchant, "ignored_merchants": muted,
            # What the follow-up needs from the policy: the title to ask for, when to remind, until when.
            "follow_up": {"billing": {"name": policy.company.name, "tax_id": policy.company.tax_id},
                          "remind": list(policy.missing_invoice.remind), "deadline": due}}
