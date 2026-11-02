"""Dependency-free demo for the localhost prod run (M1): no LLM, no API
keys. One route, one approval-gated tool — the full HITL loop:

  turn 1   message -> gated tool call -> APPROVAL PAUSE (turn suspends)
  decision approve/deny from the console (or CLI)
  turn 2   tool executes (or is denied) -> result message -> sentinel
"""

from collections.abc import AsyncIterator
from uuid import uuid4

from sectr import AgentApp, SessionContext
from sectr.events import (
    MessageDelta,
    MessageEnd,
    MessageStart,
    RunnerEvent,
    ToolCall,
    ToolResult,
)
from sectr.tools import tool


@tool(needs_approval=True)
def dangerous_action(target: str) -> str:
    """A demo gated tool: requires human approval before it runs."""
    return f"done: {target}"


app = AgentApp(name="localhost-demo")


@app.agent_route("/assist")
async def assist(ctx: SessionContext) -> AsyncIterator[RunnerEvent]:
    msg_id = uuid4().hex[:16]

    # Approval resume: the decision arrived; execute (or record the denial),
    # journal the result, say something, exhaust (sentinel ends the turn).
    if ctx.reason["kind"] == "approval_decision":
        pending = ctx.pending_tool_call
        result = (
            await ctx.execute(
                pending["name"], call_id=pending["tool_call_id"], **pending["arguments"]
            )
            if ctx.decision["approved"]
            else "denied by the reviewer"
        )
        yield ToolResult(tool_call_id=pending["tool_call_id"], result=result)
        yield MessageStart(role="assistant", message_id=msg_id)
        yield MessageDelta(
            message_id=msg_id, delta={"type": "text", "text": f"tool result: {result}"}
        )
        yield MessageEnd(message_id=msg_id)
        return

    # First turn: say something, call the gated tool. ctx.execute raises
    # ApprovalRequired (needs_approval=True) -> the adapter journals
    # TOOL_APPROVAL_REQUIRED as the final frame and SUSPENDS the turn.
    yield MessageStart(role="assistant", message_id=msg_id)
    yield MessageDelta(
        message_id=msg_id, delta={"type": "text", "text": "calling the gated tool..."}
    )
    yield MessageEnd(message_id=msg_id)

    yield ToolCall(tool_call_id="call-1", name="dangerous_action", arguments={"target": "demo"})
    result = await ctx.execute("dangerous_action", call_id="call-1", target="demo")
    yield ToolResult(tool_call_id="call-1", result=result)
