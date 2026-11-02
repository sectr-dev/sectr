"""Calibrate the mock's `openai_responses` endpoint against the real OpenAI
Responses API: the streamed event layout, and which continuation inputs it
rejects (`src/mock/validate.rs`, the Responses rules).

Streams one response with reasoning and a forced tool call (saved as
`tool.sse`, the layout the mock copies), then continues it (input: the
user message, the response's output items, a `function_call_output`) in
each variant below, and prints one JSON row per variant. Costs a fraction
of a cent. The key comes from the environment or the given .env file:

    python calibrate/responses.py ../../.env out_dir

Last run 2026-10-10 (gpt-5.4-nano), twice, with the same results. Rejected
(400): a function_call with no output for its call_id ("No tool output found
for function call ..."), an output with no call, a reasoning item whose
encrypted_content was changed ("could not be verified"), and an output that
isn't a string or an array of objects (a bare list, a number, an object).
Accepted: dropping the reasoning item (with or without the call's fc_ id),
a rewritten summary, an id-only reasoning item (store true or false),
duplicate call_ids, arguments that aren't JSON or are an array, outputs out
of call order, a trailing assistant message, two user messages in a row.
"""

import copy
import json
import os
import sys

import httpx

ENV, OUT = sys.argv[1], sys.argv[2]
URL = "https://api.openai.com/v1/responses"
MODEL = "gpt-5.4-nano"


def env_key(name):
    if os.environ.get(name):
        return os.environ[name]
    for line in open(ENV):
        if line.startswith(name + "="):
            return line.split("=", 1)[1].strip().strip("\"'")
    raise SystemExit(f"{name} not found")


HEADERS = {"Authorization": f"Bearer {env_key('OPENAI_API_KEY')}"}
TOOLS = [{
    "type": "function",
    "name": "lookup_order",
    "description": "Look up an order's status.",
    "parameters": {"type": "object", "properties": {"order_id": {"type": "string"}}, "required": ["order_id"]},
}]
USER = {"role": "user", "content": "Think step by step: which of orders 42, 17 and 8 has the largest "
        "digit sum? Then look up only that order with the tool."}
BASE = {"model": MODEL, "tools": TOOLS, "reasoning": {"effort": "medium", "summary": "auto"},
        "include": ["reasoning.encrypted_content"], "max_output_tokens": 400}


def first_response():
    """Stream until the response has a reasoning item and a function call."""
    os.makedirs(OUT, exist_ok=True)
    for _ in range(3):
        body = {**BASE, "store": False, "stream": True, "tool_choice": "required", "input": [USER]}
        with httpx.stream("POST", URL, headers=HEADERS, json=body, timeout=120) as r:
            r.raise_for_status()
            lines = list(r.iter_lines())
        output = []
        for line in lines:
            if line.startswith("data: "):
                d = json.loads(line[6:])
                if d["type"] == "response.completed":
                    output = d["response"]["output"]
        kinds = [o["type"] for o in output]
        if "reasoning" in kinds and "function_call" in kinds:
            with open(os.path.join(OUT, "tool.sse"), "w") as f:
                f.write("\n".join(lines))
            return output
    raise SystemExit("no response with reasoning and a function call in 3 tries")


output = first_response()
reasoning = next(o for o in output if o["type"] == "reasoning")
fc = next(o for o in output if o["type"] == "function_call")
cid = fc["call_id"]


def call(**changes):
    item = {k: v for k, v in fc.items() if k != "status"}
    item.update(changes)
    return {k: v for k, v in item.items() if v is not None}


def out(call_id=cid, output="shipped"):
    return {"type": "function_call_output", "call_id": call_id, "output": output}


def rs(**changes):
    item = copy.deepcopy(reasoning)
    item.update(changes)
    return {k: v for k, v in item.items() if v is not None}


def mid(s):
    m = len(s) // 2
    return s[:m] + ("B" if s[m] != "B" else "C") + s[m + 1:]


U2 = {"role": "user", "content": "Thanks. Anything else?"}
other = {"type": "function_call", "call_id": "call_other", "name": "lookup_order", "arguments": '{"order_id": "1"}'}

VARIANTS = {
    # (input, store)
    "valid: reasoning + call + output": ([USER, rs(), call(), out()], False),
    "reasoning dropped (call keeps its fc_ id)": ([USER, call(), out()], False),
    "reasoning dropped, call without id": ([USER, call(id=None), out()], False),
    "reasoning encrypted_content changed mid-way": ([USER, rs(encrypted_content=mid(reasoning["encrypted_content"])), call(), out()], False),
    "reasoning summary rewritten": ([USER, rs(summary=[{"type": "summary_text", "text": "I will look it up."}]), call(), out()], False),
    "reasoning without encrypted_content (id only), store false": ([USER, rs(encrypted_content=None), call(), out()], False),
    "reasoning without encrypted_content (id only), store true": ([USER, rs(encrypted_content=None), call(), out()], True),
    "reasoning not followed by its call": ([USER, rs(), U2], False),
    "call without output, then user": ([USER, call(id=None), U2], False),
    "call without output at the end": ([USER, call(id=None)], False),
    "output for an unknown call_id": ([USER, call(id=None), out("call_nope")], False),
    "output with no call": ([USER, out()], False),
    "duplicate call_id": ([USER, call(id=None), call(id=None, arguments='{"order_id": "1"}'), out(), out()], False),
    "arguments not JSON": ([USER, call(id=None, arguments="not json"), out()], False),
    "arguments a JSON array": ([USER, call(id=None, arguments="[1, 2]"), out()], False),
    "outputs out of call order": ([USER, call(id=None), other, out("call_other"), out()], False),
    "trailing assistant message": ([USER, {"role": "assistant", "content": "The order"}], False),
    "two user messages in a row": ([USER, U2], False),
    "output a bare JSON list": ([USER, call(id=None), out(output=["socks", "hat"])], False),
    "output a number": ([USER, call(id=None), out(output=4200)], False),
    "output a JSON object": ([USER, call(id=None), out(output={"id": "42"})], False),
}

for name, (inp, store) in VARIANTS.items():
    body = {**BASE, "store": store, "input": inp}
    r = httpx.post(URL, headers=HEADERS, json=body, timeout=120)
    j = r.json()
    if j.get("error"):
        result = f"{r.status_code} {j['error'].get('message', '')}"
    else:
        result = f"{r.status_code} ok ({j.get('status')})"
    print(json.dumps({"variant": name, "result": result[:300]}), flush=True)
