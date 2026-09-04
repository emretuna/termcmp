use anyhow::Result;

#[derive(Debug, Clone, Copy)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

pub fn get_terminal_size() -> Result<TerminalSize> {
    let (cols, rows) = crossterm::terminal::size()?;
    Ok(TerminalSize { rows, cols })
}
