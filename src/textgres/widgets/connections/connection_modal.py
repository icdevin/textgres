from pydantic import ValidationError
from textual import log, on
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.screen import ModalScreen
from textual.widgets import Button, Footer, Input, Label
from typing import Optional

from textgres.connection import Connection

class ConnectionModal(ModalScreen[Optional[Connection]]):
    CSS = """
    ConnectionModal {
        align: center middle;

        & > VerticalScroll {
            background: $background;
            padding: 1 2;
            width: 50%;
            height: 48%;
            border: wide $background-lighten-2;
            border-title-color: $text;
            border-title-background: $background;
            border-title-style: bold;
        }

        & .host-group {
            margin-bottom: 1;
        }

        & .host {
            margin-right: 1;
            width: 3fr;
        }

        & .port {
            width: 1fr;
        }

        & Input {
            margin-bottom: 1;
            height: 1;
            width: 1fr;
        }

        & Horizontal {
            height: 2;
        }

        & Button {
            width: 1fr;
            dock: bottom;
        }
    }
    """

    BINDINGS = [
        Binding("escape", "close_screen", "Cancel"),
        Binding("ctrl+s", "save_connection", "Save"),
    ]

    def __init__(
        self,
        connection: Optional[Connection] = None,
        name: Optional[str] = None,
        id: Optional[str] = None,
        classes: Optional[str] = None,
    ) -> None:
        super().__init__(name, id, classes)
        self.connection = connection or Connection()

    def compose(self) -> ComposeResult:
        with VerticalScroll() as vs:
            vs.can_focus = False
            vs.border_title = "New Connection" if self.connection.id is None else "Edit Connection"

            yield Label("Name")
            yield Input(
                self.connection.name,
                placeholder="Enter a name for this connection, e.g. Development",
                id="name-input",
            )

            with Horizontal(classes="host-group"):
                with Vertical(classes="host"):
                    yield Label("Host")
                    yield Input(
                        self.connection.host,
                        placeholder="localhost",
                        id="host-input",
                    )

                with Vertical(classes="port"):
                    yield Label("Port")
                    yield Input(
                        str(self.connection.port),
                        placeholder="5432",
                        type="integer",
                        id="port-input",
                    )

            yield Label("Database")
            yield Input(
                self.connection.database,
                placeholder="postgres",
                id="database-input",
            )

            yield Label("Username")
            yield Input(
                self.connection.username,
                placeholder="postgres",
                id="username-input",
            )

            yield Label("Password")
            yield Input(
                self.connection.password,
                password=True,
                id="password-input",
            )

            yield Button.success("Save Connection", id="save-button")

        yield Footer()

    def action_close_screen(self) -> None:
        self.dismiss(None)

    @on(Input.Submitted)
    @on(Button.Pressed, selector="#save-button")
    def on_save(self, event: Input.Submitted | Button.Pressed) -> None:
        self.save_connection()

    def action_save_connection(self) -> None:
        self.save_connection()

    def save_connection(self) -> None:
        try:
            self.connection.name = self.query_one("#name-input", Input).value
            self.connection.host = self.query_one("#host-input", Input).value
            self.connection.port = int(self.query_one("#port-input", Input).value)
            self.connection.database = self.query_one("#database-input", Input).value
            self.connection.username = self.query_one("#username-input", Input).value
            self.connection.password = self.query_one("#password-input", Input).value
            self.dismiss(self.connection)
        except ValidationError as e:
            log(e)
