import click
from click_default_group import DefaultGroup

from textgres.app import Textgres
from textgres.connection import Connection

@click.group(cls=DefaultGroup, default="default", default_if_no_args=True)
def cli() -> None:
    """A TUI for Postgres."""

@cli.command()
def default() -> None:
    app = make_textgres()
    app.run()

def make_textgres() -> Textgres:
    return Textgres()