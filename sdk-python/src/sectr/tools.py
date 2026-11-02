"""Tool registry + approval gating.

- `@tool` registers a plain function as a `Tool` (framework-free; the raw
  example drives this). Frameworks with native HITL (openai-agents
  `needs_approval`, claude-code permissions) bypass this module.
- Gating is the union of code-level marking (`needs_approval=True`) and the
  deployment's policy (`[deploy] approval_tools` in sectr.toml), which Sectr
  passes to the runner as `SECTR_APPROVAL_POLICY` (JSON list of gated tool
  names), in dev and deployed — deployment config alone can gate a tool
  the user forgot to mark. No runtime platform round-trips exist: gating is
  evaluated locally, and a gated call raises `ApprovalRequired`, which the
  adapter converts into the final TOOL_APPROVAL_REQUIRED event.
"""

from __future__ import annotations

import inspect
import json
import os
import typing
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import Any, overload

__all__ = ["ApprovalRequired", "Tool", "tool", "get_tool", "is_gated"]

ToolFn = Callable[..., Any]


class ApprovalRequired(BaseException):  # noqa: N818 — control flow, not an error
    """Raised by `SessionContext.execute` / `request_approval` when a tool
    call needs human approval. The adapter catches this escaping the handler
    and converts it into the final TOOL_APPROVAL_REQUIRED event (finality is
    adapter-enforced), ending the turn.

    Deliberately a `BaseException`: approval is control flow, not an error
    (the same reasoning as LangGraph's `GraphBubbleUp`). Frameworks that
    catch `Exception` around tool calls (e.g. LangGraph's
    `ToolNode(handle_tool_errors=True)`) would otherwise swallow the pause
    and feed an "Error: ..." string back to the model. Every catch site
    handles it explicitly by type."""

    def __init__(
        self,
        name: str,
        arguments: Any,
        *,
        call_id: str | None = None,
        reason: str | None = None,
    ) -> None:
        self.name = name
        self.arguments = arguments
        self.call_id = call_id
        self.reason = reason
        super().__init__(f"tool {name!r} requires approval")


_JSON_TYPES: dict[type, str] = {
    str: "string",
    int: "integer",
    float: "number",
    bool: "boolean",
}


def _json_schema_for(annotation: Any) -> dict[str, Any]:
    """Best-effort JSON-schema fragment for a Python annotation."""
    import types
    import typing

    if annotation is inspect.Parameter.empty or annotation is Any:
        return {}  # any JSON
    origin = typing.get_origin(annotation)
    if origin is types.UnionType or origin is typing.Union:
        subs = [_json_schema_for(a) for a in typing.get_args(annotation) if a is not type(None)]
        if any(a is type(None) for a in typing.get_args(annotation)):
            subs.append({"type": "null"})
        return {"anyOf": subs} if len(subs) > 1 else (subs[0] if subs else {})
    if origin in (list, tuple, set, frozenset):
        schema: dict[str, Any] = {"type": "array"}
        type_args = typing.get_args(annotation)
        if type_args:
            schema["items"] = _json_schema_for(type_args[0])
        return schema
    if origin is dict:
        schema = {"type": "object"}
        type_args = typing.get_args(annotation)
        if len(type_args) == 2:
            schema["additionalProperties"] = _json_schema_for(type_args[1])
        return schema
    return {"type": _JSON_TYPES[annotation]} if annotation in _JSON_TYPES else {}


@dataclass
class Tool:
    """A registered tool: the function plus its contract metadata."""

    name: str
    fn: ToolFn
    description: str | None = None
    needs_approval: bool = False

    async def __call__(self, **arguments: Any) -> Any:
        result = self.fn(**arguments)
        if inspect.isawaitable(result):
            result = await result
        return result

    @property
    def openai_schema(self) -> dict[str, Any]:
        """Chat-completions `tools=[...]` entry: function name, description,
        and a JSON-schema `parameters` derived from the signature + type
        annotations (un-annotated params accept any JSON)."""
        parameters = inspect.signature(self.fn).parameters
        # resolve string annotations (PEP 563) to real types
        hints = typing.get_type_hints(self.fn)
        properties: dict[str, Any] = {}
        required: list[str] = []
        for pname, p in parameters.items():
            if p.kind in (inspect.Parameter.VAR_POSITIONAL, inspect.Parameter.VAR_KEYWORD):
                continue
            properties[pname] = _json_schema_for(hints.get(pname, inspect.Parameter.empty))
            if p.default is inspect.Parameter.empty:
                required.append(pname)
        description = (self.description or self.name).strip()
        return {
            "type": "function",
            "function": {
                "name": self.name,
                "description": description,
                "parameters": {
                    "type": "object",
                    "properties": properties,
                    **({"required": required} if required else {}),
                },
            },
        }


_REGISTRY: dict[str, Tool] = {}


@overload
def tool(fn: ToolFn) -> Tool: ...


@overload
def tool(
    *,
    name: str | None = ...,
    description: str | None = ...,
    needs_approval: bool = ...,
) -> Callable[[ToolFn], Tool]: ...


def tool(
    fn: ToolFn | None = None,
    *,
    name: str | None = None,
    description: str | None = None,
    needs_approval: bool = False,
) -> Any:
    """Register a function as a tool. Usable bare (`@tool`) or with
    arguments (`@tool(needs_approval=True)`)."""

    def wrap(f: ToolFn) -> Tool:
        t = Tool(
            name=name or f.__name__,
            fn=f,
            description=description or f.__doc__,
            needs_approval=needs_approval,
        )
        _REGISTRY[t.name] = t
        return t

    return wrap(fn) if fn is not None else wrap


def get_tool(name: str) -> Tool:
    """Look up a registered tool by name."""
    try:
        return _REGISTRY[name]
    except KeyError:
        raise KeyError(f"no tool registered named {name!r}") from None


def platform_approval_policy() -> list[str]:
    """Tool names gated by deployment config, injected by the sidecar as
    SECTR_APPROVAL_POLICY (JSON list). Malformed values fail closed ([] is
    *not* fail-closed — unknown policy means we cannot know, so an
    unreadable value is treated as an error at spawn time; here we tolerate
    absence only)."""
    raw = os.environ.get("SECTR_APPROVAL_POLICY")
    if not raw:
        return []
    value = json.loads(raw)
    if not isinstance(value, list) or not all(isinstance(v, str) for v in value):
        raise ValueError("SECTR_APPROVAL_POLICY must be a JSON array of tool names")
    return value


def is_gated(name: str) -> bool:
    """Effective gate decision: code-level marking OR platform policy."""
    registered = _REGISTRY.get(name)
    if registered is not None and registered.needs_approval:
        return True
    return name in platform_approval_policy()
