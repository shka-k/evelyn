use super::Term;

/// One cell range marked by the user, anchored to the buffer's global line
/// coordinates rather than to screen rows. `anchor` is where the drag /
/// extension started; `head` follows the cursor. Either may be greater
/// than the other — render / extract code normalizes.
#[derive(Clone, Copy, Debug)]
pub struct Selection {
    pub anchor_line: usize,
    pub anchor_col: u16,
    pub head_line: usize,
    pub head_col: u16,
    pub mode: SelectionMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionMode {
    /// Char-wise — the standard click-and-drag.
    Char,
    /// Word-wise — double-click expands to whitespace boundaries.
    Word,
    /// Line-wise — triple-click selects whole rows.
    Line,
}

impl Selection {
    /// Returns (start_line, start_col, end_line_inclusive, end_col_exclusive)
    /// after ordering anchor / head and applying the mode's bounds.
    fn normalized_range(&self, term: &Term) -> (usize, u16, usize, u16) {
        // Tuple ordering: line dominant, column secondary — the head can
        // be lexicographically before or after the anchor.
        let (sl, sc, el, ec) = if (self.anchor_line, self.anchor_col)
            <= (self.head_line, self.head_col)
        {
            (self.anchor_line, self.anchor_col, self.head_line, self.head_col)
        } else {
            (self.head_line, self.head_col, self.anchor_line, self.anchor_col)
        };
        match self.mode {
            SelectionMode::Char => {
                // ec is the column the user clicked on — include it.
                let end_col = ec.saturating_add(1).min(term.cols);
                (sl, sc, el, end_col)
            }
            SelectionMode::Line => (sl, 0, el, term.cols),
            SelectionMode::Word => {
                let word_start = expand_word_start(term, sl, sc);
                let word_end = expand_word_end(term, el, ec);
                (sl, word_start, el, word_end.saturating_add(1).min(term.cols))
            }
        }
    }

    /// Half-open range membership for cell `(line, col)`.
    fn contains_cell(&self, line: usize, col: u16, term: &Term) -> bool {
        let (sl, sc, el, ec) = self.normalized_range(term);
        if line < sl || line > el {
            return false;
        }
        let from = if line == sl { sc } else { 0 };
        let to = if line == el { ec } else { term.cols };
        col >= from && col < to
    }
}

/// Coarse character class for word-selection boundaries. Whitespace
/// terminates either side; alphanumeric/path characters cluster as one
/// "word"; runs of pure punctuation cluster separately so a stray
/// double-click on `||` selects just the operator instead of the whole
/// surrounding line.
fn word_class(c: char) -> u8 {
    if c == '\0' || c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '~') {
        1
    } else {
        2
    }
}

fn expand_word_start(term: &Term, line: usize, col: u16) -> u16 {
    let Some(start_cell) = term.cell_at_global(line, col) else { return col };
    let class = word_class(start_cell.ch);
    if class == 0 {
        return col;
    }
    let mut c = col;
    while c > 0 {
        match term.cell_at_global(line, c - 1) {
            Some(prev) if word_class(prev.ch) == class => c -= 1,
            _ => break,
        }
    }
    c
}

fn expand_word_end(term: &Term, line: usize, col: u16) -> u16 {
    let Some(start_cell) = term.cell_at_global(line, col) else { return col };
    let class = word_class(start_cell.ch);
    if class == 0 {
        return col;
    }
    let mut c = col;
    while c + 1 < term.cols {
        match term.cell_at_global(line, c + 1) {
            Some(next) if word_class(next.ch) == class => c += 1,
            _ => break,
        }
    }
    c
}

impl Term {
    pub fn start_selection(&mut self, line: usize, col: u16, mode: SelectionMode) {
        self.selection = Some(Selection {
            anchor_line: line,
            anchor_col: col,
            head_line: line,
            head_col: col,
            mode,
        });
        self.dirty = true;
    }

    pub fn update_selection(&mut self, line: usize, col: u16) {
        if let Some(sel) = self.selection.as_mut()
            && (sel.head_line != line || sel.head_col != col)
        {
            sel.head_line = line;
            sel.head_col = col;
            self.dirty = true;
        }
    }

    pub fn clear_selection(&mut self) {
        if self.selection.is_some() {
            self.selection = None;
            self.dirty = true;
        }
    }

    /// True iff the selection covers the cell at the given screen-relative
    /// position. Wide-char continuation cells inherit from the left half so
    /// the highlight doesn't break in the middle of a glyph.
    pub fn cell_in_selection(&self, x: u16, y: u16) -> bool {
        let Some(sel) = self.selection else { return false };
        let line = self.screen_to_global_line(y);
        if sel.contains_cell(line, x, self) {
            return true;
        }
        if x > 0 && self.cell_at(x, y).ch == '\0' {
            return sel.contains_cell(line, x - 1, self);
        }
        false
    }

    /// Materialize the selection to a String, joining rows with `\n` and
    /// trimming trailing blanks per row so the typical "select a chunk of
    /// shell output" case copies cleanly. Returns None when there's no
    /// selection or the captured range produces no text (e.g. anchor on a
    /// line that's rolled out of history).
    pub fn extract_selection_text(&self) -> Option<String> {
        let sel = self.selection?;
        let (sl, sc, el, ec) = sel.normalized_range(self);
        if sl > el {
            return None;
        }
        let mut out = String::new();
        for line in sl..=el {
            let from = if line == sl { sc } else { 0 };
            let to = if line == el { ec } else { self.cols };
            let mut line_text = String::new();
            let mut col = from;
            while col < to {
                if let Some(cell) = self.cell_at_global(line, col)
                    && cell.ch != '\0'
                {
                    line_text.push(cell.ch);
                }
                col += 1;
            }
            // Trim only when we extracted to the row end — a partial row
            // selection should keep its embedded blanks (the user picked
            // exactly that range).
            let piece: &str = if to == self.cols {
                line_text.trim_end()
            } else {
                line_text.as_str()
            };
            if line != sl {
                out.push('\n');
            }
            out.push_str(piece);
        }
        if out.is_empty() { None } else { Some(out) }
    }
}

