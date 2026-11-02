"""Shared test fixtures."""

from __future__ import annotations

import pytest

from sectr.adapters import _framework_state


@pytest.fixture(autouse=True)
def _fresh_framework_state_warnings() -> None:
    """`journaled_state` warns once per process; each test starts afresh."""
    _framework_state._warned.clear()
