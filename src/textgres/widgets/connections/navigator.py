from dataclasses import dataclass
from rich.text import TextType
from textual import on, log
from textual.app import ComposeResult
from textual.binding import Binding
from textual.containers import Vertical, VerticalScroll
from textual.message import Message
from textual.reactive import Reactive, reactive
from textual.widgets import Static, Tree
from textual.widgets.tree import TreeNode
from typing import Optional

from textgres.connection import Connection
from textgres.widgets.tree import TextgresTree
from textgres.widgets.connections.connection_modal import (
  ConnectionModal,
)

class ConnectionTree(TextgresTree[Connection]):
    COMPONENT_CLASSES = {
        "node-selected",
    }

    def __init__(
        self,
        label: TextType,
        data: Optional[Connection] = None,
        *,
        name: Optional[str] = None,
        id: Optional[str] = None,
        classes: Optional[str] = None,
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

    @dataclass
    class ConnectionHighlighted(Message):
        connection: Connection
        node: TreeNode[Connection]
        tree: "ConnectionTree"

        @property
        def control(self) -> "ConnectionTree":
            return self.tree

    selected_node: Reactive[Optional[TreeNode[Connection]]] = reactive(None)
    highlighted_node: Reactive[Optional[TreeNode[Connection]]] = reactive(None)

    def watch_selected_node(self, node: Optional[TreeNode[Connection]]) -> None:
        if node and isinstance(node.data, Connection):
            self.post_message(
                self.ConnectionSelected(
                    connection=node.data,
                    node=node,
                    tree=self,
                )
            )

    def watch_highlighted_node(self, node: Optional[TreeNode[Connection]]) -> None:
        if node and isinstance(node.data, Connection):
            self.post_message(
                self.ConnectionHighlighted(
                    connection=node.data,
                    node=node,
                    tree=self,
                )
            )

    @on(Tree.NodeHighlighted)
    def on_node_highlighted(self, event: Tree.NodeHighlighted[Connection]) -> None:
        event.stop()
        if isinstance(event.node.data, Connection):
            self.highlighted_node = event.node
        else:
            self.highlighted_node = None

    @on(Tree.NodeSelected)
    def on_node_selected(self, event: Tree.NodeSelected[Connection]) -> None:
        event.stop()
        if isinstance(event.node.data, Connection):
            self.selected_node = event.node
            self.refresh()

    async def new_connection_flow(self) -> None:
        focused_before = self.screen.focused
        self.screen.set_focus(None)

        def _handle_new_connection_data(new_connection: Optional[Connection]) -> None:
            if new_connection is None:
                self.screen.set_focus(focused_before)
                return

            new_node = self.add_connection(new_connection)
            new_connection.save()

            self.notify(
                title="Connection saved",
                message=f"Connection \"{new_connection.name}\" saved.",
                timeout=5,
            )

            def post_new_connection() -> None:
                self.screen.set_focus(focused_before)
                self.select_node(new_node)
                self.scroll_to_node(new_node, animate=False)

            self.call_after_refresh(post_new_connection)

        await self.app.push_screen(
            ConnectionModal(),
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

    connection: Reactive[Optional[Connection]] = reactive(None)

    def compose(self) -> ComposeResult:
        self.can_focus = False
        yield Static("", id="host")

    def watch_connection(self, connection: Optional[Connection]) -> None:
        self.set_class(connection is None, "hidden")
        if connection:
            host = self.query_one("#host", Static)
            host.update("{}:{}".format(connection.host, str(connection.port)))

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

    BINDINGS = [
        Binding("ctrl+e", "edit_connection", "Edit Connection")
    ]

    highlighted_connection: Reactive[Optional[Connection]] = reactive(None)

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

        self.connections = Connection.load()
        for connection in self.connections:
            tree.add_connection(connection)

        yield tree
        yield ConnectionPreview()

    def on_mount(self) -> None:
        if len(self.connections) > 0:
            self.highlighted_connection = self.connections[0]

    async def action_edit_connection(self) -> None:
        log(self.highlighted_connection)
        if self.highlighted_connection:
            self.screen.set_focus(None)

            def _handle_updated_connection(updated_connection: Optional[Connection]) -> None:
                log(updated_connection)

            await self.app.push_screen(
                ConnectionModal(connection=self.highlighted_connection),
                callback=_handle_updated_connection,
            )

    # @on(ConnectionTree.ConnectionSelected)
    # def on_connection_selected(self, event: ConnectionTree.ConnectionSelected) -> None:
    #     if isinstance(event.node.data, Connection):
    #         self.connection_preview.connection = event.node.data

    @on(ConnectionTree.ConnectionHighlighted)
    def on_node_highlighted(self, event: Tree.NodeHighlighted[Connection]) -> None:
        if isinstance(event.node.data, Connection):
            self.highlighted_connection = event.node.data
        else:
            self.highlighted_connection = None

    def watch_highlighted_connection(self, connection: Optional[Connection]) -> None:
        self.connection_preview.connection = connection if connection else None

    @property
    def connection_preview(self) -> ConnectionPreview:
        return self.query_one(ConnectionPreview)

    @property
    def connection_tree(self) -> ConnectionTree:
        return self.query_one(ConnectionTree)