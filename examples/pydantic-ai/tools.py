"""Support-agent tools — plain functions, zero framework and zero sectr
imports. main.py registers them on the Agent; `refund` is marked
`requires_approval=True` there (Pydantic AI's native deferred tools), so
this module stays reusable anywhere."""


async def lookup_order(order_id: str) -> dict[str, str]:
    """Look up the status of an order."""
    return {"order_id": order_id, "status": "shipped"}


async def refund(order_id: str) -> str:
    """Refund an order (requires manager approval)."""
    return f"Refunded {order_id}"


async def todo_write(items: list[str]) -> str:
    """Replace the todo list. The journaled tool call IS the write: state
    lives in the transcript (the model reads its history directly)."""
    return "ok"
