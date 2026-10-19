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

/// Set the OS auto-launch entry.
///
/// Tests never touch the real entry: under `cfg(test)` or the `testkit`
/// feature this records the last request, readable with [`last_requested`].
#[cfg(not(any(test, feature = "testkit")))]
pub fn set_start_on_boot(enabled: bool) -> Result<(), Box<dyn std::error::Error>> {
    let auto = build_auto_launch();

    if enabled {
        auto.enable()?;
    } else {
        auto.disable()?;
    }

    Ok(())
}

#[allow(dead_code)]
pub fn is_start_on_boot_enabled() -> bool {
    build_auto_launch().is_enabled().unwrap_or(false)
}

#[cfg(any(test, feature = "testkit"))]
thread_local! {
    static LAST_REQUESTED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Test seam: records the request and leaves the OS entry alone.
#[cfg(any(test, feature = "testkit"))]
pub fn set_start_on_boot(enabled: bool) -> Result<(), Box<dyn std::error::Error>> {
    LAST_REQUESTED.with(|c| c.set(Some(enabled)));
    Ok(())
}

/// The last value passed to [`set_start_on_boot`] on this thread, if any.
#[cfg(any(test, feature = "testkit"))]
#[allow(dead_code)]
pub fn last_requested() -> Option<bool> {
    LAST_REQUESTED.with(|c| c.get())
}
