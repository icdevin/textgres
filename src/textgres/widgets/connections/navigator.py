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
from textgres.widgets.confirm_modal import ConfirmModal
from textgres.widgets.tree import TextgresTree
from textgres.widgets.connections.connection_modal import (
  ConnectionModal,
)

class ConnectionTree(TextgresTree[Connection]):
    BINDINGS = [
        Binding("ctrl+n", "new_connection", "New Connection"),
        Binding("ctrl+e", "edit_connection", "Edit Connection"),
        Binding("backspace", "delete_connection", "Delete Connection"),
        Binding("ctrl+d", "disconnect", "Disconnect")
    ]

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

    connections: Reactive[list[Connection]] = reactive([])
    selected_node: Reactive[Optional[TreeNode[Connection]]] = reactive(None)
    highlighted_node: Reactive[Optional[TreeNode[Connection]]] = reactive(None)

    def watch_connections(self, connections: list[Connection]) -> None:
        self.clear()
        for connection in connections:
            self.add_connection(connection)

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

    @on(Tree.NodeExpanded)
    def on_node_expanded(self, event: Tree.NodeExpanded[Connection]) -> None:
        # If the expanded node is top-level, i.e. it's a Connection,
        # connect if not already connected
        if event.node.parent is self.root:
            connection = event.node.data
            if not connection.connected:
                connection.connect()

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

    async def action_new_connection(self) -> None:
        focused_before = self.screen.focused
        self.screen.set_focus(None)

        def _handle_new_connection_data(new_connection: Optional[Connection]) -> None:
            if new_connection is None:
                self.screen.set_focus(focused_before)
                return

            new_connection.save()
            new_node = self.add_connection(new_connection)

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

    async def action_edit_connection(self) -> None:
        if not self.highlighted_node:
            return

        def _handle_updated_connection_data(connection: Optional[Connection]) -> None:
            if connection is None:
                return

            connection.save()
            self.highlighted_node.data = connection
            self.highlighted_node.set_label(connection.name)

            self.notify(
                title="Connection updated",
                message=f"Connection \"{connection.name}\" updated.",
                timeout=5,
            )

        await self.app.push_screen(
            ConnectionModal(connection=self.highlighted_node.data),
            callback=_handle_updated_connection_data,
        )

    async def action_delete_connection(self) -> None:
        if not self.highlighted_node:
            return

        connection = self.highlighted_node.data

        def _handle_delete_connection_data(delete: bool) -> None:
            if not delete:
                return

            connection.delete()
            self.highlighted_node.remove()

            self.notify(
                title="Connection deleted",
                message=f"Connection \"{connection.name}\" deleted.",
                timeout=5,
            )

        await self.app.push_screen(
            ConfirmModal(
                message=f"Are you sure you want to delete connection \"{connection.name}\"?",
            ),
            callback=_handle_delete_connection_data,
        )

    def action_disconnect(self) -> None:
        if self.highlighted_node is None:
            return

        connection = self.highlighted_node.data
        if connection.connected:
            connection.disconnect()
            self.notify(
                title="Disconnected",
                message=f"Disconnected from \"{connection.name}\".",
                timeout=5,
            )

    def add_connection(self, connection: Connection) -> TreeNode[Connection]:
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

    connections: Reactive[list[Connection]] = reactive([])
    highlighted_connection: Reactive[Optional[Connection]] = reactive(None)

    def watch_highlighted_connection(self, connection: Optional[Connection]) -> None:
        self.connection_preview.connection = connection or None

    def compose(self) -> ComposeResult:
        self.border_title = "Navigator"
        self.add_class("section")

        tree = ConnectionTree(label="connection")
        tree.data_bind(Navigator.connections)
        tree.guide_depth = 1
        tree.show_root = False
        tree.show_guides = False
        tree.cursor_line = 0

        yield tree
        yield ConnectionPreview()

    @on(ConnectionTree.ConnectionHighlighted)
    def on_node_highlighted(self, event: Tree.NodeHighlighted[Connection]) -> None:
        if isinstance(event.node.data, Connection):
            self.highlighted_connection = event.node.data
        else:
            self.highlighted_connection = None

    @on(ConnectionTree.ConnectionSelected)
    def on_connection_selected(self, event: ConnectionTree.ConnectionSelected) -> None:
        connection = event.connection
        connection.connect()
        self.notify(
            title="Connected",
            message=f"Connected to \"{connection.name}\".",
            timeout=5,
        )

    @property
    def connection_preview(self) -> ConnectionPreview:
        return self.query_one(ConnectionPreview)

    @property
    def connection_tree(self) -> ConnectionTree:
        return self.query_one(ConnectionTree)