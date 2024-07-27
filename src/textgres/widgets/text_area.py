from dataclasses import dataclass
from textual import on
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.message import Message
from textual.reactive import reactive, Reactive
from textual.widgets import Checkbox, Label, TextArea

class TextgresTextArea(TextArea):
    BINDINGS = []

class TextAreaFooter(Horizontal):
    DEFAULT_CSS = """\
    TextAreaFooter {
        dock: bottom;
        height: 1;
        width: 1fr;

        &:focus-within {
            background: $primary 55%;
        }

        &:disabled {
            background: transparent;
        }

        & Checkbox {
            margin: 0 1;
            height: 1;
            color: $text;
            background: transparent;
            padding: 0 1;
            border: none;

            &:focus {
                padding: 0 1;
                border: none;
                background: $accent-lighten-1;
                color: $text;

                & .toggle--label {
                    text-style: not underline;
                }
            }
        }
    }
    """

    @dataclass
    class SoftWrapChanged(Message):
        value: bool
        footer: "TextAreaFooter"

        @property
        def control(self) -> "TextAreaFooter":
            return self.footer

    soft_wrap: Reactive[bool] = reactive(True, init=False)

    def __init__(
        self,
        text_area: TextArea,
        name: str | None = None,
        id: str | None = None,
        classes: str | None = None,
        disabled: bool = False,
    ) -> None:
        super().__init__(name=name, id=id, classes=classes, disabled=disabled)
        self.text_area = text_area
        self.set_reactive(TextAreaFooter.soft_wrap, text_area.soft_wrap)

    def compose(self) -> ComposeResult:
        yield Label("", id="mode-label")
        with Horizontal(classes="dock-right w-auto"):
            yield Checkbox(
                label="Wrap",
                value=self.soft_wrap,
                button_first=False,
                id="wrap-checkbox",
            ).data_bind(value=TextAreaFooter.soft_wrap)

    @on(Checkbox.Changed, selector="#wrap-checkbox")
    def update_soft_wrap(self, event: Checkbox.Changed) -> None:
        event.stop()
        self.soft_wrap = event.value
        self.post_message(self.SoftWrapChanged(self.soft_wrap, self))

class TextEditor(Vertical):
    soft_wrap: Reactive[bool] = reactive(True, init=False)

    def __init__(
        self,
        text_area: TextArea,
        footer: TextAreaFooter,
        name: str | None = None,
        id: str | None = None,
        classes: str | None = None,
        disabled: bool = False,
    ) -> None:
        super().__init__(name=name, id=id, classes=classes, disabled=disabled)
        self.text_area = text_area
        self.footer = footer

    def compose(self) -> ComposeResult:
        yield self.text_area.data_bind(TextEditor.soft_wrap)
        yield self.footer.data_bind(TextEditor.soft_wrap)

    @on(TextAreaFooter.SoftWrapChanged, selector="TextAreaFooter")
    def update_soft_wrap(self, event: TextAreaFooter.SoftWrapChanged) -> None:
        self.soft_wrap = event.value

    @property
    def text(self) -> str:
        return self.text_area.text