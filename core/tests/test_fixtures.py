"""Fixture generation is byte-stable and independent of the hand-written answers."""
import ast
import hashlib
import subprocess
import sys

from conftest import FIXTURES


def tree_hashes(root):
    return {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted(root.rglob("*")) if path.is_file()}


def generate(target):
    subprocess.run([sys.executable, str(FIXTURES / "generate.py"), str(target)], check=True, timeout=120)
    return tree_hashes(target)


def test_two_generations_are_identical_and_match_the_committed_fixtures(tmp_path):
    first, second = generate(tmp_path / "a"), generate(tmp_path / "b")
    assert first == second
    committed = {name: digest for name, digest in tree_hashes(FIXTURES).items()
                 if name.startswith(("demo/", "edge/"))}
    assert committed == first


def test_generator_does_not_read_expected_answers():
    source = (FIXTURES / "generate.py").read_text(encoding="utf-8")
    tree = ast.parse(source)
    imported = {alias.name for node in ast.walk(tree) if isinstance(node, (ast.Import, ast.ImportFrom))
                for alias in node.names} | {node.module for node in ast.walk(tree) if isinstance(node, ast.ImportFrom)}
    assert not any(name and ("reimb_core" in name or "expected" in name) for name in imported)
    docstring = ast.get_docstring(tree, clean=False)
    strings = [node.value for node in ast.walk(tree) if isinstance(node, ast.Constant) and isinstance(node.value, str)
               and node.value != docstring]
    assert not any("expected" in value for value in strings)
