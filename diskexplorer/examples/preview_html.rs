//! Renders one frame of the app UI as styled HTML, using the real app code
//! with ratatui's TestBackend. Usage: preview_html [path]

use diskexplorer::App;
use ratatui::{Terminal, backend::TestBackend, style::Color};

fn css(c: Color) -> &'static str {
    match c {
        Color::Black => "#000000",
        Color::Red => "#ff5555",
        Color::Green => "#50fa7b",
        Color::Yellow => "#f1fa8c",
        Color::Blue => "#6272a4",
        Color::Magenta => "#ff79c6",
        Color::Cyan => "#8be9fd",
        Color::Gray => "#bbbbbb",
        Color::DarkGray => "#44475a",
        Color::LightRed => "#ff7777",
        Color::LightGreen => "#77ff99",
        Color::LightYellow => "#ffffa5",
        Color::LightBlue => "#add8e6",
        Color::LightMagenta => "#ff92df",
        Color::LightCyan => "#a4ffff",
        Color::White => "#f8f8f2",
        _ => "#f8f8f2",
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .iter()
        .skip(1)
        .find(|a| *a != "dupes" && *a != "growth")
        .map(std::path::PathBuf::from)
        .unwrap_or(std::env::current_dir().unwrap());
    let view = if args.iter().any(|a| a == "dupes") {
        diskexplorer::View::Duplicates
    } else if args.iter().any(|a| a == "growth") {
        diskexplorer::View::Growth
    } else {
        diskexplorer::View::Browse
    };
    let backend = TestBackend::new(100, 28);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut app = App::new(path).unwrap();
    app.set_view(view);
    // drain the progressive scan first
    for _ in 0..600 {
        app.poll_scan();
        if !app.is_scanning() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // then run the scheduled hashing to completion so dupes show hashed
    app.begin_hashing();
    for _ in 0..600 {
        app.poll_hashes();
        if app.hash_progress().is_none() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    app.draw(&mut terminal).unwrap();
    let buffer = terminal.backend().buffer().clone();

    let mut html = String::from(
        "<pre style=\"background:#1e1f29;color:#f8f8f2;font-family:ui-monospace,Menlo,monospace;font-size:12px;line-height:1.35;margin:0;padding:12px;border-radius:8px;overflow-x:auto;\">",
    );
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            let bold = false; // Cell exposes no modifier accessor in 0.30
            let bg_css = match cell.bg {
                Color::Reset => "transparent",
                _ => css(cell.bg),
            };
            // one span per cell: crude but effective
            html.push_str(&format!(
                "<span style=\"color:{};background:{}{}\">",
                css(cell.fg),
                bg_css,
                if bold { ";font-weight:bold" } else { "" }
            ));
            let esc = cell
                .symbol()
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            html.push_str(&esc);
            html.push_str("</span>");
        }
        html.push('\n');
    }
    html.push_str("</pre>");
    println!("{}", html);
}
