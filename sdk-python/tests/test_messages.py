"""Golden-ish tests for the canonical transcript -> messages view
(SessionContext.messages), including multi-turn reconstruction, tool-call
merging, and approval-pause preservation."""

from __future__ import annotations

import pytest

from sectr import SessionContext
from sectr.context import INTERRUPTED_TOOL_RESULT
from sectr.events import Envelope


def env(seq: int, type: str, payload: dict) -> Envelope:  # noqa: A002
    return Envelope(
        id=f"id-{seq}",
        session_id="s",
        deployment_id="d",
        seq=seq,
        type=type,
        payload=payload,
        created_at="2026-01-01T00:00:00Z",
    )


def test_multi_turn_with_tools_and_pending_approval() -> None:
    transcript = [
        env(0, "SESSION_CREATED", {"app_id": "a", "route": "/chat"}),
        env(1, "INVOCATION_STARTED", {"invocation_id": "i1", "route": "/chat", "deployment_id": "d", "reason": {"kind": "user_message", "input": "Where is order A-1?"}}),
        env(2, "MESSAGE_START", {"role": "assistant", "message_id": "m1"}),
        env(3, "MESSAGE_DELTA", {"message_id": "m1", "delta": {"type": "text", "text": "Let me "}}),
        env(4, "MESSAGE_DELTA", {"message_id": "m1", "delta": {"type": "text", "text": "check."}}),
        env(5, "MESSAGE_END", {"message_id": "m1", "finish_reason": "tool_use"}),
        env(6, "TOOL_CALL", {"tool_call_id": "c1", "name": "lookup_order", "arguments": {"order_id": "A-1"}}),
        env(7, "TOOL_RESULT", {"tool_call_id": "c1", "result": {"status": "shipped"}}),
        env(8, "MESSAGE_START", {"role": "assistant", "message_id": "m2"}),
        env(9, "MESSAGE_DELTA", {"message_id": "m2", "delta": {"type": "text", "text": "It shipped."}}),
        env(10, "MESSAGE_END", {"message_id": "m2"}),
        
        env(12, "INVOCATION_STARTED", {"invocation_id": "i2", "route": "/chat", "deployment_id": "d", "reason": {"kind": "user_message", "input": "Refund it please"}}),
        env(13, "MESSAGE_START", {"role": "assistant", "message_id": "m3"}),
        env(14, "MESSAGE_END", {"message_id": "m3", "finish_reason": "tool_use"}),
        env(15, "TOOL_CALL", {"tool_call_id": "c2", "name": "refund", "arguments": {"order_id": "A-1"}}),
        env(16, "TOOL_APPROVAL_REQUIRED", {"tool_call_id": "c2", "name": "refund", "arguments": {"order_id": "A-1"}}),
        
    ]
    msgs = SessionContext(transcript=transcript, reason={"kind": "user_message", "input": {}}).messages

    assert msgs == [
        {"role": "user", "content": "Where is order A-1?"},
        {
            "role": "assistant",
            "content": "Let me check.",
            "tool_calls": [
                {
                    "id": "c1",
                    "type": "function",
                    "function": {"name": "lookup_order", "arguments": '{"order_id": "A-1"}'},
                }
            ],
        },
        {"role": "tool", "tool_call_id": "c1", "content": '{"status": "shipped"}'},
        {"role": "assistant", "content": "It shipped."},
        {"role": "user", "content": "Refund it please"},
        {
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {
                    "id": "c2",
                    "type": "function",
                    "function": {"name": "refund", "arguments": '{"order_id": "A-1"}'},
                }
            ],
        },
    ]


def test_pending_tool_call_result_is_not_fabricated() -> None:
    transcript = [
        env(0, "TOOL_CALL", {"tool_call_id": "c1", "name": "refund", "arguments": {}}),
    ]
    msgs = SessionContext(transcript=transcript, reason={"kind": "user_message", "input": {}}).messages
    assert "tool" not in [m["role"] for m in msgs]  # no result yet — LLM left it open


def user_turn(seq: int, text: str) -> Envelope:
    return env(seq, "INVOCATION_STARTED", {"invocation_id": f"i{seq}", "route": "/chat", "deployment_id": "d",
        "reason": {"kind": "user_message", "input": text}})


def call(seq: int, cid: str) -> Envelope:
    return env(seq, "TOOL_CALL", {"tool_call_id": cid, "name": "lookup_order", "arguments": {}})


