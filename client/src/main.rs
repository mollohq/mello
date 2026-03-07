slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    log::info!("Starting Mello...");

    let app = MainWindow::new()?;
    
    // TODO: Initialize mello-core
    // TODO: Wire up callbacks
    
    app.run()?;
    Ok(())
}
