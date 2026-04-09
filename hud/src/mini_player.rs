use std::sync::mpsc;

use slint::ComponentHandle;

use crate::protocol::{HudAction, HudActionKind, HudState};

slint::include_modules!();

pub struct MiniPlayer {
    window: MiniPlayerWindow,
    _action_tx: mpsc::Sender<HudAction>,
    #[cfg(target_os = "windows")]
    needs_styles: std::cell::Cell<bool>,
}

impl MiniPlayer {
    pub fn new(action_tx: mpsc::Sender<HudAction>) -> Result<Self, Box<dyn std::error::Error>> {
        let window = MiniPlayerWindow::new()?;

        {
            let tx = action_tx.clone();
            window.on_mute_toggle(move || {
                let _ = tx.send(HudAction::Action {
                    action: HudActionKind::MuteToggle,
                });
            });
        }
        {
            let tx = action_tx.clone();
            window.on_deafen_toggle(move || {
                let _ = tx.send(HudAction::Action {
                    action: HudActionKind::DeafenToggle,
                });
            });
        }
        {
            let tx = action_tx.clone();
            window.on_leave_voice(move || {
                let _ = tx.send(HudAction::Action {
                    action: HudActionKind::LeaveVoice,
                });
            });
        }
        {
            let tx = action_tx.clone();
            window.on_open_crew(move || {
                let _ = tx.send(HudAction::Action {
                    action: HudActionKind::OpenCrew,
                });
            });
        }
        {
            let tx = action_tx.clone();
            window.on_open_stream(move || {
                let _ = tx.send(HudAction::Action {
                    action: HudActionKind::OpenStream,
                });
            });
        }
        {
            let tx = action_tx.clone();
            window.on_open_settings(move || {
                let _ = tx.send(HudAction::Action {
                    action: HudActionKind::OpenSettings,
                });
            });
        }
        {
            window.on_start_drag(move || {
                #[cfg(target_os = "windows")]
                Self::initiate_window_drag();
            });
        }

        log::info!("[mini_player] window created");

        Ok(Self {
            window,
            _action_tx: action_tx,
            #[cfg(target_os = "windows")]
            needs_styles: std::cell::Cell::new(false),
        })
    }

    /// Show the mini-player. Win32 styles are applied on the next tick()
    /// to give Slint time to finish its window setup.
    pub fn show(&self) {
        if let Err(e) = self.window.show() {
            log::error!("[mini_player] show failed: {}", e);
            return;
        }
        log::info!("[mini_player] show");

        #[cfg(target_os = "windows")]
        self.needs_styles.set(true);
    }

    /// Called every frame from the timer. Applies deferred Win32 styles.
    pub fn tick(&self) {
        #[cfg(target_os = "windows")]
        if self.needs_styles.get() {
            self.apply_win32_styles();
            self.needs_styles.set(false);
        }
    }

    pub fn hide(&self) {
        self.window.hide().ok();
        log::debug!("[mini_player] hide");
    }

    pub fn update_state(&self, state: &HudState) {
        log::debug!(
            "[mini_player] update_state: crew={} voice={} msgs={}",
            state.crew.as_ref().map_or("-", |c| c.name.as_str()),
            state.voice.as_ref().map_or(0, |v| v.members.len()),
            state.recent_messages.as_ref().map_or(0, |m| m.len()),
        );
        if let Some(ref crew) = state.crew {
            self.window.set_crew_name(crew.name.as_str().into());
            self.window
                .set_crew_initials(crew.initials.as_str().into());
            self.window.set_online_count(crew.online_count as i32);
        }

        if let Some(ref voice) = state.voice {
            self.window
                .set_channel_name(voice.channel_name.as_str().into());
            self.window.set_self_muted(voice.self_muted);

            let members: Vec<VoiceMemberData> = voice
                .members
                .iter()
                .map(|m| {
                    if m.is_self {
                        self.window.set_self_name(m.display_name.as_str().into());
                        self.window
                            .set_self_initials(m.initials.as_str().into());
                    }
                    VoiceMemberData {
                        id: m.id.as_str().into(),
                        display_name: m.display_name.as_str().into(),
                        initials: m.initials.as_str().into(),
                        speaking: m.speaking,
                        muted: m.muted,
                        is_self: m.is_self,
                    }
                })
                .collect();
            let model = std::rc::Rc::new(slint::VecModel::from(members));
            self.window.set_members(model.into());
        }

        if let Some(ref msgs) = state.recent_messages {
            const SENDER_COLORS: &[slint::Color] = &[
                slint::Color::from_rgb_u8(0xFF, 0x1E, 0x56), // accent
                slint::Color::from_rgb_u8(0x00, 0xD4, 0xAA), // teal
                slint::Color::from_rgb_u8(0x60, 0x8C, 0xFF), // blue
                slint::Color::from_rgb_u8(0xFF, 0xBD, 0x2E), // amber
                slint::Color::from_rgb_u8(0xC0, 0x60, 0xFF), // purple
            ];
            let chat: Vec<ChatPreviewData> = msgs
                .iter()
                .enumerate()
                .map(|(i, m)| ChatPreviewData {
                    display_name: m.display_name.as_str().into(),
                    text: m.text.as_str().into(),
                    sender_color: SENDER_COLORS[i % SENDER_COLORS.len()],
                })
                .collect();
            let model = std::rc::Rc::new(slint::VecModel::from(chat));
            self.window.set_chat_messages(model.into());
        }

        if let Some(ref sc) = state.stream_card {
            self.window.set_has_stream(true);
            self.window.set_stream_card(StreamCardData {
                streamer: sc.streamer.as_str().into(),
                title: sc.title.as_str().into(),
            });
        } else {
            self.window.set_has_stream(false);
        }
    }

    #[cfg(target_os = "windows")]
    fn initiate_window_drag() {
        use windows::core::w;
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
        use windows::Win32::UI::WindowsAndMessaging::*;

        unsafe {
            let hwnd = match FindWindowW(None, w!("m3llo HUD")) {
                Ok(h) => h,
                Err(e) => {
                    log::warn!("[mini_player] drag: FindWindowW failed: {}", e);
                    return;
                }
            };
            let _ = ReleaseCapture();
            let _ = SendMessageW(
                hwnd,
                WM_NCLBUTTONDOWN,
                Some(WPARAM(HTCAPTION as usize)),
                Some(LPARAM(0)),
            );
        }
    }

    #[cfg(target_os = "windows")]
    fn apply_win32_styles(&self) {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::*;

        unsafe {
            let hwnd = match FindWindowW(None, w!("m3llo HUD")) {
                Ok(h) => h,
                Err(e) => {
                    log::warn!("[mini_player] FindWindowW failed: {}", e);
                    return;
                }
            };

            let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            let new_ex =
                ex_style | WS_EX_TOPMOST.0 | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0;
            SetWindowLongW(hwnd, GWL_EXSTYLE, new_ex as i32);

            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );

            log::info!(
                "[mini_player] Win32 styles applied to hwnd={:?}",
                hwnd
            );
        }
    }
}
