use slip::{MailCache, tui};

fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("Usage: slip-tui [init|--init]");
        println!();
        println!(
            "Starts the Slip chat TUI. Use init/--init to forget cached login and show setup."
        );
        return Ok(());
    }
    if args.iter().any(|arg| arg == "init" || arg == "--init") {
        MailCache::default().clear_login()?;
    }
    tui::run("INBOX".to_string())
}
