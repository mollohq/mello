use auto_launch::AutoLaunch;

pub fn set_start_on_boot(enabled: bool) -> Result<(), Box<dyn std::error::Error>> {
    let app_name = "Mello";
    let app_path = std::env::current_exe()?;

    let auto = AutoLaunch::new(app_name, app_path.to_str().unwrap(), false, &[] as &[&str]);

    if enabled {
        auto.enable()?;
    } else {
        auto.disable()?;
    }

    Ok(())
}

pub fn is_start_on_boot_enabled() -> bool {
    let app_name = "Mello";
    let app_path = std::env::current_exe().unwrap_or_default();
    AutoLaunch::new(app_name, app_path.to_str().unwrap_or(""), false, &[] as &[&str])
        .is_enabled()
        .unwrap_or(false)
}
