"""`MESSAGE_END.framework_state`: what an adapter journals of a model
response beyond its text and calls, in its framework's own form (e.g. signed
reasoning), so a later invocation rebuilds that response as the framework
stored it. Opaque to the platform; only the adapter that wrote it reads it.

The field is `{"adapter": <name>, "v": <version>, "data": {...}}`. Each
adapter types its `data` as a `FrameworkData` model, versioned with `v`. An
adapter ignores a field that isn't its own, is another version, or doesn't
validate, and rebuilds the message without it: that covers a redeploy that
changed the adapter or the framework.

The journal keeps a string value byte for byte, but an object or a number
only by value (an object's keys may come back in another order). So a piece
the adapter hands back to its framework untouched, which the framework may
forward to the provider as it is, goes in as its exact JSON text (`exact`,
read back with `parsed`); structure only the adapter reads may be JSON.
"""

from __future__ import annotations

import json
import logging
from typing import Any, ClassVar, TypeVar

from pydantic import BaseModel, ConfigDict, ValidationError

__all__ = [
    "FRAMEWORK_STATE_LIMIT",
    "FrameworkData",
    "exact",
    "framework_state",
    "journaled_state",
    "parsed",
    "unreadable",
]

logger = logging.getLogger(__name__)

# What `journaled_state` has warned about, once per process: every turn
# rebuilds the whole history, so a warning per message would repeat on
# every turn.
_warned: set[tuple[str, object, str]] = set()

# The most a message's field may take, serialized. Encrypted reasoning runs
# to a few KB per message; one far larger would bloat the journal and every
# transcript sent with an invoke. The worker applies the same limit.
FRAMEWORK_STATE_LIMIT = 256 * 1024


class FrameworkData(BaseModel):
    """An adapter's `data`, in its format `version`. A field this version
    doesn't know is a format it can't read: adding one bumps `version`."""

    model_config = ConfigDict(frozen=True, extra="forbid")

    adapter: ClassVar[str]
    version: ClassVar[int]


M = TypeVar("M", bound=FrameworkData)


def framework_state(data: FrameworkData) -> dict[str, Any] | None:
    """The `MESSAGE_END.framework_state` for `data`. Pieces at their default
    (absent) aren't written. `None` when there's nothing to keep, or when it
    can't be journaled (over `FRAMEWORK_STATE_LIMIT`): the message is then
    rebuilt without it, as if the framework had dropped it."""
    fields = data.model_dump(mode="json", exclude_defaults=True)
    if not fields:
        return None
    state = {"adapter": data.adapter, "v": data.version, "data": fields}
    # Counted as the worker does: compact, UTF-8.
    size = len(json.dumps(state, separators=(",", ":"), ensure_ascii=False).encode())
    if size > FRAMEWORK_STATE_LIMIT:
        logger.warning(
            "sectr: %s framework state of %d bytes is over the %d byte limit, not journaled; "
            "later turns rebuild this message without it",
            data.adapter,
            size,
            FRAMEWORK_STATE_LIMIT,
        )
        return None
    return state


def journaled_state(message: dict[str, Any], model: type[M]) -> M | None:
    """The framework state journaled with a canonical assistant message
    (`SessionContext.messages`) as `model`, if its adapter wrote it in this
    version and it validates.

    Another adapter's state is ignored quietly: that's a session whose
    framework changed. This adapter's state in another version (e.g. after
    rolling back to an older SDK), or one that doesn't validate, is ignored
    with a warning, once per process: the message is rebuilt without it, so
    the model loses that response's reasoning."""
    state = message.get("_sectr_framework_state")
    if not isinstance(state, dict) or state.get("adapter") != model.adapter:
        return None
    version = state.get("v")
    if version != model.version:
        _warn_once(
            model,
            version,
            "version",
            f"sectr: {model.adapter} framework state v{version} can't be read by this SDK "
            f"(it reads v{model.version}); messages carrying it are rebuilt without it, "
            "dropping their reasoning",
        )
        return None
    try:
        return model.model_validate(state.get("data"))
    except ValidationError as exc:
        unreadable(model, str(exc))
        return None


def unreadable(model: type[FrameworkData], detail: str) -> None:
    """Warn, once per process, that the adapter can't read its own state in
    its current version (it doesn't validate, or holds a piece the adapter
    can't use): messages carrying it are rebuilt without it."""
    _warn_once(
        model,
        model.version,
        "invalid",
        f"sectr: {model.adapter} framework state v{model.version} can't be read; messages "
        f"carrying it are rebuilt without it, dropping their reasoning: {detail}",
    )


def _warn_once(model: type[FrameworkData], version: object, problem: str, message: str) -> None:
    key = (model.adapter, version, problem)
    if key not in _warned:
        _warned.add(key)
        logger.warning(message)


def exact(value: Any) -> str:
    """`value`'s JSON text, for a piece the journal must keep exactly."""
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


def parsed(text: str) -> dict[str, Any]:
    """A JSON object written with `exact`, its keys in their original order.
    Raises `ValueError` if `text` isn't one."""
    value = json.loads(text)
    if not isinstance(value, dict):
        raise ValueError("not a JSON object")
    return value
