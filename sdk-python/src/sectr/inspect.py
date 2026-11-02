"""Manifest extraction.

Two modes:

1. `SECTR_MANIFEST=1` (set by the platform's build step): the entry module is
   imported INSIDE the built runner image, the `AgentApp` is discovered, and
   the AUTHORITATIVE manifest is printed as JSON, exit 0. Real import — no
   AST parsing — because route registration only happens at runtime.
2. CLI scan (`python -m sectr.inspect <entry>`): the same manifest extracted
   locally with the user's own Python, used by `sectr deploy` for
   pre-upload validation and feedback.

Entry format: `module:attribute` (e.g. `main:app`); bare `module` resolves
to a single `AgentApp` attribute in the module (error if zero or several).

The manifest declares routes, app name, python version, and env var NAMES
(never values — the sidecar injects values from the secret store at spawn).
Importing user code runs it; that is true of every real discovery mechanism
and of the runner itself, which is why the authoritative import happens
inside the isolated built image.
"""

from __future__ import annotations

import ast
import importlib
import json
import os
import sys
from collections.abc import Callable
from typing import Any

from .app import AgentApp

__all__ = ["build_manifest", "main", "scan_interrupt_imports"]


def _resolve_app(entry: str) -> AgentApp:
    """Import `module[:attribute]` and return the AgentApp it points at."""
    module_name, sep, attr_name = entry.partition(":")
    if not module_name or " " in module_name:
        raise ValueError(
            f"invalid entry {entry!r} — expected 'module:attribute' (e.g. 'main:app')"
        )
    module = importlib.import_module(module_name)

    if sep:
        if not attr_name:
            raise ValueError(f"invalid entry {entry!r} — empty attribute name")
        try:
            candidate = getattr(module, attr_name)
        except AttributeError:
            raise ValueError(
                f"module {module_name!r} has no attribute {attr_name!r}"
            ) from None
        return _as_agent_app(candidate, entry)

    # Bare module: exactly one AgentApp must be present.
    candidates = [v for v in vars(module).values() if isinstance(v, AgentApp)]
    if len(candidates) == 1:
        return candidates[0]
    if not candidates:
        raise ValueError(
            f"no AgentApp found in module {module_name!r} — "
            "entry should be 'module:attribute' (e.g. 'main:app')"
        )
    raise ValueError(
        f"multiple AgentApps found in module {module_name!r} — "
        "entry must be 'module:attribute'"
    )


def _as_agent_app(candidate: Any, entry: str) -> AgentApp:
    if not isinstance(candidate, AgentApp):
        raise TypeError(
            f"entry {entry!r} resolved to {type(candidate).__name__!r}, not an AgentApp"
        )
    return candidate


_NATIVE_INTERRUPTS = frozenset({"langgraph.types.interrupt", "langchain.agents.interrupt"})


def _dotted(node: ast.expr) -> str | None:
    parts: list[str] = []
    while isinstance(node, ast.Attribute):
        parts.append(node.attr)
        node = node.value
    if not isinstance(node, ast.Name):
        return None
    parts.append(node.id)
    return ".".join(reversed(parts))


def scan_interrupt_imports(source: str, filename: str) -> list[str]:
    """Warnings for uses of LangGraph's native `interrupt()` in `source`.

    sectr runs graphs without a checkpointer, so a native interrupt cannot
    be resumed (the adapter also fails loudly at runtime; this is the early,
    static signal). Flags `from langgraph.types import interrupt` (and the
    `langchain.agents` spelling) at the import, and attribute uses such as
    `langgraph.types.interrupt(...)` through any import alias."""
    try:
        tree = ast.parse(source)
    except SyntaxError:
        return []

    aliases: dict[str, str] = {}  # local name -> dotted path it is bound to
    lines: set[int] = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for alias in node.names:
                if alias.asname:
                    aliases[alias.asname] = alias.name
                else:
                    root = alias.name.split(".")[0]
                    aliases[root] = root
        elif isinstance(node, ast.ImportFrom) and node.module and node.level == 0:
            for alias in node.names:
                full = f"{node.module}.{alias.name}"
                aliases[alias.asname or alias.name] = full
                if full in _NATIVE_INTERRUPTS:
                    lines.add(node.lineno)
    for node in ast.walk(tree):
        if isinstance(node, ast.Attribute) and (dotted := _dotted(node)):
            root, _, rest = dotted.partition(".")
            if root in aliases and f"{aliases[root]}.{rest}" in _NATIVE_INTERRUPTS:
                lines.add(node.lineno)
    return [
        f"{filename}:{line}: uses langgraph's native interrupt(), which sectr cannot resume "
        "(no checkpointer) — gate tools with sectr.adapters.langgraph.as_langgraph_tool "
        "(needs_approval=True) instead"
        for line in sorted(lines)
    ]


