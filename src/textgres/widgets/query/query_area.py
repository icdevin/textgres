from textual import log
from textual.app import ComposeResult
from textual.containers import Vertical
from textual.reactive import Reactive, reactive
from textual.widgets import Select

from textgres.connection import Connection
from textgres.widgets.text_area import (
  TextgresTextArea,
  TextAreaFooter,
  TextEditor,
)

class QueryTextArea(TextgresTextArea):
    def on_mount(self):
        self.tab_behavior = "indent"
        self.show_line_numbers = True

class QueryArea(Vertical):
    DEFAULT_CSS = """
    QueryArea {
        & TextEditor {
            height: 1fr;
        }
    }
    """

    connections: Reactive[list[Connection]] = reactive([])

    def compose(self) -> ComposeResult:
        yield Select(options=[("No connections", -1)], allow_blank=False, id="placeholder-select")
        with Vertical() as vertical:
            vertical.border_title = "Query"
            text_area = QueryTextArea()
            yield TextEditor(
                text_area=text_area,
                footer=TextAreaFooter(text_area),
                id="text-query-editor",
            )

    def on_mount(self):
        self.border_title = "Query"
        self.add_class("section")

    def watch_connections(self):
        if len(self.connections) == 0:
            self.connection_select.disabled = True
        else:
            options = [(conn.name, i) for i, conn in enumerate(self.connections)]
            self.connection_select.set_options(options)
            self.connection_select.disabled = False

    @property
    def connection_select(self) -> Select:
        return self.query_one(Select)