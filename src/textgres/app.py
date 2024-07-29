from pathlib import Path

from textual import log
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
from textgres.widgets.query.query_area import QueryArea
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

class MainScreen(Screen[None]):
    AUTO_FOCUS = None

    BINDINGS = [
        Binding("ctrl+h", "toggle_navigator", "Show/Hide Navigator")
    ]

    connections: Reactive[list[Connection]] = reactive([])

    def compose(self) -> ComposeResult:
        yield AppHeader()
        with AppBody():
            yield Navigator().data_bind(MainScreen.connections)
            yield QueryArea()
            yield ResultsArea()
        yield Footer()

    def action_toggle_navigator(self) -> None:
        self.navigator.toggle_class("hidden")

    @property
    def navigator(self) -> Navigator:
        return self.query_one(Navigator)

class Textgres(App[None]):
    CSS_PATH = Path(__file__).parent / "textgres.scss"
    BINDINGS = []

    connections: Reactive[list[Connection]] = reactive(Connection.load())

    def get_default_screen(self) -> MainScreen:
        self.main_screen = MainScreen().data_bind(Textgres.connections)
        return self.main_screen

if __name__ == "__main__":
    app = Textgres()
    app.run()