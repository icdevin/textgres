from dataclasses import dataclass
from typing import Any, Iterable, Self
from textual.widgets import DataTable

class TextgresDataTable(DataTable[str]):
    DEFAULT_CSS = """\
    TextgresDataTable {
        height: 1fr;

        &.empty {
            display: none;
        }
    }
    """

    BINDINGS = []

    def __init__(self, *args: Any, **kwargs: Any):
        super().__init__(*args, **kwargs)
        self.cursor_vertical_escape = True

    def on_mount(self) -> None:
        self.add_class("empty")