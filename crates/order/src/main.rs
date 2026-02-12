use rander::{ratatui, tui::OrderTui};
fn main() -> anyhow::Result<()> {
    let mut tui = OrderTui::default();
    ratatui::run(|terminal| tui.run(terminal))?;
    Ok(())
}
