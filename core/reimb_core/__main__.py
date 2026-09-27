"""One bounded JSON envelope per subprocess invocation."""
import logging
import os
from pathlib import Path
import sys
from pydantic import ValidationError

from .errors import CoreError, require
from .extract import extract
from .gates import gates
from .history import project, validated_history
from .package import package
from .policy import load_policy
from .storage import Store, atomic_write, inside
from .values import canonical, strict_json
from .verify import verify

COMMANDS = {"ingest", "extract", "gates", "evidence", "link", "missing", "history", "package", "verify", "claim"}
FIELDS = {
    "ingest": ({"source_path", "original_name"}, set()),
    "extract": ({"source_file_id"}, {"vision_candidate"}),
    "gates": ({"invoices", "history_snapshot"}, set()),
    "history": ({"action"}, {"entries", "events"}),
    "package": ({"confirmed_snapshot", "expected_snapshot_hash"}, set()),
    "verify": ({"snapshot_hash", "manifest_object_id", "history_snapshot"}, set()),
}


def dispatch(envelope, command):
    require(isinstance(envelope, dict) and set(envelope) ==
            {"schema_version", "command", "request_id", "batch_dir", "policy_path", "input"}, message="Invalid envelope fields")
    require(type(envelope["schema_version"]) is int and envelope["schema_version"] == 1,
            "UNSUPPORTED_VERSION", "Unsupported schema version")
    require(envelope["command"] == command and command in COMMANDS, "UNKNOWN_COMMAND", "Unknown or mismatched command")
    require(isinstance(envelope["request_id"], str) and 0 < len(envelope["request_id"]) <= 128,
            message="Invalid request identifier")
    require(command in FIELDS, "UNKNOWN_COMMAND", "Command is not available in this phase")
    payload = envelope["input"]
    required, optional = FIELDS[command]
    require(isinstance(payload, dict) and required <= set(payload) <= required | optional, message="Invalid command fields")
    root_value = os.environ.get("REIMB_DATA")
    require(root_value is not None and Path(root_value).is_absolute(), "INVALID_PATH", "REIMB_DATA is required")
    root = Path(root_value)
    require(not root.is_symlink(), "INVALID_PATH", "Symbolic runtime root")
    store = Store(envelope["batch_dir"], root)
    # Policy is a configured local file, not a path accepted from HTTP callers.
    policy_path = Path(envelope["policy_path"])
    require(policy_path.is_absolute() and policy_path.is_file() and not policy_path.is_symlink(),
            "INVALID_PATH", "Invalid policy file")
    policy = load_policy(policy_path)
    if command == "ingest":
        return store.ingest(**payload)
    if command == "extract":
        return extract(store, **payload)
    if command == "gates":
        return gates(policy=policy, **payload)
    if command == "history":
        require(payload["action"] in ("validate_import", "project"), message="Invalid history action")
        key = "entries" if payload["action"] == "validate_import" else "events"
        require(set(payload) == {"action", key}, message="Invalid history command fields")
        # validate_import checks imported entries; project folds a complete appended event log.
        result = validated_history(payload[key]) if key == "entries" else project(payload[key])
        target = inside(store.batch / "history" / (result["history_hash"] + ".json"), store.root, must_exist=False)
        atomic_write(target, canonical(result["validated_snapshot"]))
        return result
    if command == "package":
        return package(store, policy, **payload)
    return verify(store, policy, **payload)


def main():
    os.umask(0o077)
    logging.getLogger("pypdf").disabled = True
    request_id = None
    try:
        raw = sys.stdin.buffer.read(4 * 1024 * 1024 + 1)
        require(len(raw) <= 4 * 1024 * 1024, "INPUT_TOO_LARGE", "Request size limit exceeded")
        envelope = strict_json(raw)
        if isinstance(envelope, dict) and isinstance(envelope.get("request_id"), str) and len(envelope["request_id"]) <= 128:
            request_id = envelope["request_id"]
        require(len(sys.argv) == 2, "UNKNOWN_COMMAND", "Exactly one command is required")
        result = dispatch(envelope, sys.argv[1])
        response = {"schema_version": 1, "request_id": request_id, "ok": True, "result": result}
        data = canonical(response)
        require(len(data) <= 8 * 1024 * 1024, "CORE_OUTPUT_LIMIT", "Response size limit exceeded", 4)
        code = 0
    except Exception as error:
        if isinstance(error, ValidationError):
            error = CoreError("INVALID_SCHEMA", "Invalid record schema")
        elif isinstance(error, (TypeError, ValueError)):
            error = CoreError("INVALID_SCHEMA", "Invalid input type")
        elif isinstance(error, OSError):
            error = CoreError("IO_ERROR", "Local file operation failed", 4)
        elif not isinstance(error, CoreError):
            error = CoreError("INTERNAL_ERROR", "Core operation failed", 4)
        data = canonical({"schema_version": 1, "request_id": request_id, "ok": False,
                          "error": {"code": error.code, "message": error.message, "retriable": error.retriable}})
        code = error.exit_code
    sys.stdout.buffer.write(data + b"\n")
    return code


if __name__ == "__main__":
    raise SystemExit(main())
