"""Support-agent tools — sectr-registered (framework-free), converted to
LangGraph tools in main.py via `as_langgraph_tool`, preserving approval
gating: a gated call pauses the turn, and the adapter executes it once the
approval lands. (LangGraph has no native per-tool HITL without its
checkpointer, which we deliberately do not use — the journal is the
checkpoint.)"""

from sectr.tools import tool


@tool
async def lookup_order(order_id: str) -> dict[str, str]:
    """Look up the status of an order."""
    return {"order_id": order_id, "status": "shipped"}


@tool(needs_approval=True)
async def refund(order_id: str) -> str:
    """Refund an order (requires manager approval)."""
    return f"Refunded {order_id}"


@tool
async def todo_write(items: list[str]) -> str:
    """Replace the todo list. The journaled tool call IS the write: state
    lives in the transcript (the model reads its history directly)."""
    return "ok"


TOOLS = [lookup_order, refund, todo_write]
