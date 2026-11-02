"""Calibrate the mock's `anthropic` endpoint against the real Anthropic
Messages API: the streamed event layout, and which continuation requests it
rejects (`src/mock/validate.rs`, the Anthropic rules).

Streams one response with (summarized) thinking and a tool call (saved as
`tool.sse`, the layout the mock copies), then continues it (the user
message, the assistant's content blocks, a `tool_result`) in each variant
below, and prints one JSON row per variant. Costs a fraction of a cent. The
key comes from the environment or the given .env file:

    python calibrate/anthropic.py ../../.env out_dir

Last run 2026-10-10 (claude-haiku-5-5), twice, with the same results.
Rejected (400): `thinking.type: enabled` (this model takes only
`adaptive`); a thinking signature changed, empty or missing, in the last
turn or an earlier one; a tool_use without its tool_result opening the next
message (none, text before it, text between two results, or the call last);
a tool_result with no tool_use in the previous message; duplicate tool_use
ids in a message; tool_use input that isn't an object; a trailing assistant
message ("does not support assistant message prefill"); an empty text block;
null content; a `system` role in `messages`; tool_result content that is a
number, an object or a list of strings. Accepted: dropping the thinking
(thinking enabled or not), rewriting or emptying its text, moving it after
the call, text after the results, results split across user messages or out
of order, a call id reused in a later turn, two user or two assistant
messages in a row, empty string or empty list content, a conversation
opening with the assistant.

Rerun 2026-10-12 with the `tool_result` variants at the end, same results
for the rest: an empty text block is rejected only as a message's own
content. Inside a `tool_result` (alone, beside a non-empty one, or in an
earlier turn) it's accepted, as is an empty list.
"""

import copy
import json
import os
import sys

import httpx

ENV, OUT = sys.argv[1], sys.argv[2]
URL = "https://api.anthropic.com/v1/messages"
MODEL = "claude-haiku-5-5"


def env_key(name):
    if os.environ.get(name):
        return os.environ[name]
    for line in open(ENV):
        if line.startswith(name + "="):
            return line.split("=", 1)[1].strip().strip("\"'")
    raise SystemExit(f"{name} not found")


HEADERS = {"x-api-key": env_key("ANTHROPIC_API_KEY"), "anthropic-version": "2023-06-01"}
TOOLS = [{
    "name": "lookup_order",
    "description": "Look up an order's status.",
    "input_schema": {"type": "object", "properties": {"order_id": {"type": "string"}}, "required": ["order_id"]},
}]
USER = {"role": "user", "content": "Think step by step: which of orders 42, 17 and 8 has the largest "
        "digit sum? Then look up only that order with the tool."}
THINKING = {"thinking": {"type": "adaptive", "display": "summarized"}, "output_config": {"effort": "high"}}
BASE = {"model": MODEL, "tools": TOOLS, "max_tokens": 2048}


def first_response():
    """Stream until the response has a thinking block and a tool call."""
    os.makedirs(OUT, exist_ok=True)
    for _ in range(3):
        body = {**BASE, **THINKING, "stream": True, "messages": [USER]}
        with httpx.stream("POST", URL, headers=HEADERS, json=body, timeout=120) as r:
            r.raise_for_status()
            lines = list(r.iter_lines())
        blocks = {}
        for line in lines:
            if not line.startswith("data: "):
                continue
            d = json.loads(line[6:])
            if d["type"] == "content_block_start":
                blocks[d["index"]] = dict(d["content_block"])
            elif d["type"] == "content_block_delta":
                b, delta = blocks[d["index"]], d["delta"]
                for kind, field in [("thinking_delta", "thinking"), ("signature_delta", "signature"),
                                    ("text_delta", "text"), ("input_json_delta", "partial_json")]:
                    if delta["type"] == kind:
                        b[field] = b.get(field, "") + delta[field]
        content = [blocks[i] for i in sorted(blocks)]
        for b in content:
            if b["type"] == "tool_use":
                b["input"] = json.loads(b.pop("partial_json", "") or "{}")
        kinds = [b["type"] for b in content]
        if "thinking" in kinds and "tool_use" in kinds:
            with open(os.path.join(OUT, "tool.sse"), "w") as f:
                f.write("\n".join(lines))
            return content
    raise SystemExit("no response with thinking and a tool call in 3 tries")


content = first_response()
thinking = next(b for b in content if b["type"] == "thinking")
use = next(b for b in content if b["type"] == "tool_use")
cid = use["id"]


def asst(*blocks):
    return {"role": "assistant", "content": list(blocks)}


def th(**changes):
    b = copy.deepcopy(thinking)
    b.update(changes)
    return {k: v for k, v in b.items() if v is not None}


def call(id=cid, input=None):
    return {"type": "tool_use", "id": id, "name": use["name"], "input": use["input"] if input is None else input}


def res(id=cid, content="shipped"):
    return {"type": "tool_result", "tool_use_id": id, "content": content}


def usr(*blocks):
    return {"role": "user", "content": list(blocks)}


def text(s):
    return {"type": "text", "text": s}


