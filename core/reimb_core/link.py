"""Whole-batch linking: one invoice to one payment, unique occupancy, refunds judged after linking."""
import re
from collections import defaultdict
from datetime import date, timedelta
from pydantic import model_validator

from .errors import CoreError, require
from .evidence import Evidence
from .history import replacement_candidates, validated_history
from .models import Decision, HistoryEntry, Identifier, Invoice, Record
from .rules import check_category, fact_kind, stay_over_limit
from .values import local_date, sum_cents

REMARK_DATE = re.compile(r"(?:行程日期|航班\s*[A-Z0-9]{2,8})\s*([0-9]{4}-[0-9]{2}-[0-9]{2})")
# Payment dates stand for the service only where paying and consuming happen together.
IMMEDIATE = {"meal", "local_taxi"}
# Design amendment A1: daily limits apply per service day and per kind shown by the invoice facts.
DAILY_LIMITS = {"meal": "meal_per_day_cents", "local_taxi": "local_taxi_per_day_cents"}
TRAVEL = {"air", "lodging"}
BLOCKING_GATE_REASONS = {"FACT_UNCONFIRMED", "REPLACEMENT_REQUIRES_DECISION", "HISTORY_ALREADY_PAID", "HISTORY_UNKNOWN"}


class Gate(Record):
    disposition: str
    review_reasons: list[str]


class LinkItem(Record):
    id: Identifier
    invoice: Invoice
    gate: Gate
    category: str | None = None
    declared_service_date: str | None = None

    @model_validator(mode="after")
    def values(self):
        if self.declared_service_date is not None:
            local_date(self.declared_service_date)
        if self.gate.disposition not in ("accepted", "needs_decision", "rejected"):
            raise ValueError("Unknown gate disposition")
        return self


def merchant_matches(merchant, seller, remark="", short_names=None):
    """Heuristic brand match: the bill's leading brand characters appear in the seller or remark,
    or the seller's policy short name appears in the bill merchant. A miss only makes a coincidence."""
    merchant = merchant.strip()
    if not merchant:
        return False
    head = merchant[:2] if re.match(r"[一-鿿]", merchant) else merchant.split()[0].lower()
    short = (short_names or {}).get(seller)
    return head in seller or (bool(remark) and head in remark) or (bool(short) and short in merchant)


def rule_category(invoice, categories):
    """The single policy category whose kind the facts show; none when the facts say nothing or it is ambiguous."""
    kind = fact_kind(invoice)
    matches = [name for name, category in categories.items() if category.kind == kind]
    return matches[0] if kind != "other" and len(matches) == 1 else None


def window(period, days):
    start = date.fromisoformat(period + "-01")
    end = (start.replace(day=28) + timedelta(days=4)).replace(day=1) - timedelta(days=1)
    return (start - timedelta(days=days)).isoformat(), end.isoformat()


class Resolver:
    def __init__(self, policy, evidence, history, decisions, period):
        self.policy = policy
        self.evidence = evidence
        self.history = history
        self.decisions = decisions
        self.window = window(period, policy.evidence_window_days)

    def item_decisions(self, item_id, kind):
        return [decision for decision in self.decisions if decision.item_id == item_id and decision.kind == kind]

    def payments(self, invoice):
        """Equal-amount outgoing payments, split into genuine candidates and coincidences."""
        genuine, coincidences = [], []
        for evidence in self.evidence:
            if evidence.kind == "trip" or not evidence.usable() or evidence.amount_cents != invoice.amount_cents.value:
                continue
            if evidence.kind == "wechat_payment" and evidence.flow != "支出":
                continue
            day = evidence.payment_date
            if day is not None and not self.window[0] <= day <= self.window[1]:
                continue
            merchant_ok = merchant_matches(evidence.merchant, invoice.seller_name.value, invoice.remark.value,
                                           self.policy.short_names)
            is_purchase = evidence.kind == "order_screenshot" or evidence.trade_type == "商户消费"
            (genuine if merchant_ok and is_purchase else coincidences).append(evidence)
        return genuine, coincidences

    def trips(self, invoice):
        return [evidence for evidence in self.evidence if evidence.kind == "trip" and evidence.usable()
                and evidence.amount_cents == invoice.amount_cents.value
                and merchant_matches(evidence.merchant, invoice.seller_name.value, "", self.policy.short_names)]

    def dated_sources(self, item, invoice, trips, payment, replaced):
        """Service-date evidence in the design's priority order; payment dates are only a fallback."""
        sources = []
        if item.declared_service_date:
            sources.append(("declared", item.declared_service_date))
        if payment is not None and payment.kind == "order_screenshot":
            sources.append(("screenshot", payment.service_date))
        if len(trips) == 1:
            sources.append(("trip", trips[0].service_date))
        if invoice.service_period is not None:
            sources.append(("stay", invoice.service_period.check_in.value))
        remark = REMARK_DATE.search(invoice.remark.value)
        if remark:
            sources.append(("remark", local_date(remark[1])))
        if replaced is not None:
            sources.append(("history", replaced["service_date"]))
        return sources


