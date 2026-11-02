"""The beta subagent contract, as checks every adapter's subagent
tests share. A subagent is another agent the outer one runs from a tool, in
whatever shape the framework allows (`as_tool()`, a tool running a graph or
a delegate agent, a hand-written `Runner.run`, a handoff…).

- Opaque: the turn's frames hold the outer tool's call and result, never
  the subagent's own messages, calls or results (`assert_opaque`).
- A gated tool inside a subagent (in code, or named by
  `SECTR_APPROVAL_POLICY`) fails loudly with `SUBAGENT_GATED_TOOL` and never
  runs (`assert_fails_loudly`). The adapter does that at construction or at
  turn time, never both; the check accepts either.

The conformance cases (`crates/sectr-conformance/src/scenarios/subagent.rs`)
check the same contract through the platform, on one shape per framework.
"""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

from sectr.app import Handler
from sectr.context import SessionContext
from sectr.drive import drive

SUBAGENT_GATED_TOOL = "subagent_gated_tool"


def user_turn(text: str) -> SessionContext:
    return SessionContext(transcript=[], reason={"kind": "user_message", "input": text})


async def run_turn(handler: Handler, text: str) -> list[dict[str, Any]]:
    return [frame async for frame in drive(handler, user_turn(text))]


def error_codes(frames: list[dict[str, Any]]) -> list[str | None]:
    return [f["payload"].get("code") for f in frames if f["type"] == "ERROR"]


async def assert_fails_loudly(build: Callable[[], Handler], text: str, ran: list[Any]) -> None:
    """Building the handler raises with `SUBAGENT_GATED_TOOL` in its message,
    or the turn for `text` ends in an `ERROR` with that code. Either way the
    gated tool never ran (`ran` is what it appends to when it runs)."""
    try:
        handler = build()
    except Exception as exc:  # noqa: BLE001 - any construction error, checked below
        assert SUBAGENT_GATED_TOOL in str(exc), f"rejected at construction without the code: {exc!r}"
        assert ran == []
        return
    frames = await run_turn(handler, text)
    assert ran == [], "the gated tool inside the subagent ran"
    types = [f["type"] for f in frames]
    assert SUBAGENT_GATED_TOOL in error_codes(frames), f"no ERROR with the code; frames: {types}"
    assert "TOOL_APPROVAL_REQUIRED" not in types


def assert_opaque(frames: list[dict[str, Any]], outer_tool: str, inner_text: str) -> None:
    """The turn completed, and its only tool call is `outer_tool`'s: none of
    the subagent's own calls, and none of its text (`inner_text`, something
    only the subagent's model says) in a message."""
    types = [f["type"] for f in frames]
    assert "ERROR" not in types and "TOOL_APPROVAL_REQUIRED" not in types, types
    calls = [f["payload"]["name"] for f in frames if f["type"] == "TOOL_CALL"]
    assert calls == [outer_tool], f"tool calls journaled: {calls}"
    said = "".join(
        f["payload"]["delta"].get("text", "") for f in frames if f["type"] == "MESSAGE_DELTA"
    )
    assert inner_text not in said, f"the subagent's text reached the journal: {said!r}"
