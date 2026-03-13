use muda::{Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};

use crate::APP_NAME;

/// Build the macOS NSMenuBar. Must be called on the main thread before
/// the Slint event loop starts.
pub fn build_menu_bar() -> Menu {
    let menu = Menu::new();

    // -- Mello --
    let app_menu = Submenu::with_id("app", APP_NAME, true);
    app_menu
        .append(&PredefinedMenuItem::about(
            Some(&format!("About {APP_NAME}")),
            None,
        ))
        .ok();
    app_menu.append(&PredefinedMenuItem::separator()).ok();
    app_menu
        .append(&MenuItem::with_id(
            MenuId::new("check_updates"),
            "Check for Updates…",
            true,
            None,
        ))
        .ok();
    app_menu.append(&PredefinedMenuItem::separator()).ok();
    app_menu
        .append(&MenuItem::with_id(
            MenuId::new("prefs"),
            "Preferences…",
            true,
            Some("cmd+,".parse().unwrap()),
        ))
        .ok();
    app_menu.append(&PredefinedMenuItem::separator()).ok();
    app_menu.append(&PredefinedMenuItem::services(None)).ok();
    app_menu.append(&PredefinedMenuItem::separator()).ok();
    app_menu
        .append(&PredefinedMenuItem::hide(Some("Hide Mello")))
        .ok();
    app_menu.append(&PredefinedMenuItem::hide_others(None)).ok();
    app_menu.append(&PredefinedMenuItem::show_all(None)).ok();
    app_menu.append(&PredefinedMenuItem::separator()).ok();
    app_menu
        .append(&PredefinedMenuItem::quit(Some(&format!("Quit {APP_NAME}"))))
        .ok();
    menu.append(&app_menu).ok();

    // -- Edit --
    // All PredefinedMenuItems — these integrate with the macOS responder chain
    // and give Slint TextInput fields correct system behaviour for free.
    let edit_menu = Submenu::with_id("edit", "Edit", true);
    edit_menu.append(&PredefinedMenuItem::undo(None)).ok();
    edit_menu.append(&PredefinedMenuItem::redo(None)).ok();
    edit_menu.append(&PredefinedMenuItem::separator()).ok();
    edit_menu.append(&PredefinedMenuItem::cut(None)).ok();
    edit_menu.append(&PredefinedMenuItem::copy(None)).ok();
    edit_menu.append(&PredefinedMenuItem::paste(None)).ok();
    edit_menu
        .append(&PredefinedMenuItem::select_all(None))
        .ok();
    edit_menu.append(&PredefinedMenuItem::separator()).ok();
    edit_menu
        .append(&MenuItem::with_id(
            MenuId::new("find"),
            "Find…",
            true,
            Some("cmd+f".parse().unwrap()),
        ))
        .ok();
    menu.append(&edit_menu).ok();

    // -- View --
    let view_menu = Submenu::with_id("view", "View", true);
    view_menu
        .append(&MenuItem::with_id(
            MenuId::new("mute"),
            "Toggle Mute",
            true,
            Some("cmd+ctrl+m".parse().unwrap()),
        ))
        .ok();
    view_menu
        .append(&MenuItem::with_id(
            MenuId::new("deafen"),
            "Toggle Deafen",
            true,
            Some("cmd+ctrl+d".parse().unwrap()),
        ))
        .ok();
    menu.append(&view_menu).ok();

    // -- Help --
    let help_menu = Submenu::with_id("help", "Help", true);
    help_menu
        .append(&MenuItem::with_id(
            MenuId::new("github"),
            "Mello on GitHub",
            true,
            None,
        ))
        .ok();
    menu.append(&help_menu).ok();

    menu
}