def link(items, evidence, history_snapshot, decisions, period, policy, agent_choices=None):
    require(isinstance(items, list) and len(items) <= 100 and isinstance(evidence, list) and len(evidence) <= 5000
            and isinstance(decisions, list), message="Invalid link input")
    require(isinstance(period, str) and re.fullmatch(r"[0-9]{4}-[0-9]{2}", period) is not None, message="Invalid period")
    items = [LinkItem.model_validate(raw) for raw in items]
    evidence = [Evidence.model_validate(raw) for raw in evidence]
    decisions = [Decision.model_validate(raw) for raw in decisions]
    agent_choices = agent_choices or {}
    require(isinstance(agent_choices, dict) and all(isinstance(v, str) for v in agent_choices.values()),
            message="Invalid agent choices")
    require(len({item.id for item in items}) == len(items), message="Duplicate item identifiers")
    require(len({entry.id for entry in evidence}) == len(evidence), message="Duplicate evidence identifiers")
    require(all(decision.item_id in {item.id for item in items} for decision in decisions),
            message="Decision for an unknown item")
    history = [HistoryEntry.model_validate(row) for row in validated_history(history_snapshot)["validated_snapshot"]]
    resolver = Resolver(policy, evidence, history, decisions, period)
    by_id = {entry.id: entry for entry in evidence}

    plans, claims, tentative = {}, defaultdict(list), []
    for item in items:
        invoice = item.invoice
        plan = {"item": item, "reasons": [], "notes": [], "payment": None, "supporting": [], "service_date": None,
                "resolution": None, "decision_id": None, "category": item.category, "category_source": "given",
                "payment_candidates": [], "coincidences": []}
        plans[item.id] = plan
        if item.gate.disposition == "rejected":
            plan["reasons"] = list(item.gate.review_reasons)
            continue
        plan["kind"] = fact_kind(invoice)
        if plan["category"] is None:
            plan["category"], plan["category_source"] = rule_category(invoice, policy.categories), "rule"
        elif plan["category"] not in policy.categories:
            plan["reasons"].append("CATEGORY_UNKNOWN")
        else:
            try:
                check_category(invoice, policy.categories[plan["category"]])
            except CoreError:
                # A suggested category that contradicts the invoice facts is shown to the user, not applied.
                plan["reasons"].append("CATEGORY_CONFLICT")
        if plan["category"] is None:
            plan["reasons"].append("CATEGORY_UNKNOWN")
        genuine, coincidences = resolver.payments(invoice)
        plan["payment_candidates"] = [entry.id for entry in genuine]
        plan["coincidences"] = [entry.id for entry in coincidences]
        if not invoice.trusted():
            # A vision reading may not occupy anything; a single matching payment is only reserved for it.
            plan["reasons"].append("FACT_UNCONFIRMED")
            if len(genuine) == 1:
                tentative.append({"evidence_id": genuine[0].id, "item_id": item.id})
            continue
        replacements = replacement_candidates(invoice, history)
        replaced = None
        if replacements:
            replaced = replacements[0] if len(replacements) == 1 else None
            chosen = [d for d in resolver.item_decisions(item.id, "replace_unpaid_invoice")
                      if replaced and d.typed().invoice_no == replaced["invoice_no"]]
            if replaced is None or replaced["reason"] == "HISTORY_UNKNOWN":
                plan["reasons"].append("HISTORY_UNKNOWN")
            elif replaced["reason"] == "HISTORY_ALREADY_PAID":
                plan["reasons"].append("HISTORY_ALREADY_PAID")
            elif not chosen:
                plan["reasons"].append("REPLACEMENT_REQUIRES_DECISION")
            else:
                plan["decision_id"], plan["resolution"] = chosen[0].id, "confirmed"
        trips = resolver.trips(invoice)
        if len(trips) > 1:
            plan["reasons"].append("MULTIPLE_CANDIDATES")
        # Payment selection: a user choice, a code-verified agent ranking, or the single genuine candidate.
        payment = None
        chosen_evidence = resolver.item_decisions(item.id, "choose_evidence")
        if chosen_evidence:
            target = chosen_evidence[-1].typed().evidence_id
            if target in {entry.id for entry in genuine}:
                payment, plan["resolution"], plan["decision_id"] = by_id[target], "confirmed", chosen_evidence[-1].id
            else:
                plan["reasons"].append("LINK_CONFLICT")
        elif replaced is not None and replaced["reason"] != "HISTORY_UNKNOWN":
            pass  # The historical expense is the payment source; no new bill payment is needed.
        elif len(genuine) == 1:
            payment, plan["resolution"] = genuine[0], "automatic"
        elif len(genuine) > 1:
            choice = agent_choices.get(item.id)
            if choice in {entry.id for entry in genuine}:
                payment, plan["resolution"] = by_id[choice], "automatic"
                plan["notes"].append("AGENT_RANKED")
            else:
                plan["reasons"].append("MULTIPLE_CANDIDATES")
        else:
            plan["reasons"].append("EVIDENCE_MISSING")
        plan["payment"] = payment
        plan["supporting"] = [trips[0].id] if len(trips) == 1 else []
        sources = resolver.dated_sources(item, invoice, trips, payment, replaced)
        days = {day for _, day in sources}
        manual = resolver.item_decisions(item.id, "manual_evidence")
        if manual:
            plan["service_date"], plan["resolution"], plan["decision_id"] = manual[-1].typed().service_date, "manual_exception", manual[-1].id
        elif len(days) > 1:
            plan["reasons"].append("DATE_CONFLICT")
            plan["date_sources"] = sources
        elif days:
            plan["service_date"] = days.pop()
        elif payment is not None and plan["kind"] in IMMEDIATE and payment.payment_date:
            plan["service_date"] = payment.payment_date
        elif payment is not None or replaced is not None:
            plan["reasons"].append("SERVICE_DATE_MISSING")
        if invoice.service_period is None and plan["kind"] == "lodging":
            plan["reasons"].append("STAY_PERIOD_MISSING")
        if plan["service_date"] and not resolver.window[0] <= plan["service_date"] <= resolver.window[1]:
            plan["reasons"].append("OUT_OF_WINDOW")
        if payment is not None:
            claims[payment.id].append(item.id)
        for entry in plan["supporting"]:
            claims[entry].append(item.id)

    # Occupancy is unique across the batch; contested evidence is taken from every claimant.
    contested = {evidence_id for evidence_id, owners in claims.items() if len(owners) > 1}
    for evidence_id in contested:
        for owner in claims[evidence_id]:
            plan = plans[owner]
            if "LINK_CONFLICT" not in plan["reasons"]:
                plan["reasons"].append("LINK_CONFLICT")
            if plan["payment"] is not None and plan["payment"].id == evidence_id:
                plan["payment"], plan["resolution"] = None, None
            plan["supporting"] = [entry for entry in plan["supporting"] if entry != evidence_id]

    # Refund gate runs only on payments that survived linking.
    refund_results = []
    for plan in plans.values():
        payment = plan["payment"]
        if payment is None:
            continue
        refund_results.append({"item_id": plan["item"].id, "payment_evidence_id": payment.id, "status": payment.payment_status,
                               "refund_cents": payment.refund_cents})
        if payment.payment_status == "full_refund":
            plan["reasons"].append("FULL_REFUND")
        elif payment.payment_status == "partial_refund":
            plan["reasons"].append("PARTIAL_REFUND")
        elif payment.payment_status != "paid":
            plan["reasons"].append("PAYMENT_STATUS_UNKNOWN")

    apply_limits(plans, policy, resolver)
    return render(plans, tentative, policy)