def _interrupt_warnings(entry: str) -> list[str]:
    """Scan the entry module and the user modules next to it (already
    imported by `_resolve_app`) for native-interrupt uses."""
    entry_module = sys.modules.get(entry.partition(":")[0])
    entry_file = getattr(entry_module, "__file__", None)
    if not entry_file:
        return []
    root = os.path.dirname(os.path.abspath(entry_file))
    files = [os.path.abspath(entry_file)]
    for module in list(sys.modules.values()):
        path = getattr(module, "__file__", None)
        if not path or not path.endswith(".py"):
            continue
        path = os.path.abspath(path)
        if (
            path not in files
            and path.startswith(root + os.sep)
            and "site-packages" not in path
            and ".venv" not in path
        ):
            files.append(path)
    warnings: list[str] = []
    for path in files:
        try:
            with open(path, encoding="utf-8") as f:
                source = f.read()
        except OSError:
            continue
        warnings.extend(scan_interrupt_imports(source, os.path.relpath(path, root)))
    return warnings


def build_manifest(entry: str) -> dict[str, Any]:
    """Import `module[:attribute]` and build the manifest dict.

    Importing executes the user's module (route registration is the point).
    Routes come from the AgentApp registry; the python version reflects the
    interpreter running this inspection — authoritative when it happens
    inside the built runner image. `warnings` (static-scan findings, e.g.
    LangGraph's native `interrupt()`) is present only when non-empty."""
    app = _resolve_app(entry)
    warnings = _interrupt_warnings(entry)
    return {
        "name": app.name,
        "entry": entry,
        "python": f"{sys.version_info.major}.{sys.version_info.minor}",
        # framework: the handler's `sectr_framework` marker, when the adapter
        # sets one (absent for bare handlers — not every handler is an
        # adapter product).
        "routes": [
            {"path": route.path, **({"framework": fw} if (fw := getattr(route.handler, "sectr_framework", None)) else {})}
            for route in app.routes
        ],
        "env": list(app.env),
        **({"warnings": warnings} if warnings else {}),
    }


def main(argv: list[str] | None = None) -> int:
    argv = sys.argv[1:] if argv is None else argv

    if os.environ.get("SECTR_MANIFEST"):
        entry = os.environ.get("SECTR_ENTRY")
        if not entry:
            print("SECTR_MANIFEST is set but SECTR_ENTRY is missing", file=sys.stderr)
            return 2
        return _run(lambda: build_manifest(entry))
    if argv:
        return _run(lambda: build_manifest(argv[0]))

    print(
        "usage: python -m sectr.inspect <module:attribute>\n"
        "       (or set SECTR_MANIFEST=1 + SECTR_ENTRY=module:attribute)",
        file=sys.stderr,
    )
    return 2


def _run(build: Callable[[], dict[str, Any]]) -> int:
    try:
        manifest = build()
    except Exception as exc:  # noqa: BLE001 — CLI boundary: report, exit 1
        print(f"sectr inspect failed: {exc}", file=sys.stderr)
        return 1
    for warning in manifest.get("warnings", []):
        print(f"sectr inspect warning: {warning}", file=sys.stderr)
    # SECTR_MANIFEST_OUT: write the JSON to a file instead of stdout — the
    # server's build step reads the authoritative manifest from the file
    # because the user module's import may print to stdout (route banners,
    # framework logs), which would corrupt stream parsing.
    out = os.environ.get("SECTR_MANIFEST_OUT")
    if out:
        with open(out, "w") as f:
            json.dump(manifest, f, indent=2)
        return 0
    print(json.dumps(manifest, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
