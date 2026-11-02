"""`MESSAGE_END.framework_state` (`sectr.adapters._framework_state`): the
tag on every adapter's state, its typed `data`, exact pieces, and the size
cap."""

from __future__ import annotations

import logging
from typing import Any

import pytest

from sectr.adapters._framework_state import (
    FRAMEWORK_STATE_LIMIT,
    FrameworkData,
    exact,
    framework_state,
    journaled_state,
    parsed,
)


class _Data(FrameworkData):
    adapter = "x"
    version = 1

    pieces: tuple[str, ...] = ()
    name: str | None = None


def test_state_is_tagged_with_its_adapter_and_version() -> None:
    assert framework_state(_Data(pieces=("p",))) == {"adapter": "x", "v": 1, "data": {"pieces": ["p"]}}


def test_absent_pieces_arent_written_and_nothing_to_keep_is_no_state() -> None:
    assert framework_state(_Data(name="n")) == {"adapter": "x", "v": 1, "data": {"name": "n"}}
    assert framework_state(_Data()) is None


def test_state_over_the_limit_isnt_journaled(caplog: pytest.LogCaptureFixture) -> None:
    with caplog.at_level(logging.WARNING):
        assert framework_state(_Data(name="a" * FRAMEWORK_STATE_LIMIT)) is None
    assert "over the" in caplog.text
    assert framework_state(_Data(name="a" * (FRAMEWORK_STATE_LIMIT - 100))) is not None


@pytest.mark.parametrize(
    ("state", "read"),
    [
        ({"adapter": "x", "v": 1, "data": {"name": "n"}}, True),
        ({"adapter": "y", "v": 1, "data": {"name": "n"}}, False),
        ({"adapter": "x", "v": 2, "data": {"name": "n"}}, False),
        ({"adapter": "x", "v": 1, "name": "n"}, False),
        ({"adapter": "x", "v": 1, "data": {"name": 1}}, False),
        ({"adapter": "x", "v": 1, "data": {"name": "n", "other": 1}}, False),
        ("garbage", False),
        (None, False),
    ],
    ids=["own", "foreign", "other_version", "not_under_data", "invalid", "unknown_field", "garbage", "none"],
)
def test_only_the_adapter_that_wrote_a_state_reads_it(state: Any, read: bool) -> None:
    message = {"role": "assistant", "_sectr_framework_state": state}
    assert journaled_state(message, _Data) == (_Data(name="n") if read else None)


def test_an_exact_piece_keeps_its_key_order() -> None:
    piece = {"type": "thinking", "signature": "s", "index": 0, "thinking": "ü"}
    text = exact(piece)
    assert text == '{"type":"thinking","signature":"s","index":0,"thinking":"ü"}'
    assert list(parsed(text)) == ["type", "signature", "index", "thinking"]


@pytest.mark.parametrize("text", ["{not json", "[1]", '"s"'])
def test_a_piece_that_isnt_a_json_object_doesnt_parse(text: str) -> None:
    with pytest.raises(ValueError):
        parsed(text)


@pytest.mark.parametrize(
    ("state", "warned"),
    [
        ({"adapter": "y", "v": 1, "data": {"name": "n"}}, None),
        ({"adapter": "x", "v": 2, "data": {"name": "n"}}, "v2 can't be read by this SDK (it reads v1)"),
        ({"adapter": "x", "v": 1, "data": {"name": 1}}, "v1 can't be read"),
    ],
    ids=["foreign_quietly", "newer_version", "invalid"],
)
def test_state_it_cant_read_warns_once_per_process(
    state: Any, warned: str | None, caplog: pytest.LogCaptureFixture
) -> None:
    message = {"role": "assistant", "_sectr_framework_state": state}
    with caplog.at_level(logging.WARNING):
        for _ in range(3):  # every turn rebuilds the whole history
            assert journaled_state(message, _Data) is None
    if warned is None:
        assert caplog.records == []
    else:
        assert len(caplog.records) == 1 and warned in caplog.text
