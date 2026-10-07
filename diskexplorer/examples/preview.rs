//! Renders one frame of the app UI to plain text via ratatui's TestBackend,
//! so you can see the layout without launching the interactive TUI.

use diskexplorer::App;
use ratatui::{Terminal, backend::TestBackend};

fn main() {
    let backend = TestBackend::new(100, 28);
    // TestBackend never fails: its error type is Infallible, so unwrap it is.
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new(std::env::current_dir().unwrap()).unwrap();
    app.draw(&mut terminal).unwrap();
    let buffer = terminal.backend().buffer().clone();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
}
