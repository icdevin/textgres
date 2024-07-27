from typing import TypeVar
from textual.binding import Binding
from textual.widgets import Tree

T = TypeVar("T")

class TextgresTree(Tree[T]):
    DEFAULT_CSS = """
    TextgresTree {
        scrollbar-size-horizontal: 0;
    }
    """

    BINDINGS = []