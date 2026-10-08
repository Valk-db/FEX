//! Binary entry point: parses args, then runs the TUI or headless scan.

use crossterm::event::{self, Event, KeyEventKind};
use diskexplorer::{App, human_size, scan};
use ratatui::DefaultTerminal;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn run(terminal: &mut DefaultTerminal, root: PathBuf) -> io::Result<()> {
    let mut app = App::new(root)?;
    loop {
        // Never block the UI: scan streams in, hashing is manual (h).
        app.poll_scan();
        app.poll_hashes();
        app.draw(terminal)?;
        // Wake up regularly so the progressive scan repaints live instead
        // of only when a key is pressed. No added input latency: poll
        // returns immediately when a key arrives.
        if event::poll(Duration::from_millis(150))? {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if app.handle_key(key.code) {
                break;
            }
        }
    }
    Ok(())
}

/// Headless mode: print the largest entries under `root`, no TUI.
fn scan_only(root: &Path) -> io::Result<()> {
    let data = scan(root)?;
    let root = root.canonicalize()?;
    let mut top: Vec<(&PathBuf, &u64)> = data.dir_sizes_logical.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    println!(
        "scanned {} files in {} dirs under {}",
        data.file_count,
        data.dir_count,
        root.display()
    );
    for (path, size) in top.iter().take(20) {
        let rel = path.strip_prefix(&root).unwrap_or(path);
        println!("{:>10}  {}", human_size(**size), rel.display());
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let scan_only_flag = args.iter().any(|a| a == "--scan-only");
    let path = args
        .iter()
        .skip(1)
        .find(|a| *a != "--scan-only")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);

    if scan_only_flag {
        return scan_only(&path);
    }

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, path);
    ratatui::restore();
    result
}
