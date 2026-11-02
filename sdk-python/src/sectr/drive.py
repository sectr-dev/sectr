"""Turn-stream driver: turns a handler's async generator into the runner
event stream, enforcing the contract's lifecycle rules.

Shared by `sectr.server` (SSE transport) and, later, framework adapters.
The rules (see `schema/README.md`, "Turn semantics"):

- The turn ends when `NO_MORE_ACTIONS` is emitted. The driver appends it on
  generator exhaustion; a handler may also yield it explicitly to end the
  turn early.
- An approval pause SUSPENDS the turn: `ApprovalRequired` escaping the
  handler (or a yielded `ToolApprovalRequired`) becomes the final
  TOOL_APPROVAL_REQUIRED frame — and deliberately NO sentinel, because the
  gated tool is a pending action.
- Anything yielded after `ToolApprovalRequired` is a handler error (ERROR +
  sentinel).
- An `Error` event ends the turn: ERROR + sentinel.
- An unhandled exception becomes ERROR + sentinel.
- An `approval_decision` turn whose transcript holds no pending tool call
  is ERROR + sentinel before the handler runs: there is nothing to resume,
  and running the handler anyway would silently generate a fresh reply.
  The platform refuses such decisions (409), so this guards direct callers
  and future drift between the Rust and Python `pending_tool_call`.
"""

from __future__ import annotations

import uuid
from collections.abc import AsyncIterator
from typing import Any

from .app import Handler
from .context import SessionContext
from .events import Error, NoMoreActions, RunnerEvent, ToolApprovalRequired
from .tools import ApprovalRequired

__all__ = ["drive"]


def _frame(event: RunnerEvent) -> dict[str, Any]:
    return event.to_runner_event()


async def drive(handler: Handler, ctx: SessionContext) -> AsyncIterator[dict[str, Any]]:
    if ctx.reason.get("kind") == "approval_decision" and ctx.pending_tool_call is None:
        yield _frame(
            Error(
                message="approval_decision turn, but the transcript has no pending "
                "tool call to resume",
                code="no_pending_tool_call",
            )
        )
        yield _frame(NoMoreActions())
        return
    gen = handler(ctx)
    awaiting = False
    try:
        async for event in gen:
            if not isinstance(event, RunnerEvent):
                raise TypeError(
                    f"handler yielded {type(event).__name__!r}, not a sectr event"
                )
            if isinstance(event, NoMoreActions):
                yield _frame(event)
                return
            if isinstance(event, ToolApprovalRequired):
                if awaiting:
                    raise RuntimeError(
                        "handler yielded TOOL_APPROVAL_REQUIRED twice in one turn"
                    )
                awaiting = True
                yield _frame(event)
            elif isinstance(event, Error):
                yield _frame(event)
                yield _frame(NoMoreActions())
                return
            elif awaiting:
                raise RuntimeError(
                    "handler yielded after TOOL_APPROVAL_REQUIRED — the turn is "
                    "suspended there (finality violation)"
                )
            else:
                yield _frame(event)
    except ApprovalRequired as exc:
        # Suspension, NOT turn end: no sentinel on purpose.
        yield _frame(
            ToolApprovalRequired(
                tool_call_id=exc.call_id or uuid.uuid4().hex[:16],
                name=exc.name,
                arguments=exc.arguments,
                reason=exc.reason,
            )
        )
        return
    except Exception as exc:  # noqa: BLE001 — the journal needs the failure
        yield _frame(Error(message=str(exc) or type(exc).__name__))
        yield _frame(NoMoreActions())
        return
    if not awaiting:
        # Normal exhaustion: the turn is over. (If awaiting, the stream just
        # ends — the turn stays suspended until the decision's invocation.)
        yield _frame(NoMoreActions())
