"""Deterministic scripted chat model for offline CI — selected by
`SECTR_FAKE=1` in `main.py`. A plain LangChain `BaseChatModel` (LangGraph
runs against any chat model); no sectr imports — this file is example-local,
exactly what a user without a provider key would write.

Each `sectr dev` invocation is a FRESH process, so the script cannot be a
fixed sequence: every model call is classified from the conversation it
receives (the same rules a real model follows, scripted):

- refund call already in the conversation → the approval decision landed;
  emit the closing text.
- no tool calls at all → first turn: text + `lookup_order` call.
- the last message is a user message mentioning "refund" → the gated
  `refund` call (no text, like many real models).
- otherwise → close the lookup turn.

Like a real provider, the model REJECTS a conversation holding an
unanswered tool call — a fake that tolerated one would hide resume bugs.
"""

from __future__ import annotations

import json
from collections.abc import AsyncIterator
from typing import Any

from langchain_core.language_models.chat_models import BaseChatModel
from langchain_core.messages import (
    AIMessage,
    AIMessageChunk,
    BaseMessage,
    HumanMessage,
    ToolMessage,
)
from langchain_core.outputs import ChatGeneration, ChatGenerationChunk, ChatResult

__all__ = ["scenario_model"]

Reply = tuple[str, list[tuple[str, str, dict[str, Any]]]]  # (text, [(call_id, name, args)])


def _check_answered(messages: list[BaseMessage]) -> None:
    pending: list[str] = []
    for m in messages:
        if isinstance(m, ToolMessage):
            if m.tool_call_id in pending:
                pending.remove(m.tool_call_id)
            continue
        if pending:
            raise ValueError(f"tool calls {pending} are unanswered before the next message")
        if isinstance(m, AIMessage) and m.tool_calls:
            pending = [str(c["id"]) for c in m.tool_calls]
    if pending:
        raise ValueError(f"tool calls {pending} are unanswered at the end of the conversation")


def _classify(messages: list[BaseMessage]) -> Reply:
    _check_answered(messages)
    calls = [c for m in messages if isinstance(m, AIMessage) for c in m.tool_calls]
    if any(c["name"] == "refund" for c in calls):
        return "Refund processed — anything else?", []
    if not calls:
        return "Let me check that order.", [("c_lookup", "lookup_order", {"order_id": "123"})]
    last = messages[-1]
    if isinstance(last, HumanMessage) and "refund" in str(last.content).lower():
        return "", [("c_refund", "refund", {"order_id": "123"})]
    return "Good news — order 123 shipped!", []


class ScenarioChatModel(BaseChatModel):
    @property
    def _llm_type(self) -> str:
        return "sectr-scenario"

    def bind_tools(self, tools: Any, **kwargs: Any) -> Any:
        return self  # the script already knows which tools exist

    def _generate(
        self, messages: list[BaseMessage], stop: Any = None, run_manager: Any = None, **kwargs: Any
    ) -> ChatResult:
        text, calls = _classify(messages)
        message = AIMessage(
            content=text,
            tool_calls=[{"id": i, "name": n, "args": a, "type": "tool_call"} for i, n, a in calls],
        )
        return ChatResult(generations=[ChatGeneration(message=message)])

    async def _astream(  # type: ignore[override]
        self, messages: list[BaseMessage], stop: Any = None, run_manager: Any = None, **kwargs: Any
    ) -> AsyncIterator[ChatGenerationChunk]:
        text, calls = _classify(messages)
        words = text.split(" ") if text else []
        for i, word in enumerate(words):
            yield ChatGenerationChunk(
                message=AIMessageChunk(content=word + (" " if i < len(words) - 1 else ""))
            )
        for index, (call_id, name, args) in enumerate(calls):
            raw = json.dumps(args)
            half = len(raw) // 2
            yield ChatGenerationChunk(
                message=AIMessageChunk(
                    content="",
                    tool_call_chunks=[{"name": name, "args": raw[:half], "id": call_id, "index": index}],
                )
            )
            yield ChatGenerationChunk(
                message=AIMessageChunk(
                    content="",
                    tool_call_chunks=[{"name": None, "args": raw[half:], "id": None, "index": index}],
                )
            )
        yield ChatGenerationChunk(message=AIMessageChunk(content="", chunk_position="last"))


def scenario_model() -> ScenarioChatModel:
    return ScenarioChatModel()
