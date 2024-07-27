from textual.app import ComposeResult
from textual.containers import Vertical

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

    def compose(self) -> ComposeResult:
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

