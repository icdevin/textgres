from textual import on
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Button, Footer, Label
from typing import Optional

from textgres.widgets.center_middle import CenterMiddle

class ConfirmModal(ModalScreen[bool]):
    CSS = """
    ConfirmModal {
        align: center middle;

        & > VerticalScroll {
            background: $background;
            padding: 1 2;
            width: 50%;
            height: 30%;
            border: wide $background-lighten-2;
            border-title-color: $text;
            border-title-background: $background;
            border-title-style: bold;
        }

        & Input {
            margin-bottom: 1;
            height: 1;
            width: 1fr;
        }

        & Horizontal {
            height: 2;
        }

        & .buttons {
            dock: bottom;

            & Button {
                width: 1fr;
            }
        }
    }
    """

    BINDINGS = [
        Binding("escape", "close_screen", "Cancel"),
    ]

    def __init__(
        self,
        message: str,
        title: str = "Are you sure?",
    ) -> None:
        super().__init__()
        self.message = message
        self.title = title

    def compose(self) -> ComposeResult:
        with VerticalScroll() as vs:
            vs.can_focus = False
            vs.border_title = self.title

            yield CenterMiddle(Label(self.message))

            with Horizontal(classes="buttons"):
                yield Button("No", id="deny-button")
                yield Button.error("Yes", id="confirm-button")

        yield Footer()

    def action_close_screen(self) -> None:
        self.dismiss(False)

    @on(Button.Pressed, selector="#confirm-button")
    def on_confirm(self) -> None:
        self.dismiss(True)

    @on(Button.Pressed, selector="#deny-button")
    def on_deny(self) -> None:
        self.dismiss(False)