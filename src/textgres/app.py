from pathlib import Path

from textual import log, on
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical
from textual.reactive import Reactive, reactive
from textual.screen import Screen
from textual.widgets import Footer, Label

from textgres.connection import Connection
from textgres.widgets.connections.navigator import (
    ConnectionTree,
    Navigator
)
from textgres.widgets.query.query_area import QueryArea, QueryTextArea
from textgres.widgets.results.results_area import ResultsArea

class AppHeader(Horizontal):
    """The header of the app."""

    DEFAULT_CSS = """
    AppHeader {
        padding: 0 3;
        margin-top: 1;
        margin-bottom: 1;
        height: 1;

        & > #app-title {
            dock: left;
        }
    }
    """

    def compose(self) -> ComposeResult:
        yield Label("Textgres", id="app-title")

class AppBody(Vertical):
    """The body of the app."""

    DEFAULT_CSS = """
    AppBody {
        padding: 0 2;
    }
    """

class Textgres(App[None]):
    CSS_PATH = Path(__file__).parent / "textgres.scss"
    BINDINGS = [
        Binding("ctrl+j", "toggle_navigator", "Show/Hide Navigator"),
    ]

    connections: Reactive[list[Connection]] = reactive(Connection.load())

    def compose(self) -> ComposeResult:
        yield AppHeader()
        with AppBody():
            yield Navigator().data_bind(Textgres.connections)
            yield QueryArea().data_bind(Textgres.connections)
            yield ResultsArea()
        yield Footer()

    def action_toggle_navigator(self) -> None:
        self.navigator.toggle_class("hidden")
        if self.navigator.has_class("hidden") and self.navigator.connection_tree.has_focus:
            self.screen.focus_next()

    @on(ConnectionTree.ConnectionAdded)
    def on_connection_added(self, event: ConnectionTree.ConnectionAdded) -> None:
        connection = event.connection

        connection.save()
        connections = [*self.connections, connection]
        self.connections = connections

        self.notify(
            title="Connection saved",
            message=f"Connection \"{connection.name}\" saved.",
            timeout=5,
        )

    @on(ConnectionTree.ConnectionUpdated)
    def on_connection_updated(self, event: ConnectionTree.ConnectionUpdated) -> None:
        connection = event.connection

        connection.save()
        connections = [c if c != connection else connection for c in self.connections]
        log(self.connections)
        self.connections = connections
        log(self.connections)

        self.notify(
            title="Connection updated",
            message=f"Connection \"{connection.name}\" updated.",
            timeout=5,
        )

    @on(ConnectionTree.ConnectionRemoved)
    def on_connection_removed(self, event: ConnectionTree.ConnectionRemoved) -> None:
        connection = event.connection

        connection.delete()
        connections = [c for c in self.connections if c != connection]
        self.connections = connections

        self.notify(
            title="Connection deleted",
            message=f"Connection \"{connection.name}\" deleted.",
            timeout=5,
        )

    @property
    def navigator(self) -> Navigator:
        return self.query_one(Navigator)

if __name__ == "__main__":
    app = Textgres()
    app.run()