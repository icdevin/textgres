use std::cell::RefCell;

use ratatui::{
  buffer::Buffer,
  crossterm::event::KeyEvent,
  layout::Rect,
  style::{Modifier, Style},
  widgets::{Block, Widget},
};
use ratatui_textarea::TextArea;
use tree_sitter_highlight::{
  HighlightConfiguration, HighlightEvent, Highlighter as TreeSitterHighlighter,
};
use unicode_width::UnicodeWidthChar;

use crate::theme::THEME;

// These names cover every capture emitted by tree-sitter-sequel's SQL query.
const CAPTURE_NAMES: &[&str] = &[
  "attribute",
  "boolean",
  "comment",
  "conditional",
  "field",
  "float",
  "function.call",
  "keyword",
  "keyword.operator",
  "number",
  "operator",
  "parameter",
  "punctuation.bracket",
  "punctuation.delimiter",
  "spell",
  "storageclass",
  "string",
  "type",
  "type.builtin",
  "type.qualifier",
  "variable",
];

/// Retains the proven textarea editing model and adds a PostgreSQL-aware renderer.
pub struct SqlEditor {
  textarea: TextArea<'static>,
  syntax: RefCell<SyntaxHighlighter>,
}

impl SqlEditor {
  /// Configures editor chrome and the bundled SQL grammar once per editor instance.
  pub fn new(lines: Vec<String>) -> Self {
    let mut textarea = TextArea::new(lines);
    textarea.set_line_number_style(Style::default().fg(THEME.muted));
    textarea.set_cursor_line_style(Style::default().bg(THEME.cursor_line));
    Self {
      textarea,
      syntax: RefCell::new(SyntaxHighlighter::new()),
    }
  }

  /// Delegates input so existing textarea navigation and editing remain unchanged.
  pub fn input(&mut self, key: KeyEvent) -> bool {
    self.textarea.input(key)
  }

  /// Exposes the canonical plain text used for execution and script persistence.
  pub fn lines(&self) -> &[String] {
    self.textarea.lines()
  }

  /// Keeps pane borders owned by the application layout.
  pub fn set_block(&mut self, block: Block<'static>) {
    self.textarea.set_block(block);
  }
}

impl Widget for &SqlEditor {
  fn render(self, area: Rect, buffer: &mut Buffer) {
    (&self.textarea).render(area, buffer);
    let inner = self
      .textarea
      .block()
      .map_or(area, |block| block.inner(area));
    self
      .syntax
      .borrow_mut()
      .render(self.textarea.lines(), &self.textarea, inner, buffer);
  }
}

#[derive(Clone, Copy, Debug)]
struct StyledRange {
  start: usize,
  end: usize,
  style: Style,
}

struct SyntaxHighlighter {
  configuration: HighlightConfiguration,
  highlighter: TreeSitterHighlighter,
}

