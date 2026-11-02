"""Tests for sectr.inspect: manifest extraction by real import.

Each test writes a small module to a tmp dir and imports it — the same
mechanism the deploy flow uses (scan mode locally, authoritative mode
inside the built image)."""

from __future__ import annotations

import importlib
import sys
import textwrap
from pathlib import Path

import pytest

from sectr import AgentApp
from sectr.inspect import build_manifest, main

MODULE_SRC = '''
from sectr import AgentApp

app = AgentApp(name="support-agent", env=["OPENROUTER_API_KEY"])

@app.agent_route("/chat")
async def chat(ctx):
    yield []
'''

MULTI_APP_SRC = MODULE_SRC + "\napp2 = AgentApp(name='second')\n"


class TempModule:
    """Write a module file into tmp_path (FLAT — a subdirectory would be
    picked up as a namespace package) and make it importable; cleans up
    sys.path and sys.modules on exit."""

    def __init__(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, name: str, src: str
    ) -> None:
        self.name = name
        self.path = tmp_path
        (tmp_path / f"{name}.py").write_text(textwrap.dedent(src))
        monkeypatch.syspath_prepend(str(tmp_path))
        importlib.invalidate_caches()

    def cleanup(self, monkeypatch: pytest.MonkeyPatch) -> None:
        try:
            sys.path.remove(str(self.path))  # all modules in a test share tmp_path
        except ValueError:
            pass
        monkeypatch.delitem(sys.modules, self.name, raising=False)


@pytest.fixture()
def temp_module(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    created: list[TempModule] = []

    def factory(name: str, src: str) -> TempModule:
        mod = TempModule(tmp_path, monkeypatch, name, src)
        created.append(mod)
        return mod

    yield factory
    for mod in created:
        mod.cleanup(monkeypatch)


def test_manifest_by_attribute(temp_module) -> None:
    temp_module("exapp", MODULE_SRC)
    manifest = build_manifest("exapp:app")
    assert manifest == {
        "name": "support-agent",
        "entry": "exapp:app",
        "python": "3.12",
        "routes": [{"path": "/chat"}],
        "env": ["OPENROUTER_API_KEY"],
    }


def test_manifest_by_bare_module_single_app(temp_module) -> None:
    temp_module("exsingle", MODULE_SRC)
    manifest = build_manifest("exsingle")
    assert manifest["name"] == "support-agent"
    assert manifest["entry"] == "exsingle"


def test_bare_module_with_two_apps_is_an_error(temp_module) -> None:
    temp_module("exmulti", MULTI_APP_SRC)
    with pytest.raises(ValueError, match="multiple AgentApps"):
        build_manifest("exmulti")


def test_bare_module_without_app_is_an_error(temp_module) -> None:
    temp_module("exempty", "x = 1\n")
    with pytest.raises(ValueError, match="no AgentApp found"):
        build_manifest("exempty")


def test_missing_attribute_is_an_error(temp_module) -> None:
    temp_module("exmissing", MODULE_SRC)
    with pytest.raises(ValueError, match="no attribute 'nope'"):
        build_manifest("exmissing:nope")


def test_non_app_attribute_is_an_error(temp_module) -> None:
    temp_module("exwrong", "app = 42\n")
    with pytest.raises(TypeError, match="not an AgentApp"):
        build_manifest("exwrong:app")


def test_invalid_entry_string() -> None:
    with pytest.raises(ValueError, match="invalid entry"):
        build_manifest(":app")


def test_route_validation_rejects_bad_paths() -> None:
    app = AgentApp()
    with pytest.raises(ValueError, match="must start with '/'"):
        app.add_route("chat", lambda ctx: None)  # type: ignore[arg-type]


# ---- CLI modes ---------------------------------------------------------


def test_scan_mode_main(temp_module, capsys: pytest.CaptureFixture[str]) -> None:
    temp_module("exscan", MODULE_SRC)
    code = main(["exscan:app"])
    out = capsys.readouterr().out
    assert code == 0
    assert '"routes"' in out and '"/chat"' in out


def test_authoritative_mode_via_env(
    temp_module, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    temp_module("exauth", MODULE_SRC)
    monkeypatch.setenv("SECTR_MANIFEST", "1")
    monkeypatch.setenv("SECTR_ENTRY", "exauth:app")
    assert main([]) == 0
    out = capsys.readouterr().out
    assert '"env"' in out and '"OPENROUTER_API_KEY"' in out


def test_authoritative_mode_without_entry_fails(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setenv("SECTR_MANIFEST", "1")
    monkeypatch.delenv("SECTR_ENTRY", raising=False)
    assert main([]) == 2


def test_import_failure_is_reported_not_raised(
    capsys: pytest.CaptureFixture[str]
) -> None:
    assert main(["definitely_not_importable_xyz:app"]) == 1
    assert "sectr inspect failed" in capsys.readouterr().err


def test_no_args_shows_usage(capsys: pytest.CaptureFixture[str]) -> None:
    assert main([]) == 2
    assert "usage:" in capsys.readouterr().err


def test_pydantic_ai_example_manifest_is_import_safe_without_a_key(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """`sectr inspect` imports the example before secrets are injected: with
    no provider key the module must still import and record its framework."""
    example = Path(__file__).resolve().parents[2] / "examples" / "pydantic-ai"
    for var in ("SECTR_FAKE", "OPENROUTER_API_KEY", "SECTR_MODEL"):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.syspath_prepend(str(example))
    modules = ("main", "tools", "fake")
    for name in modules:
        monkeypatch.delitem(sys.modules, name, raising=False)
    try:
        manifest = build_manifest("main:app")
    finally:
        for name in modules:
            sys.modules.pop(name, None)
    assert manifest == {
        "name": "support-agent",
        "entry": "main:app",
        "python": f"{sys.version_info.major}.{sys.version_info.minor}",
        "routes": [{"path": "/chat", "framework": "pydantic-ai"}],
        "env": ["OPENROUTER_API_KEY"],
    }
