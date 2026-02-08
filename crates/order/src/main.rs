use rander::{tui::OrderTui, ratatui};
fn main() -> anyhow::Result<()> {
    let mut tui = OrderTui::default();
    ratatui::run(|terminal| tui.run(terminal))?;
    Ok(())
}
