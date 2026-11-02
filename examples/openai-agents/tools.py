"""Support-agent tools — 100% vanilla openai-agents code. No sectr imports.

Approval uses the framework's native HITL (`needs_approval`); the adapter
maps `RunResultStreaming.interruptions` -> TOOL_APPROVAL_REQUIRED. The
deployment's policy (`[deploy] approval_tools` in sectr.toml) is unioned in
by the adapter.
"""

from agents import function_tool


@function_tool
async def lookup_order(order_id: str) -> dict[str, str]:
    """Look up the status of an order."""
    return {"order_id": order_id, "status": "shipped"}


@function_tool(needs_approval=True)
async def refund(order_id: str) -> str:
    """Refund an order (requires manager approval)."""
    return f"Refunded {order_id}"


@function_tool
async def todo_write(items: list[str]) -> str:
    """Replace the todo list. The journaled tool call IS the write: state
    lives in the transcript (the model reads its history directly)."""
    return "ok"