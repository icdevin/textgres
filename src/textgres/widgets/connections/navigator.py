from dataclasses import dataclass
from rich.text import Text, TextType
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
        Binding("ctrl+n", "new_connection", "New"),
        Binding("ctrl+e", "edit_connection", "Edit"),
        Binding("backspace", "delete_connection", "Delete"),
        Binding("ctrl+d", "disconnect", "Disconnect")
    ]

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
    class ConnectionHighlighted(Message):
        connection: Connection
        node: TreeNode[Connection]
        tree: "ConnectionTree"

        @property
        def control(self) -> "ConnectionTree":
            return self.tree

    @dataclass
    class ConnectionAdded(Message):
        connection: Connection

    @dataclass
    class ConnectionUpdated(Message):
        connection: Connection

    @dataclass
    class ConnectionRemoved(Message):
        connection: Connection

    connections: Reactive[list[Connection]] = reactive(list)
    highlighted_node: Reactive[Optional[TreeNode[Connection]]] = reactive(None)

    def watch_connections(self, connections: list[Connection]) -> None:
        # Loops through the root node's children and removes any which are not
        # in the connections list
        for node in self.root.children:
            if node.data not in connections:
                node.remove()

        # Loops through the connections list and adds any which are not in the
        # root node's children
        #
        # If the connection is already in the root node's children, check the
        # node label against the connection name and update the node label if
        # they are different
        for i, connection in enumerate(connections):
            node = next((n for n in self.root.children if n.data == connection), None)
            if node is None:
                node = self.add_connection(connection)
            elif node.label != self.get_connection_label(connection):
                self.update_node_label(node)

    @on(Tree.NodeExpanded)
    def on_node_expanded(self, event: Tree.NodeExpanded[Connection]) -> None:
        if isinstance(event.node.data, Connection):
            connection = event.node.data
            self.connect_connection(connection)
            self.update_node_label(event.node)
            if not connection.connected:
                event.node.collapse()

    @on(Tree.NodeHighlighted)
    def on_node_highlighted(self, event: Tree.NodeHighlighted[Connection]) -> None:
        if isinstance(event.node.data, Connection):
            self.highlighted_node = event.node
            self.post_message(
                self.ConnectionHighlighted(
                    connection=event.node.data,
                    node=event.node,
                    tree=self,
                )
            )
        else:
            self.highlighted_node = None

    async def action_new_connection(self) -> None:
        focused_before = self.screen.focused
        self.screen.set_focus(None)

        def _handle_new_connection_data(new_connection: Optional[Connection]) -> None:
            if new_connection is None:
                self.screen.set_focus(focused_before)
                return

            self.post_message(self.ConnectionAdded(connection=new_connection))
            self.move_cursor(self.root.children[-1])

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

            self.post_message(self.ConnectionUpdated(connection=connection))

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

            self.post_message(self.ConnectionRemoved(connection=connection))

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
            self.highlighted_node.collapse()
            self.update_node_label(self.highlighted_node)
            self.notify(
                title="Disconnected",
                message=f"Disconnected from \"{connection.name}\".",
                timeout=5,
            )

    def add_connection(self, connection: Connection) -> TreeNode[Connection]:
        return self.root.add(self.get_connection_label(connection), data=connection)

    def connect_connection(self, connection: Connection) -> None:
        try:
          connection.connect()
          self.notify(
              title="Connected",
              message=f"Connected to \"{connection.name}\".",
              timeout=5,
          )
        except Exception as e:
          self.notify(
              title="Connection error",
              message=f"Could not connect to \"{connection.name}\".",
              severity="error",
              timeout=5,
          )
          log.error(e)

    def get_connection_label(self, connection: Connection) -> Text:
        label = Text(connection.name)
        if connection.connected:
            label.append(" (connected)", style="green")
        return label

    def update_node_label(self, node: TreeNode[Connection]) -> None:
        node.set_label(self.get_connection_label(node.data))

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
            host.update(
                "{}:{}/{}".format(
                    connection.host,
                    str(connection.port),
                    connection.database,
                )
            )

class Navigator(Vertical):
    DEFAULT_CSS = """
    Navigator {
        height: 1fr;
        dock: left;
        width: auto;
        max-width: 25%;

        & .hidden {
            display: none;
        }

        & Tree {
            min-width: 20;
            background: transparent;
        }
    }
    """

    connections: Reactive[list[Connection]] = reactive(list)
    highlighted_connection: Reactive[Optional[Connection]] = reactive(None)

    def compose(self) -> ComposeResult:
        self.border_title = "Navigator"
        self.add_class("section")

        tree = ConnectionTree(label="connection")
        tree.data_bind(Navigator.connections)
        tree.auto_expand = False
        tree.guide_depth = 1
        tree.show_root = False
        tree.show_guides = False
        tree.cursor_line = 0

        yield tree
        yield ConnectionPreview()

    def watch_highlighted_connection(self, connection: Optional[Connection]) -> None:
        self.connection_preview.connection = connection or None

    @on(ConnectionTree.ConnectionHighlighted)
    def on_node_highlighted(self, event: Tree.NodeHighlighted[Connection]) -> None:
        if isinstance(event.node.data, Connection):
            self.highlighted_connection = event.node.data
        else:
            self.highlighted_connection = None

    @property
    def connection_preview(self) -> ConnectionPreview:
        return self.query_one(ConnectionPreview)

    @property
    def connection_tree(self) -> ConnectionTree:
        return self.query_one(ConnectionTree)