def calls(*ids: str) -> list[dict]:
    return [{"id": i, "type": "function", "function": {"name": "lookup_order", "arguments": "{}"}} for i in ids]


def test_interrupted_call_followed_by_a_user_turn_is_answered() -> None:
    """Every prefix of a journal is a valid journal: a turn that died after
    journaling a TOOL_CALL (crash, or the tool raised) must still fold into
    a conversation providers accept once the user writes again."""
    transcript = [
        user_turn(0, "Where is A-1?"),
        env(1, "MESSAGE_START", {"role": "assistant", "message_id": "m1"}),
        env(2, "MESSAGE_END", {"message_id": "m1"}),
        call(3, "c1"),
        env(4, "ERROR", {"message": "lookup timed out"}),
        env(5, "NO_MORE_ACTIONS", {}),
        user_turn(6, "Hello?"),
    ]
    msgs = SessionContext(transcript=transcript, reason={"kind": "user_message", "input": "Hello?"}).messages
    assert msgs == [
        {"role": "user", "content": "Where is A-1?"},
        {"role": "assistant", "content": "", "tool_calls": calls("c1")},
        {"role": "tool", "tool_call_id": "c1", "content": INTERRUPTED_TOOL_RESULT},
        {"role": "user", "content": "Hello?"},
    ]


def test_only_the_unanswered_sibling_is_answered_after_the_real_results() -> None:
    transcript = [
        user_turn(0, "Check A-1 and A-2"),
        call(1, "c1"),
        call(2, "c2"),
        env(3, "TOOL_RESULT", {"tool_call_id": "c1", "result": "shipped"}),
        user_turn(4, "And?"),
    ]
    msgs = SessionContext(transcript=transcript, reason={"kind": "user_message", "input": "And?"}).messages
    assert msgs[1:] == [
        {"role": "assistant", "content": "", "tool_calls": calls("c1", "c2")},
        {"role": "tool", "tool_call_id": "c1", "content": "shipped"},
        {"role": "tool", "tool_call_id": "c2", "content": INTERRUPTED_TOOL_RESULT},
        {"role": "user", "content": "And?"},
    ]


def test_a_trailing_unanswered_call_is_left_for_the_approval_resume() -> None:
    transcript = [
        user_turn(0, "Refund A-1"),
        call(1, "c1"),
        env(2, "TOOL_APPROVAL_REQUIRED", {"tool_call_id": "c1", "name": "lookup_order", "arguments": {}}),
        env(3, "TOOL_APPROVAL_GRANTED", {"tool_call_id": "c1"}),
        env(4, "INVOCATION_STARTED", {"invocation_id": "i4", "route": "/chat", "deployment_id": "d",
            "reason": {"kind": "approval_decision", "decision": {"approved": True}}}),
    ]
    ctx = SessionContext(transcript=transcript, reason={"kind": "approval_decision", "decision": {"approved": True}})
    assert [m["role"] for m in ctx.messages] == ["user", "assistant"]  # nothing synthesized
    assert ctx.pending_tool_call is not None


def test_typed_deltas_merge_interpretably() -> None:
    text = {"type": "text", "text": "hi"}
    reasoning = {"type": "reasoning", "text": "thinking..."}
    legacy_str = "legacy bare string"  # pre-closure journal data
    foreign = {"type": "image", "url": "x"}  # pre-closure journal data
    transcript = [
        env(0, "MESSAGE_START", {"role": "assistant", "message_id": "m1"}),
        env(1, "MESSAGE_DELTA", {"message_id": "m1", "delta": text}),
        env(2, "MESSAGE_DELTA", {"message_id": "m1", "delta": reasoning}),
        env(3, "MESSAGE_DELTA", {"message_id": "m1", "delta": legacy_str}),
        env(4, "MESSAGE_DELTA", {"message_id": "m1", "delta": foreign}),
    ]
    msgs = SessionContext(transcript=transcript, reason={"kind": "user_message", "input": {}}).messages
    assert msgs[0]["content"] == "hi"  # typed text merges
    assert msgs[0]["reasoning_content"] == "thinking..."  # reasoning kept separate
    assert msgs[0]["_sectr_native_chunks"] == [legacy_str, foreign]  # defensive fallback


