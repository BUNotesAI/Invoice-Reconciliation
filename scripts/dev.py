#!/usr/bin/env python3
"""Start the isolated P0 environment and run the Rust bot in the foreground."""
import os
import subprocess

from dev_env import DATA, ROOT, main as provision


if __name__ == "__main__":
    provision()
    environment = dict(os.environ, REIMB_DATA=str(DATA), NO_PROXY="127.0.0.1,localhost", no_proxy="127.0.0.1,localhost")
    result = subprocess.run(["cargo", "run", "--locked", "--manifest-path", str(ROOT / "bot/Cargo.toml")], env=environment)
    raise SystemExit(result.returncode)
