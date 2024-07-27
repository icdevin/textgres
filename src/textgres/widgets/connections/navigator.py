from dataclasses import dataclass
from rich.text import TextType
from textual import on
from textual.app import ComposeResult
from textual.containers import Vertical, VerticalScroll
from textual.message import Message
from textual.reactive import Reactive, reactive
from textual.widgets import Static, Tree
from textual.widgets.tree import TreeNode

from textgres.connection import Connection
from textgres.widgets.tree import TextgresTree
from textgres.widgets.connections.new_connection_modal import (
  NewConnectionModal,
  NewConnectionData
)

class ConnectionTree(TextgresTree[Connection]):
    COMPONENT_CLASSES = {
        "node-selected",
    }

    def __init__(
        self,
        label: TextType,
        data: Connection | None = None,
        *,
        name: str | None = None,
        id: str | None = None,
        classes: str | None = None,
        disabled: bool = False,
    ) -> None:
        super().__init__(
            label,
            data,
            name=name,
            id=id,
            classes=classes,
            disabled=disabled,
        )

    @dataclass
    class ConnectionSelected(Message):
        connection: Connection
        node: TreeNode[Connection]
        tree: "ConnectionTree"

        @property
        def control(self) -> "ConnectionTree":
            return self.tree

    currently_open: Reactive[TreeNode[Connection] | None] = reactive(None)

    def watch_currently_open(self, node: TreeNode[Connection] | None) -> None:
        if node and isinstance(node.data, Connection):
            self.post_messsage(
                self.ConnectionSelected(
                    connection=node.data,
                    node=node,
                    tree=self,
                )
            )

    @on(Tree.NodeSelected)
    def on_node_selected(self, event: Tree.NodeSelected[Connection]) -> None:
        event.stop()
        if isinstance(event.node.data, Connection):
            self.currently_open = event.node
            self.refresh()

    async def new_connection_flow(self) -> None:
        focused_before = self.screen.focused
        self.screen.set_focus(None)

        def _handle_new_connection_data(new_connection_data: NewConnectionData | None) -> None:
            if new_connection_data is None:
                self.screen.set_focus(focused_before)
                return

            new_connection = Connection(
                name=new_connection_data.name,
                host=new_connection_data.host,
                port=new_connection_data.port,
                database=new_connection_data.database,
                username=new_connection_data.username,
            )

            new_node = self.add_connection(new_connection)

            self.notify(
                title="Connection saved",
                message=f"Connection \"{new_connection_data.name}\" saved.",
                timeout=5,
            )

            def post_new_connection() -> None:
                self.screen.set_focus(focused_before)
                self.select_node(new_node)
                self.scroll_to_node(new_node, animate=False)

            self.call_after_refresh(post_new_connection)

        await self.app.push_screen(
            NewConnectionModal(),
            callback=_handle_new_connection_data,
        )

    def add_connection(
        self,
        connection: Connection,
    ) -> TreeNode[Connection]:
        return self.root.add(connection.name, data=connection)

class ConnectionPreview(VerticalScroll):
    DEFAULT_CSS = """
    ConnectionPreview {
        color: gray;
        background: transparent;
        dock: bottom;
        height: auto;
        max-height: 50%;
        padding: 0 1;
        border-top: solid gray 35%;
        &.hidden {
            display: none;
        }
    }
    """

    connection: Reactive[Connection | None] = reactive(None)

    def compose(self) -> ComposeResult:
        self.can_focus = False
        yield Static("", id="host")

    def watch_connection(self, connection: Connection | None) -> None:
        self.set_class(connection is None, "hidden")
        if connection:
            host = self.query_one("#host", Static)
            host.update(connection.host)

class Navigator(Vertical):
    DEFAULT_CSS = """
    Navigator {
        height: 1fr;
        dock: left;
        width: auto;
        max-width: 33%;

        & Tree {
            min-width: 20;
            background: transparent;
        }
    }
    """

    def __init__(self) -> None:
        super().__init__()

    def compose(self) -> ComposeResult:
        self.border_title = "Navigator"
        self.add_class("section")

        tree = ConnectionTree(
            label="connection"
        )
        tree.guide_depth = 1
        tree.show_root = False
        tree.show_guides = False
        tree.cursor_line = 0

        yield tree
        yield ConnectionPreview()

    @on(ConnectionTree.ConnectionSelected)
    def on_connection_selected(self, event: ConnectionTree.ConnectionSelected) -> None:
        if isinstance(event.node.data, Connection):
            self.connection_preview.connection = event.node.data

    @property
    def connection_preview(self) -> ConnectionPreview:
        return self.query_one(ConnectionPreview)

    @property
    def connection_tree(self) -> ConnectionTree:
        return self.query_one(ConnectionTree)