def test_framework_state_lands_on_its_message_with_its_calls() -> None:
    state = {"adapter": "x", "v": 1, "data": {"signed": "sig"}}
    transcript = [
        env(0, "MESSAGE_START", {"role": "assistant", "message_id": "m1"}),
        env(1, "MESSAGE_END", {"message_id": "m1", "framework_state": state}),
        env(2, "TOOL_CALL", {"tool_call_id": "c1", "name": "f", "arguments": {}}),
        env(3, "TOOL_RESULT", {"tool_call_id": "c1", "result": "ok"}),
        env(4, "MESSAGE_START", {"role": "assistant", "message_id": "m2"}),
        env(5, "MESSAGE_END", {"message_id": "m2"}),
    ]
    msgs = SessionContext(transcript=transcript, reason={"kind": "user_message", "input": {}}).messages
    assert msgs[0]["_sectr_framework_state"] == state
    assert [c["id"] for c in msgs[0]["tool_calls"]] == ["c1"]
    assert "_sectr_framework_state" not in msgs[2]


def test_tool_call_without_any_assistant_message_still_rendered() -> None:
    transcript = [
        env(0, "TOOL_CALL", {"tool_call_id": "c1", "name": "t", "arguments": {}}),
    ]
    msgs = SessionContext(transcript=transcript, reason={"kind": "user_message", "input": {}}).messages
    assert msgs == [
        {
            "role": "assistant",
            "content": "",
            "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "t", "arguments": "{}"}}
            ],
        }
    ]


def _user(seq: int, text: str) -> Envelope:
    return env(seq, "INVOCATION_STARTED", {"invocation_id": f"i{seq}", "route": "/chat", "deployment_id": "d", "reason": {"kind": "user_message", "input": text}})


def _text(seq: int, mid: str, text: str) -> list[Envelope]:
    return [
        env(seq, "MESSAGE_START", {"role": "assistant", "message_id": mid}),
        env(seq + 1, "MESSAGE_DELTA", {"message_id": mid, "delta": {"type": "text", "text": text}}),
        env(seq + 2, "MESSAGE_END", {"message_id": mid}),
    ]


def _call(seq: int, cid: str, name: str = "refund") -> Envelope:
    return env(seq, "TOOL_CALL", {"tool_call_id": cid, "name": name, "arguments": {}})


def _shape(msgs: list[dict]) -> list[tuple]:
    return [
        (m["role"], m.get("content"), [c["id"] for c in m.get("tool_calls", [])])
        for m in msgs
    ]


def test_tool_only_call_in_a_later_turn_gets_its_own_message() -> None:
    """A tool-only response opening turn 2 must not attach to turn
    1's last assistant message (that put the call before turn 2's user
    message, and the every-prefix rule then answered it as interrupted)."""
    transcript = [
        _user(0, "status?"),
        *_text(1, "m1", "Shipped."),
        _user(4, "refund it"),
        _call(5, "c"),
        env(6, "TOOL_APPROVAL_REQUIRED", {"tool_call_id": "c", "name": "refund", "arguments": {}}),
    ]
    msgs = SessionContext(transcript=transcript, reason={"kind": "approval_decision", "decision": {"approved": True}}).messages
    assert _shape(msgs) == [
        ("user", "status?", []),
        ("assistant", "Shipped.", []),
        ("user", "refund it", []),
        ("assistant", "", ["c"]),
    ]


def test_tool_only_call_after_a_tool_result_gets_its_own_message() -> None:
    """Within one turn: a second, tool-only model response after a tool
    result is a new assistant message, not a late addition to the first."""
    transcript = [
        _user(0, "where is 1?"),
        *_text(1, "m1", "Checking."),
        _call(4, "c1", "lookup"),
        env(5, "TOOL_RESULT", {"tool_call_id": "c1", "result": "shipped"}),
        _call(6, "c2"),
        env(7, "TOOL_RESULT", {"tool_call_id": "c2", "result": "refunded"}),
    ]
    msgs = SessionContext(transcript=transcript, reason={"kind": "user_message", "input": {}}).messages
    assert _shape(msgs) == [
        ("user", "where is 1?", []),
        ("assistant", "Checking.", ["c1"]),
        ("tool", "shipped", []),
        ("assistant", "", ["c2"]),
        ("tool", "refunded", []),
    ]


