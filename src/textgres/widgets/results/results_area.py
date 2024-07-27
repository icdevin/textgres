from textual.app import ComposeResult
from textual.containers import Vertical
from textual.widgets import Label

from textgres.widgets.center_middle import CenterMiddle
from textgres.widgets.results.results_table import ResultsTable

class ResultsArea(Vertical):
    DEFAULT_CSS = """
    ResultsArea {
        border-subtitle-color: $text-muted;

        & ResultsTable {
            display: block;
        }

        & #empty-message {
            display: none;
        }

        &.empty {
            & ResultsTable {
                display: none;
            }

            & #empty-message {
                hatch: right $surface-lighten-1 70%;
                display: block;
            }
        }
    }
    """

    def __init__(
        self,
        name: str | None = None,
        id: str | None = None,
        classes: str | None = None,
        disabled: bool = False,
    ) -> None:
        super().__init__(name=name, id=id, classes=classes, disabled=disabled)
        self.table = ResultsTable()

    def on_mount(self) -> None:
        self.border_title = "Results"
        self.add_class("section")

    def compose(self) -> ComposeResult:
        self.set_class(self.table.row_count == 0, "empty")
        yield CenterMiddle(Label("No results."), id="empty-message")
        yield self.table