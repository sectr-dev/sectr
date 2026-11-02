"""Tests for Tool.openai_schema (signature-derived JSON schema)."""

from __future__ import annotations

from typing import Any

from sectr.tools import tool


async def lookup(order_id: str, limit: int = 5, verbose: bool | None = None) -> dict:
    """Look up an order."""


def test_schema_shape() -> None:
    t = tool(lookup)
    schema = t.openai_schema
    assert schema["type"] == "function"
    fn = schema["function"]
    assert fn["name"] == "lookup"
    assert fn["description"] == "Look up an order."
    params = fn["parameters"]
    assert params["type"] == "object"
    assert params["properties"]["order_id"] == {"type": "string"}
    assert params["properties"]["limit"] == {"type": "integer"}
    assert params["properties"]["verbose"] == {
        "anyOf": [{"type": "boolean"}, {"type": "null"}]
    }
    assert params["required"] == ["order_id"]


def test_list_and_dict_annotations() -> None:
    async def f(items: list[str], meta: dict[str, Any]) -> str: ...

    props = tool(f).openai_schema["function"]["parameters"]["properties"]
    assert props["items"] == {"type": "array", "items": {"type": "string"}}
    assert props["meta"]["type"] == "object"


def test_unannotated_param_accepts_anything() -> None:
    async def f(payload):  # type: ignore[no-untyped-def]
        return payload

    props = tool(f).openai_schema["function"]["parameters"]["properties"]
    assert props["payload"] == {}


def test_var_args_skipped() -> None:
    async def f(a: str, *args: Any, **kwargs: Any) -> str: ...

    props = tool(f).openai_schema["function"]["parameters"]["properties"]
    assert set(props) == {"a"}
