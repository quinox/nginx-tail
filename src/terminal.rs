use rustix::termios::tcgetwinsize;

// https://en.wikipedia.org/wiki/ANSI_escape_code#CSI_(Control_Sequence_Introducer)_sequences
pub mod colors {
    pub const CSI: &str = "\x1b[";
    pub const GREEN: &str = "\x1b[32m";
    pub const PURPLE: &str = "\x1b[35m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const RED: &str = "\x1b[31m";
    pub const WHITE: &str = "\x1b[1\x1b[37m"; // bright white
    pub const ORANGE: &str = "\x1b[93m"; // bright yellow
    pub const RESET: &str = "\x1b[0m";
}

pub fn get_terminal_width() -> u16 {
    match tcgetwinsize(std::io::stderr()) {
        Ok(x) => x.ws_col,
        Err(_) => 80,
    }
}

pub fn get_terminal_height() -> u16 {
    match tcgetwinsize(std::io::stderr()) {
        Ok(x) => x.ws_row,
        Err(_) => 10,
    }
}
