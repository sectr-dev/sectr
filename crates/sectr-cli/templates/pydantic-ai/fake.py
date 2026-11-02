"""Deterministic scripted model — what answers when no OPENAI_API_KEY is
set, so `sectr dev` works with ZERO env vars. A plain Pydantic AI
`FunctionModel` (public testing API); no sectr imports.

Each `sectr dev` invocation is a FRESH process, so the script cannot be a
fixed sequence: every model call is classified from the conversation it
receives (the same rules a real model follows, scripted):

- a `refund` call already in the conversation → the approval decision
  landed; emit the closing text.
- no tool calls at all → first turn: text + `lookup_order` call.
- the last request is a user message mentioning "refund" → the gated
  `refund` call (no text, like many real models).
- otherwise → close the lookup turn.

Like a real provider, the model REJECTS a conversation holding an
unanswered tool call — a fake that tolerated one would hide resume bugs.
Text streams in several chunks and replies carry a thinking part, so the
adapter's delta and reasoning mappings are exercised.
"""

from __future__ import annotations

import json
from collections.abc import AsyncIterator
from typing import Any

from pydantic_ai.messages import (
    ModelMessage,
    ModelRequest,
    RetryPromptPart,
    ToolCallPart,
    ToolReturnPart,
    UserPromptPart,
)
from pydantic_ai.models.function import AgentInfo, DeltaThinkingPart, DeltaToolCall, FunctionModel

__all__ = ["scenario_model"]


def _check_answered(messages: list[ModelMessage]) -> None:
    pending: list[str] = []
    for m in messages:
        if isinstance(m, ModelRequest):
            for p in m.parts:
                if isinstance(p, (ToolReturnPart, RetryPromptPart)) and p.tool_call_id in pending:
                    pending.remove(p.tool_call_id)
            if pending:
                raise ValueError(f"tool calls {pending} are unanswered before the next request")
        else:
            if pending:
                raise ValueError(f"tool calls {pending} are unanswered before the next response")
            pending = [p.tool_call_id for p in m.parts if isinstance(p, ToolCallPart)]
    if pending:
        raise ValueError(f"tool calls {pending} are unanswered at the end of the conversation")


def _words(text: str) -> list[str]:
    words = text.split(" ")
    return [w + (" " if i < len(words) - 1 else "") for i, w in enumerate(words)]


async def _stream(messages: list[ModelMessage], info: AgentInfo) -> AsyncIterator[Any]:
    _check_answered(messages)
    calls = [p for m in messages if not isinstance(m, ModelRequest) for p in m.parts if isinstance(p, ToolCallPart)]
    last = messages[-1]
    last_user = ""
    if isinstance(last, ModelRequest):
        last_user = " ".join(str(p.content) for p in last.parts if isinstance(p, UserPromptPart))

    text = ""
    call: tuple[str, str, dict[str, str]] | None = None
    if any(c.tool_name == "refund" for c in calls):
        text = "Refund processed — anything else?"
    elif not calls:
        text, call = "Let me check that order.", ("c_lookup", "lookup_order", {"order_id": "123"})
    elif "refund" in last_user.lower():
        call = ("c_refund", "refund", {"order_id": "123"})
    else:
        text = "Good news — order 123 shipped!"

    if text:
        yield {0: DeltaThinkingPart(content="Working out the reply.")}
        for chunk in _words(text):
            yield chunk
    if call is not None:
        call_id, name, args = call
        yield {1: DeltaToolCall(name=name, json_args=json.dumps(args), tool_call_id=call_id)}


def scenario_model() -> FunctionModel:
    return FunctionModel(stream_function=_stream)