def apply_limits(plans, policy, resolver):
    """Daily limits compare the per-day, per-category sum; hotels compare against nights."""
    days = defaultdict(list)
    for plan in plans.values():
        item = plan["item"]
        if item.gate.disposition == "rejected" or not item.invoice.trusted() or plan["service_date"] is None:
            continue
        kind = fact_kind(item.invoice)
        if kind in DAILY_LIMITS:
            days[(kind, plan["service_date"])].append(plan)
        if stay_over_limit(item.invoice, policy):
            over_limit(plan, resolver)
    for (category, _day), group in days.items():
        limit = getattr(policy.limits, DAILY_LIMITS[category])
        if sum_cents(plan["item"].invoice.amount_cents.value for plan in group) > limit:
            for plan in group:
                over_limit(plan, resolver)


def over_limit(plan, resolver):
    # An applicant's explanation settles the applicant side; finance still reviews it.
    if resolver.item_decisions(plan["item"].id, "explain_over_limit"):
        plan["notes"].append("OVER_LIMIT_EXPLAINED")
    else:
        plan["reasons"].append("OVER_LIMIT")


def disposition(plan):
    item = plan["item"]
    if item.gate.disposition == "rejected":
        return "rejected"
    if "FULL_REFUND" in plan["reasons"]:
        return "rejected"
    return "needs_decision" if plan["reasons"] else "accepted"


