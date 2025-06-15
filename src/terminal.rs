use rustix::termios::LocalModes;
use rustix::termios::OptionalActions;
use rustix::termios::SpecialCodeIndex;
use rustix::termios::Termios;
use rustix::termios::tcgetattr;
use rustix::termios::tcgetwinsize;
use rustix::termios::tcsetattr;

use crate::Error;

// https://en.wikipedia.org/wiki/ANSI_escape_code#CSI_(Control_Sequence_Introducer)_sequences
pub mod colors {
    pub const CSI: &str = "\x1b[";
    pub const GREEN: &str = "\x1b[32m";
    pub const PURPLE: &str = "\x1b[35m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const RED: &str = "\x1b[31m";
    pub const WHITE: &str = "\x1b[1\x1b[37m"; // bright white
    pub const ORANGE: &str = "\x1b[93m"; // bright yellow
    pub const REVERSE: &str = "\x1b[7m"; // reverse-video (doesn't always work)
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

pub struct DroppableTermios(Termios);
impl Drop for DroppableTermios {
    fn drop(&mut self) {
        let fd = std::io::stdout();
        tcsetattr(fd, OptionalActions::Now, &self.0).unwrap();
    }
}

/// Activates raw mode and returns a droppable object. When the object is dropped
/// the terminal settings are restored to their original state.
pub fn activate_raw_mode() -> Result<DroppableTermios, Error> {
    let fd = std::io::stdout();
    let droppable = DroppableTermios(
        tcgetattr(&fd).map_err(|x| Error(format!("Failed to get termios: {x:?}")))?,
    );
    let mut termios = tcgetattr(&fd).map_err(|x| Error(format!("Failed to get termios: {x:?}")))?;

    // man 3 termios
    termios
        .local_modes
        .remove(LocalModes::ICANON | LocalModes::ECHO | LocalModes::TOSTOP);

    // the combination of VMIN and VTIME enables blocking reads (again, see man 3 termios)
    termios.special_codes[SpecialCodeIndex::VMIN] = 1;
    termios.special_codes[SpecialCodeIndex::VTIME] = 0;
    tcsetattr(fd, OptionalActions::Now, &termios).unwrap();
    Ok(droppable)
}
