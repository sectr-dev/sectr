"""Calibrate the mock's `openrouter` reasoning rule (`src/mock/validate.rs`,
`intact_signatures`): what does OpenRouter routed to Claude do with the
reasoning a tool-loop continuation sends back?

Streams one response with reasoning and a tool call (retrying if the model
happens not to reason or not to call the tool), saves the raw stream, then
sends the continuation (user, assistant with the call, tool result) back
with the assistant's reasoning in each variant below. Prints one JSON row
per variant. Costs a fraction of a cent. The key comes from the
environment or the given .env file:

    python calibrate/reasoning.py ../../.env out_dir

Last run 2026-10-10 (anthropic/claude-haiku-5.5, routed to Claude Platform
on AWS): only a signature changed in its cryptographic part was rejected
(400 "Invalid `signature` in `thinking` block"). Intact details (merged or
as streamed), rewritten text with the signature intact, unsigned details,
text only and no reasoning at all were accepted. The stream layout the
mock copies is in the saved `stream.sse`.
"""

import copy
import json
import os
import sys

import httpx

ENV, OUT = sys.argv[1], sys.argv[2]
URL = "https://openrouter.ai/api/v1/chat/completions"
MODEL = "anthropic/claude-haiku-5.5"


def env_key(name):
    if os.environ.get(name):
        return os.environ[name]
    for line in open(ENV):
        if line.startswith(name + "="):
            return line.split("=", 1)[1].strip().strip("\"'")
    raise SystemExit(f"{name} not found")


HEADERS = {"Authorization": f"Bearer {env_key('OPENROUTER_API_KEY')}"}
TOOLS = [{
    "type": "function",
    "function": {
        "name": "lookup_order",
        "description": "Look up an order's status.",
        "parameters": {"type": "object", "properties": {"order_id": {"type": "string"}}, "required": ["order_id"]},
    },
}]
BASE = {"model": MODEL, "tools": TOOLS, "reasoning": {"effort": "high"}, "max_tokens": 2048}
# A question that makes the model think before calling the tool.
USER = {
    "role": "user",
    "content": "Think carefully step by step first: which of orders 42, 17 and 8 has the "
    "largest digit sum? Then look up only that order with the tool.",
}


def first_response():
    """Stream until a response has both reasoning and a tool call."""
    for _ in range(3):
        lines = []
        body = {**BASE, "stream": True, "messages": [USER]}
        with httpx.stream("POST", URL, headers=HEADERS, json=body, timeout=120) as r:
            r.raise_for_status()
            lines = list(r.iter_lines())
        content, reasoning, details, calls = "", "", [], {}
        for line in lines:
            if not line.startswith("data: ") or line == "data: [DONE]":
                continue
            for choice in json.loads(line[6:]).get("choices", []):
                d = choice.get("delta", {})
                content += d.get("content") or ""
                reasoning += d.get("reasoning") or ""
                details.extend(d.get("reasoning_details") or [])
                for tc in d.get("tool_calls") or []:
                    c = calls.setdefault(tc["index"], {"id": None, "name": "", "arguments": ""})
                    c["id"] = tc.get("id") or c["id"]
                    fn = tc.get("function") or {}
                    c["name"] += fn.get("name") or ""
                    c["arguments"] += fn.get("arguments") or ""
        if reasoning and calls:
            with open(os.path.join(OUT, "stream.sse"), "w") as f:
                f.write("\n".join(lines))
            return content, reasoning, details, calls[0]
    raise SystemExit("no response with both reasoning and a tool call in 3 tries")


def merged(fragments):
    """Concatenate the streamed fragments per (type, index)."""
    out = {}
    for f in fragments:
        m = out.setdefault((f.get("type"), f.get("index")), {
            k: v for k, v in f.items() if k not in ("text", "summary", "data", "signature")
        })
        for k in ("text", "summary", "data", "signature"):
            if f.get(k):
                m[k] = m.get(k, "") + f[k]
    return list(out.values())


def edited(details, change):
    out = copy.deepcopy(details)
    for d in out:
        change(d)
    return out


def sig_middle(d):
    if d.get("signature"):
        s = d["signature"]
        m = len(s) // 2
        d["signature"] = s[:m] + ("B" if s[m] != "B" else "C") + s[m + 1:]


def rewrite_text(d):
    if d.get("text"):
        d["text"] = "I will just look up order 17."


def unsign(d):
    d.pop("signature", None)


os.makedirs(OUT, exist_ok=True)
content, reasoning, fragments, call = first_response()
details = merged(fragments)
tool_calls = [{"id": call["id"], "type": "function",
               "function": {"name": call["name"], "arguments": call["arguments"]}}]


def assistant(**extra):
    return {"role": "assistant", "content": content or None, "tool_calls": tool_calls, **extra}


VARIANTS = {
    "details intact (merged)": assistant(reasoning_details=details),
    "details as streamed (fragments)": assistant(reasoning_details=fragments),
    "signature changed mid-way": assistant(reasoning_details=edited(details, sig_middle)),
    "text rewritten, signature intact": assistant(reasoning_details=edited(details, rewrite_text)),
    "unsigned details": assistant(reasoning_details=edited(details, unsign)),
    "reasoning text only": assistant(reasoning=reasoning),
    "no reasoning": assistant(),
}
result = {"role": "tool", "tool_call_id": call["id"], "content": "shipped"}
for name, message in VARIANTS.items():
    body = {**BASE, "stream": False, "messages": [USER, message, result]}
    r = httpx.post(URL, headers=HEADERS, json=body, timeout=120)
    j = r.json()
    if "error" in j:
        raw = (j["error"].get("metadata") or {}).get("raw", "")
        outcome = f"{r.status_code} {j['error'].get('message', '')} {str(raw)[:200]}"
    else:
        outcome = f"{r.status_code} ok"
    print(json.dumps({"variant": name, "result": outcome}), flush=True)