def render(plans, tentative, policy):
    links, summary = [], {key: {"count": 0, "cents": 0} for key in ("automatic", "needs_decision", "rejected", "accepted")}
    occupancy = []
    for plan in sorted(plans.values(), key=lambda value: value["item"].id):
        item = plan["item"]
        state = disposition(plan)
        amount = item.invoice.amount_cents.value
        key = "automatic" if state == "accepted" and plan["resolution"] == "automatic" and not plan["notes"] else state
        summary[key]["count"] += 1
        summary[key]["cents"] = sum_cents([summary[key]["cents"], amount])
        payment = plan["payment"] if state != "rejected" else None
        links.append({
            "item_id": item.id, "disposition": state, "review_reasons": sorted(set(plan["reasons"])), "notes": sorted(set(plan["notes"])),
            "category": plan["category"], "category_source": plan["category_source"],
            "payment_evidence_id": payment.id if payment else None,
            "supporting_evidence_ids": plan["supporting"] if state != "rejected" else [],
            "service_date": plan["service_date"], "resolution": plan["resolution"] if state != "rejected" else None,
            "decision_id": plan["decision_id"], "payment_candidates": plan["payment_candidates"],
            "coincidences": plan["coincidences"], "date_sources": [list(pair) for pair in plan.get("date_sources", [])]})
        if state != "rejected":
            for evidence_id in ([payment.id] if payment else []) + plan["supporting"]:
                occupancy.append({"evidence_id": evidence_id, "item_id": item.id, "kind": "occupied"})
    occupancy += [dict(entry, kind="tentative") for entry in tentative]
    travel = sorted({day for plan in plans.values() if plan["service_date"] and plan.get("kind") in TRAVEL
                     for day in stay_days(plan)})
    return {"links": links, "occupancy": {"claims": sorted(occupancy, key=lambda e: (e["evidence_id"], e["item_id"])),
                                          "travel_dates": travel},
            "summary": summary, "policy_hash": policy.sha(), "policy_categories": sorted(policy.categories),
            "policy_limits": policy.limits.model_dump(), "policy_short_names": dict(policy.short_names)}


def stay_days(plan):
    invoice = plan["item"].invoice
    if invoice.service_period is not None:
        start = date.fromisoformat(invoice.service_period.check_in.value)
        return [(start + timedelta(days=offset)).isoformat() for offset in range(invoice.service_period.nights.value + 1)]
    return [plan["service_date"]]
