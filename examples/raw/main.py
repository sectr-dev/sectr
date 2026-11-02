"""Raw variant — the support agent with NO framework: a hand-rolled agentic
loop over OpenRouter's OpenAI-compatible chat-completions API.

This is the reference for what the SDK core provides without adapters:
SessionContext, typed events, the tool registry, and approval
pause/resume. The full contract is exercised: streaming deltas, tool
calls + journaled results, transcript-held state (todo_write), and an
approval pause that suspends the turn.

The adapter (sectr.server) streams the handler through sectr.drive, which
appends NO_MORE_ACTIONS on exhaustion (turn over); ApprovalRequired escaping
this handler becomes the final TOOL_APPROVAL_REQUIRED (turn suspended).
"""

from __future__ import annotations

import functools
import json
import os
import uuid
from collections.abc import AsyncIterator
from typing import Any, cast

from openai import AsyncOpenAI
from openai.types.chat import ChatCompletionChunk
from openai import AsyncStream

from sectr import AgentApp, SessionContext
from sectr.events import (
    MessageDelta,
    MessageEnd,
    MessageStart,
    RunnerEvent,
    ToolCall,
    ToolResult,
)
from tools import TOOLS

MODEL = "anthropic/claude-sonnet-4.5"

SYSTEM = "You are a customer-support agent for a small shop."


# The client is created lazily: importing this module (e.g. `sectr inspect`
# during deploy) must not require secrets — they are injected at spawn time.
@functools.lru_cache(maxsize=1)
def _client() -> AsyncOpenAI:
    return AsyncOpenAI(
        base_url="https://openrouter.ai/api/v1",
        api_key=os.environ["OPENROUTER_API_KEY"],  # injected by the sidecar
    )


app = AgentApp(name="support-agent", env=["OPENROUTER_API_KEY"])


def assistant_message(text: str, calls: dict[int, dict[str, str]]) -> dict[str, Any]:
    msg: dict[str, Any] = {"role": "assistant", "content": text or None}
    if calls:
        msg["tool_calls"] = [
            {
                "id": c["id"],
                "type": "function",
                "function": {"name": c["name"], "arguments": c["arguments"]},
            }
            for c in calls.values()
        ]
    return msg


def tool_message(call_id: str, result: Any) -> dict[str, Any]:
    return {"role": "tool", "tool_call_id": call_id, "content": json.dumps(result)}


@app.agent_route("/chat")
async def chat(ctx: SessionContext) -> AsyncIterator[RunnerEvent]:
    # ctx.messages already contains the current user message: the transcript
    # includes this invocation's INVOCATION_STARTED (whose reason carries it).
    messages: list[dict[str, Any]] = [
        {"role": "system", "content": SYSTEM},
        *ctx.messages,
    ]

    # Approval resume: the transcript ends with an unanswered tool call and
    # the decision has arrived. Execute (or record the denial), journal the
    # result, feed it to the model, and continue the loop.
    if ctx.reason["kind"] == "approval_decision":
        pending = ctx.pending_tool_call
        if pending is None:
            raise RuntimeError("approval resume but no pending tool call")
        result: Any = (
            await ctx.execute(
                pending["name"], call_id=pending["tool_call_id"], **pending["arguments"]
            )
            if ctx.decision["approved"]
            else {"error": "denied"}
        )
        yield ToolResult(tool_call_id=pending["tool_call_id"], result=result)
        messages.append(tool_message(pending["tool_call_id"], result))

    while True:
        raw_stream = await _client().chat.completions.create(
            model=MODEL,
            messages=messages,  # type: ignore[arg-type]
            tools=cast(Any, [t.openai_schema for t in TOOLS]),
            stream=True,
        )
        stream = cast(AsyncStream[ChatCompletionChunk], raw_stream)

        # Stream the assistant message; accumulate tool-call fragments.
        msg_id = uuid.uuid4().hex[:16]
        yield MessageStart(role="assistant", message_id=msg_id)
        text = ""
        calls: dict[int, dict[str, str]] = {}
        async for chunk in stream:
            choice = chunk.choices[0]
            delta = choice.delta
            if delta.content:
                text += delta.content
                yield MessageDelta(
                    message_id=msg_id,
                    delta={"type": "text", "text": delta.content},
                )
            for tc in delta.tool_calls or []:
                frag = calls.setdefault(
                    tc.index, {"id": "", "name": "", "arguments": ""}
                )
                if tc.id:
                    frag["id"] = tc.id
                if tc.function and tc.function.name:
                    frag["name"] = tc.function.name
                if tc.function and tc.function.arguments:
                    frag["arguments"] += tc.function.arguments
        yield MessageEnd(message_id=msg_id)

        if not calls:
            return  # generator exhausted -> driver emits NO_MORE_ACTIONS (turn over)

        # Journal + execute each requested tool, in order.
        messages.append(assistant_message(text, calls))
        for frag in calls.values():
            arguments = json.loads(frag["arguments"] or "{}")
            yield ToolCall(
                tool_call_id=frag["id"], name=frag["name"], arguments=arguments
            )
            # Raises ApprovalRequired if gated -> adapter journals
            # TOOL_APPROVAL_REQUIRED as the final frame and SUSPENDS the turn
            # (no NO_MORE_ACTIONS); the decision comes back on a fresh
            # invocation with reason kind approval_decision.
            result = await ctx.execute(
                frag["name"], call_id=frag["id"], **arguments
            )
            yield ToolResult(tool_call_id=frag["id"], result=result)
            messages.append(tool_message(frag["id"], result))
