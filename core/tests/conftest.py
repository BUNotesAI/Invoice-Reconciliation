"""Test harness: every core call goes through the real `python -m reimb_core` subprocess."""
import copy
import hashlib
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[2]
FIXTURES = REPO / "fixtures"
POLICY = REPO / "policy" / "example.yaml"
ACTOR = "@reimb-linyi:reimb.local"
AT = "2026-11-02T01:00:00Z"


def expected(name):
    return json.loads((FIXTURES / "expected" / name).read_text(encoding="utf-8"))


def canonical_hash(value):
    # The orchestrator computes the snapshot hash the same way the spec defines it.
    data = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(data).hexdigest()


class Runtime:
    """One private data root with a policy copy, an upload area and one batch directory."""

    def __init__(self, root, batch="batch"):
        self.root = root
        self.policy = root / "policy.yaml"
        self.uploads = root / "uploads"
        self.batch = root / batch
        self.uploads.mkdir(parents=True, exist_ok=True)
        if not self.policy.exists():
            shutil.copyfile(POLICY, self.policy)

    def call(self, command, payload, request_id="req-1", raw=None, argv=None, env=None):
        envelope = {"schema_version": 1, "command": command, "request_id": request_id,
                    "batch_dir": str(self.batch), "policy_path": str(self.policy), "input": payload}
        data = raw if raw is not None else json.dumps(envelope, ensure_ascii=False).encode()
        environment = {"PATH": os.environ.get("PATH", ""), "PYTHONPATH": str(REPO / "core"), "REIMB_DATA": str(self.root)}
        environment.update(env or {})
        process = subprocess.run([sys.executable, "-m", "reimb_core", *(argv if argv is not None else [command])],
                                 input=data, capture_output=True, timeout=60, env={k: v for k, v in environment.items() if v is not None})
        assert process.stdout.endswith(b"\n") and process.stdout.count(b"\n") == 1, process.stdout[:200]
        response = json.loads(process.stdout)
        assert response["schema_version"] == 1
        return process.returncode, response

    def ok(self, command, payload):
        code, response = self.call(command, payload)
        assert code == 0 and response["ok"], response
        return response["result"]

    def fails(self, command, payload, code, exit_code):
        status, response = self.call(command, payload)
        assert (status, response["ok"], response["error"]["code"]) == (exit_code, False, code), response
        return response["error"]

    def upload(self, source, name=None):
        target = self.uploads / (name or source.name)
        shutil.copyfile(source, target)
        return target

    def ingest(self, path, name=None):
        return self.ok("ingest", {"source_path": str(path), "original_name": name or path.name})["source_file"]


@pytest.fixture
def runtime(tmp_path):
    return Runtime(tmp_path / "data")


def confirm(candidate, corrections, event_id, actor=ACTOR, at=AT):
    """Orchestrator step after the user checked a vision reading: every field becomes a confirmed fact."""
    invoice = copy.deepcopy(candidate)
    for name, fact in list(invoice.items()):
        if not isinstance(fact, dict) or fact.get("level") != "candidate":
            continue
        value = corrections.get(name, fact["value"])
        invoice[name] = {
            "id": fact["id"].removesuffix(".candidate") + ".confirmed", "value": value, "level": "confirmed",
            "source": {"file_sha256": fact["source"]["file_sha256"], "method": "user",
                       "locator": {"type": "user_confirmation", "event_id": event_id, "actor": actor,
                                   "confirmed_at": at, "original_fact_id": fact["id"]}},
            "validation_results": ["user_checked"], "candidate_id": fact["id"], "confirmed_by": actor,
            "confirmation_event": event_id, "confirmed_at": at}
    return invoice


def confirmed_fact_ids(invoice):
    return sorted(fact["id"] for fact in invoice.values() if isinstance(fact, dict) and fact.get("level") == "confirmed")


def decision(identifier, item_id, kind, payload, revision=5, event=None, actor=ACTOR):
    return {"id": identifier, "item_id": item_id, "kind": kind, "payload": payload, "actor": actor, "at": AT,
            "expected_revision": revision, "source_event_id": event or f"event-{identifier}"}


def item(item_id, invoice, service_date, category, short_name, detail, decision_ids=(), replaces=None):
    return {"id": item_id, "invoice": invoice, "service_date": service_date, "category": category,
            "short_name": short_name, "expense_detail": detail, "decision_ids": list(decision_ids),
            "replaces_invoice_no": replaces}


def snapshot(runtime_policy_hash, history_hash, items, decisions, batch_id="batch-test", applicant="林一",
             period="2026-10", revision=7):
    return {"batch_id": batch_id, "revision": revision, "applicant": applicant, "period": period,
            "policy_hash": runtime_policy_hash, "history_hash": history_hash, "items": items, "decisions": decisions}