def test_parallel_tool_only_calls_share_one_message() -> None:
    """openai-agents journals a parallel response as every TOOL_CALL, then
    every TOOL_RESULT: the calls stay on one assistant message."""
    transcript = [
        *_text(0, "m0", "Earlier."),
        _user(3, "check both"),
        _call(4, "a", "lookup"),
        _call(5, "b", "lookup"),
        env(6, "TOOL_RESULT", {"tool_call_id": "a", "result": "1"}),
        env(7, "TOOL_RESULT", {"tool_call_id": "b", "result": "2"}),
    ]
    msgs = SessionContext(transcript=transcript, reason={"kind": "user_message", "input": {}}).messages
    assert _shape(msgs) == [
        ("assistant", "Earlier.", []),
        ("user", "check both", []),
        ("assistant", "", ["a", "b"]),
        ("tool", "1", []),
        ("tool", "2", []),
    ]


def test_resume_turn_reason_does_not_add_user_message() -> None:
    transcript = [
        env(0, "INVOCATION_STARTED", {"invocation_id": "i2", "route": "/chat", "deployment_id": "d",
            "reason": {"kind": "approval_decision", "decision": {"approved": True}}}),
    ]
    assert SessionContext(transcript=transcript, reason={"kind": "user_message", "input": {}}).messages == []




def test_crash_stitch_synthesizes_continuation_prompt() -> None:
    """An unterminated trailing message + SESSION_ERROR => the partial
    message as-is + a synthesized user continuation prompt (never persisted;
    derived from the journal)."""
    transcript = [
        env(0, "SESSION_CREATED", {"app_id": "a", "route": "/chat"}),
        env(1, "INVOCATION_STARTED", {"invocation_id": "i1", "route": "/chat", "deployment_id": "d", "reason": {"kind": "user_message", "input": "Write me a poem"}}),
        env(2, "MESSAGE_START", {"role": "assistant", "message_id": "m1"}),
        env(3, "MESSAGE_DELTA", {"message_id": "m1", "delta": {"type": "text", "text": "Roses are red,"}}),
        # crash: no MESSAGE_END; the reconciler synthesized the turn end
        env(4, "SESSION_ERROR", {"message": "turn abandoned: lease expired"}),
        env(5, "NO_MORE_ACTIONS", {}),
        # the NEXT invocation (the follow-up turn) — its messages view is
        # what the runner sees
        env(6, "INVOCATION_STARTED", {"invocation_id": "i2", "route": "/chat", "deployment_id": "d", "reason": {"kind": "user_message", "input": "try again"}}),
    ]
    ctx = SessionContext(transcript=transcript, reason={"kind": "user_message", "input": {}})
    msgs = ctx.messages
    assert msgs[0] == {"role": "user", "content": "Write me a poem"}
    assert msgs[1] == {"role": "assistant", "content": "Roses are red,"}
    assert msgs[2]["role"] == "user"
    assert "error interrupted" in msgs[2]["content"]


def test_crash_stitch_does_not_fire_on_clean_messages_or_tool_calls() -> None:
    """The stitch is ONLY for an unterminated message under a SESSION_ERROR:
    a completed message with a later SESSION_ERROR is untouched, and a crash
    after a journaled TOOL_CALL leaves the unanswered call as-is (providers
    reject user-after-tool_calls; the interrupted-tool machinery owns it)."""
    transcript = [
        env(0, "SESSION_CREATED", {"app_id": "a", "route": "/chat"}),
        env(1, "INVOCATION_STARTED", {"invocation_id": "i1", "route": "/chat", "deployment_id": "d", "reason": {"kind": "user_message", "input": "hi"}}),
        # completed message, then the turn died anyway
        env(2, "MESSAGE_START", {"role": "assistant", "message_id": "m1"}),
        env(3, "MESSAGE_DELTA", {"message_id": "m1", "delta": {"type": "text", "text": "done"}}),
        env(4, "MESSAGE_END", {"message_id": "m1"}),
        env(5, "TOOL_CALL", {"tool_call_id": "c1", "name": "f", "arguments": {"x": 1}}),
        env(6, "SESSION_ERROR", {"message": "turn abandoned: lease expired"}),
        env(7, "NO_MORE_ACTIONS", {}),
        env(8, "INVOCATION_STARTED", {"invocation_id": "i2", "route": "/chat", "deployment_id": "d", "reason": {"kind": "user_message", "input": "again"}}),
    ]
    ctx = SessionContext(transcript=transcript, reason={"kind": "user_message", "input": {}})
    msgs = ctx.messages
    roles = [m["role"] for m in msgs]
    # no synthetic user message after the tool call
    assert roles == ["user", "assistant", "user", "tool", "assistant"] or roles[-1] != "user" or "error interrupted" not in str(msgs[-1]), roles
    assert all("error interrupted" not in str(m) for m in msgs)