impl SyntaxHighlighter {
  fn new() -> Self {
    // A failure here means the grammar bundled at compile time is incompatible.
    // The upstream numeric predicates use `%d`; append valid regex predicates.
    let highlights_query = format!(
      "{}\n{}",
      tree_sitter_sequel::HIGHLIGHTS_QUERY,
      r#"
((literal) @number
  (#match? @number "^[-+]?[0-9]+$"))
((literal) @float
  (#match? @float "^[-+]?[0-9]*\.[0-9]+([eE][-+]?[0-9]+)?$"))
"#
    );
    let mut configuration = HighlightConfiguration::new(
      tree_sitter_sequel::LANGUAGE.into(),
      "sql",
      &highlights_query,
      "",
      "",
    )
    .expect("the bundled SQL highlight query must be valid");
    configuration.configure(CAPTURE_NAMES);
    Self {
      configuration,
      highlighter: TreeSitterHighlighter::new(),
    }
  }

  // Reparse the current buffer on each draw so incomplete edits update immediately.
  fn render(&mut self, lines: &[String], textarea: &TextArea<'_>, area: Rect, buffer: &mut Buffer) {
    if area.is_empty() {
      return;
    }
    let source = lines.join("\n");
    let ranges = self.highlight_ranges(&source);
    if ranges.is_empty() {
      return;
    }

    let line_number_width = textarea
      .line_number_style()
      .map_or(0, |_| lines.len().max(1).to_string().len() + 2);

    let screen_cursor = textarea.screen_cursor();
    let cursor_position = find_cursor(buffer, area);
    let (top_row, top_column) = cursor_position.map_or((0, 0), |(x, y)| {
      let visible_row = usize::from(y.saturating_sub(area.y));
      let visible_column = usize::from(x.saturating_sub(area.x));
      (
        screen_cursor.row.saturating_sub(visible_row),
        (screen_cursor.col + line_number_width).saturating_sub(visible_column),
      )
    });

    let mut line_offsets = Vec::with_capacity(lines.len());
    let mut offset = 0;
    for line in lines {
      line_offsets.push(offset);
      offset += line.len() + 1;
    }
    for visible_row in 0..usize::from(area.height) {
      let row = top_row + visible_row;
      let Some(line) = lines.get(row) else {
        break;
      };
      self.style_line(
        line,
        line_offsets[row],
        &ranges,
        top_column,
        line_number_width,
        Rect::new(area.x, area.y + visible_row as u16, area.width, 1),
        buffer,
      );
    }
  }

  fn highlight_ranges(&mut self, source: &str) -> Vec<StyledRange> {
    let Ok(events) =
      self
        .highlighter
        .highlight(&self.configuration, source.as_bytes(), None, |_| None)
    else {
      return Vec::new();
    };
    let mut ranges = Vec::new();
    let mut styles = vec![Style::default()];
    for event in events {
      match event {
        Ok(HighlightEvent::Source { start, end }) => ranges.push(StyledRange {
          start,
          end,
          style: *styles.last().unwrap_or(&Style::default()),
        }),
        Ok(HighlightEvent::HighlightStart(highlight)) => {
          let name = CAPTURE_NAMES.get(highlight.0).copied().unwrap_or("");
          styles.push(capture_style(name));
        }
        Ok(HighlightEvent::HighlightEnd) => {
          styles.pop();
        }
        Err(_) => return Vec::new(),
      }
    }
    ranges
  }

  #[allow(clippy::too_many_arguments)]
  fn style_line(
    &self,
    line: &str,
    line_offset: usize,
    ranges: &[StyledRange],
    top_column: usize,
    line_number_width: usize,
    area: Rect,
    buffer: &mut Buffer,
  ) {
    let mut display_column = 0;
    let mut range_index = ranges.partition_point(|range| range.end <= line_offset);
    for (byte_offset, character) in line.char_indices() {
      let source_offset = line_offset + byte_offset;
      while ranges
        .get(range_index)
        .is_some_and(|range| range.end <= source_offset)
      {
        range_index += 1;
      }
      let style = ranges
        .get(range_index)
        .filter(|range| range.start <= source_offset && source_offset < range.end)
        .map_or_else(Style::default, |range| range.style);
      let width = character_width(character, display_column);
      for column in display_column..display_column + width {
        let visible_column = line_number_width as isize + column as isize - top_column as isize;
        if 0 <= visible_column && visible_column < area.width as isize {
          let x = area.x + visible_column as u16;
          buffer[(x, area.y)].set_style(style);
        }
      }
      display_column += width;
    }
  }
}

// The cursor is the only reversed cell in the textarea's default rendering.
fn find_cursor(buffer: &Buffer, area: Rect) -> Option<(u16, u16)> {
  for y in area.y..area.bottom() {
    for x in area.x..area.right() {
      if buffer[(x, y)].modifier.contains(Modifier::REVERSED) {
        return Some((x, y));
      }
    }
  }
  None
}

fn character_width(character: char, column: usize) -> usize {
  if character == '\t' {
    4 - (column % 4)
  } else {
    character.width().unwrap_or(0)
  }
}

fn capture_style(name: &str) -> Style {
  match name {
    "keyword" | "keyword.operator" => Style::default()
      .fg(THEME.accent)
      .add_modifier(Modifier::BOLD),
    "conditional" | "storageclass" | "type" | "type.builtin" | "type.qualifier" | "boolean" => {
      Style::default().fg(THEME.purple)
    }
    "function.call" | "operator" => Style::default().fg(THEME.cyan),
    "string" => Style::default().fg(THEME.green),
    "number" | "float" | "parameter" | "attribute" => Style::default().fg(THEME.yellow),
    "comment" | "spell" => Style::default()
      .fg(THEME.muted)
      .add_modifier(Modifier::ITALIC),
    "punctuation.bracket" | "punctuation.delimiter" => Style::default().fg(THEME.muted),
    _ => Style::default(),
  }
}

#[cfg(test)]
mod tests {
  use ratatui_textarea::CursorMove;

  use super::*;

  #[test]
  fn highlights_sql_tokens_with_semantic_styles() {
    let mut highlighter = SyntaxHighlighter::new();
    let source = "SELECT 42, 'value' -- note";
    let ranges = highlighter.highlight_ranges(source);
    let style_at = |needle: &str| {
      let offset = source.find(needle).unwrap();
      ranges
        .iter()
        .find(|range| range.start <= offset && offset < range.end)
        .unwrap()
        .style
    };

    assert_eq!(style_at("SELECT").fg, Some(THEME.accent));
    assert_eq!(style_at("42").fg, Some(THEME.yellow));
    assert_eq!(style_at("'value'").fg, Some(THEME.green));
    assert_eq!(style_at("-- note").fg, Some(THEME.muted));
  }

  #[test]
  fn renders_highlights_without_removing_the_cursor() {
    let editor = SqlEditor::new(vec!["SELECT 'value';".into()]);
    let area = Rect::new(0, 0, 30, 3);
    let mut buffer = Buffer::empty(area);

    (&editor).render(area, &mut buffer);

    let keyword = buffer
      .content()
      .iter()
      .find(|cell| cell.symbol() == "S")
      .unwrap();
    assert_eq!(keyword.fg, THEME.accent);
    assert!(keyword.modifier.contains(Modifier::BOLD));
    assert!(keyword.modifier.contains(Modifier::REVERSED));
    assert!(
      buffer
        .content()
        .iter()
        .any(|cell| cell.symbol() == "v" && cell.fg == THEME.green)
    );
  }

  #[test]
  fn highlights_text_exposed_by_horizontal_scrolling() {
    let mut editor = SqlEditor::new(vec!["SELECT x FROM y WHERE z".into()]);
    editor.textarea.move_cursor(CursorMove::End);
    let area = Rect::new(0, 0, 15, 2);
    let mut buffer = Buffer::empty(area);

    (&editor).render(area, &mut buffer);

    let from_start = buffer
      .content()
      .iter()
      .find(|cell| cell.symbol() == "F")
      .unwrap();
    assert_eq!(from_start.fg, THEME.accent);
  }
}
