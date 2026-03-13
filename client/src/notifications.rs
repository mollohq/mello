use notify_rust::Notification;

pub fn notify_member_joined(name: &str) {
    Notification::new()
        .summary("Crew")
        .body(&format!("{} jumped in", name))
        .timeout(notify_rust::Timeout::Milliseconds(4000))
        .show()
        .ok();
}

pub fn notify_message(crew: &str, sender: &str, preview: &str) {
    Notification::new()
        .summary(crew)
        .body(&format!("{}: {}", sender, preview))
        .timeout(notify_rust::Timeout::Milliseconds(4000))
        .show()
        .ok();
}

pub fn notify_invite(inviter: &str, crew: &str) {
    Notification::new()
        .summary("Mello invite")
        .body(&format!("{} invited you to {}", inviter, crew))
        .timeout(notify_rust::Timeout::Milliseconds(0)) // persistent
        .show()
        .ok();
}