def mid(s):
    m = len(s) // 2
    return s[:m] + ("B" if s[m] != "B" else "C") + s[m + 1:]


U2 = {"role": "user", "content": "Thanks. Anything else?"}
DONE = asst(text("Order 17 has shipped."))
ok = [USER, asst(th(), call()), usr(res())]

VARIANTS = {
    # (messages, thinking enabled)
    "valid: thinking + call + result": (ok, True),
    "valid, as received (with text blocks)": ([USER, asst(*content), usr(res())], True),
    "thinking dropped": ([USER, asst(call()), usr(res())], True),
    "thinking dropped, thinking disabled": ([USER, asst(call()), usr(res())], False),
    "thinking kept, thinking disabled": (ok, False),
    "thinking dropped, text before the call": ([USER, asst(text("Let me check."), call()), usr(res())], True),
    "thinking after the call": ([USER, asst(call(), th()), usr(res())], True),
    "signature changed mid-way": ([USER, asst(th(signature=mid(thinking["signature"])), call()), usr(res())], True),
    "signature empty": ([USER, asst(th(signature=""), call()), usr(res())], True),
    "signature missing": ([USER, asst(th(signature=None), call()), usr(res())], True),
    "thinking text rewritten": ([USER, asst(th(thinking="I will look up order 17."), call()), usr(res())], True),
    "thinking text emptied": ([USER, asst(th(thinking=""), call()), usr(res())], True),
    "earlier turn: thinking dropped": ([USER, asst(call()), usr(res()), DONE, U2], True),
    "earlier turn: signature changed": ([USER, asst(th(signature=mid(thinking["signature"])), call()), usr(res()), DONE, U2], True),
    "call without result, then user": ([USER, asst(th(), call()), U2], True),
    "call without result at the end": ([USER, asst(th(), call())], True),
    "result for an unknown id": ([USER, asst(th(), call()), usr(res("toolu_nope"))], True),
    "result with no call": ([USER, usr(res())], True),
    "text before the result": ([USER, asst(th(), call()), usr(text("Here:"), res())], True),
    "text after the result": ([USER, asst(th(), call()), usr(res(), text("Go on."))], True),
    "two calls, results in separate user messages": (
        [USER, asst(th(), call(), call("toolu_other")), usr(res()), usr(res("toolu_other"))], True),
    "two calls, results out of order": (
        [USER, asst(th(), call(), call("toolu_other")), usr(res("toolu_other"), res())], True),
    "two calls, text between the results": (
        [USER, asst(th(), call(), call("toolu_other")), usr(res(), text("And:"), res("toolu_other"))], True),
    "call id reused in a later turn": (
        [USER, asst(th(), call()), usr(res()), asst(call()), usr(res())], True),
    "duplicate call ids": ([USER, asst(th(), call(), call()), usr(res(), res())], True),
    "input a JSON array": ([USER, asst(th(), call(input=[1, 2])), usr(res())], True),
    "input a string": ([USER, asst(th(), call(input="42")), usr(res())], True),
    "trailing assistant text": ([USER, asst(text("Order"))], False),
    "trailing assistant text, thinking enabled": ([USER, asst(text("Order"))], True),
    "two user messages in a row": ([USER, U2], True),
    "two assistant messages in a row": ([USER, DONE, DONE, U2], True),
    "empty text block": ([USER, asst(text("")), U2], True),
    "assistant content empty string": ([USER, {"role": "assistant", "content": ""}, U2], True),
    "assistant content empty list": ([USER, asst(), U2], True),
    "assistant content null": ([USER, {"role": "assistant", "content": None}, U2], True),
    "first message from the assistant": ([DONE, U2], True),
    "system role in messages": ([{"role": "system", "content": "Be brief."}, USER], True),
    "result content a list of text blocks": ([USER, asst(th(), call()), usr(res(content=[text("shipped")]))], True),
    "result content a list of strings": ([USER, asst(th(), call()), usr(res(content=["shipped"]))], True),
    "result content a number": ([USER, asst(th(), call()), usr(res(content=4200))], True),
    "result content an object": ([USER, asst(th(), call()), usr(res(content={"id": "42"}))], True),
    "result content empty string": ([USER, asst(th(), call()), usr(res(content=""))], True),
    "result content an empty text block": ([USER, asst(th(), call()), usr(res(content=[text("")]))], True),
    "result content text blocks, one empty": (
        [USER, asst(th(), call()), usr(res(content=[text("shipped"), text("")]))], True),
    "result content empty list": ([USER, asst(th(), call()), usr(res(content=[]))], True),
    "earlier turn: result an empty text block": (
        [USER, asst(th(), call()), usr(res(content=[text("")])), DONE, U2], True),
}

for name, (messages, enabled) in VARIANTS.items():
    body = {**BASE, **(THINKING if enabled else {}), "messages": messages}
    r = httpx.post(URL, headers=HEADERS, json=body, timeout=120)
    j = r.json()
    if j.get("type") == "error":
        result = f"{r.status_code} {j['error'].get('message', '')}"
    else:
        result = f"{r.status_code} ok ({j.get('stop_reason')})"
    print(json.dumps({"variant": name, "result": result[:300]}), flush=True)
