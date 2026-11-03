#!/usr/bin/python3
"""Emit a bounded, generic AT-SPI tree for the current Ilia process."""

from __future__ import annotations

import json
import sys
import time

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi


node_count = 0


def children(accessible):
    count = accessible.get_child_count()
    if count < 0 or count > 200:
        raise RuntimeError(f"unreasonable accessible child count: {count}")
    return [accessible.get_child_at_index(index) for index in range(count)]


def snapshot(accessible, depth=0):
    global node_count
    node_count += 1
    if node_count > 300 or depth > 12:
        raise RuntimeError("accessible tree exceeds the snapshot bounds")
    states = accessible.get_state_set()
    name = accessible.get_name() or ""
    if len(name) > 10000:
        raise RuntimeError(f"unreasonable accessible name length: {len(name)}")
    result = {
        "role": accessible.get_role_name(),
        "name": name,
        "interfaces": sorted(accessible.get_interfaces()),
        "selected": states.contains(Atspi.StateType.SELECTED),
        "focused": states.contains(Atspi.StateType.FOCUSED),
        "children": [],
    }
    if accessible.is_text():
        count = accessible.get_character_count()
        if count < 0 or count > 10000:
            raise RuntimeError(f"unreasonable accessible text length: {count}")
        result["text"] = Atspi.Text.get_text(accessible, 0, count)
    if accessible.is_table():
        row_count = Atspi.Table.get_n_rows(accessible)
        column_count = Atspi.Table.get_n_columns(accessible)
        if row_count < 0 or row_count > 100 or column_count < 0 or column_count > 10:
            raise RuntimeError(
                f"unreasonable accessible table shape: {row_count}x{column_count}"
            )
        table = []
        for row in range(row_count):
            values = []
            for column in range(column_count):
                cell = Atspi.Table.get_accessible_at(accessible, row, column)
                value = cell.get_name() or ""
                if len(value) > 10000:
                    raise RuntimeError(
                        f"unreasonable accessible cell name length: {len(value)}"
                    )
                values.append(value)
            table.append(values)
        result["table"] = table
    result["children"] = [snapshot(child, depth + 1) for child in children(accessible)]
    return result


def main():
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        desktop = Atspi.get_desktop(0)
        applications = [
            child for child in children(desktop)
            if (child.get_name() or "").casefold() == "ilia"
        ]
        if len(applications) == 1:
            time.sleep(0.1)
            print(json.dumps(snapshot(applications[0]), sort_keys=True))
            return 0
        time.sleep(0.05)
    print("timed out waiting for one Ilia AT-SPI application", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
