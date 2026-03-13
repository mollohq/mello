use auto_launch::AutoLaunchBuilder;

use crate::APP_NAME;

fn build_auto_launch() -> auto_launch::AutoLaunch {
    let app_path = std::env::current_exe().unwrap_or_default();
    AutoLaunchBuilder::new()
        .set_app_name(APP_NAME)
        .set_app_path(app_path.to_str().unwrap_or(""))
        .set_use_launch_agent(false)
        .set_args(&[] as &[&str])
        .build()
        .expect("failed to build AutoLaunch")
}

pub fn set_start_on_boot(enabled: bool) -> Result<(), Box<dyn std::error::Error>> {
    let auto = build_auto_launch();

    if enabled {
        auto.enable()?;
    } else {
        auto.disable()?;
    }

    Ok(())
}

pub fn is_start_on_boot_enabled() -> bool {
    build_auto_launch().is_enabled().unwrap_or(false)
}
