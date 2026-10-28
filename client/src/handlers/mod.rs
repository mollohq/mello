mod auth;
mod chat;
mod clip;
mod crew;
mod game;
pub(crate) mod games;
mod presence;
mod stats;
mod stream_cards;
// stage 9: the stream card state and helpers are used by the app context,
// the callbacks and the stream timer.
pub(crate) mod streaming;
mod voice;

use mello_core::Event;

pub(crate) use crew::{invite_join_error_message, invite_resolve_error_message, InviteSource};

use crate::app_context::AppContext;

/// Count the minutes of the live session cards again.
pub(crate) fn refresh_live_session_cards(ctx: &AppContext) {
    clip::refresh_live_session_cards(ctx);
}

/// Put the belt and recap cards on screen again from the last feed: a belt or
/// a recap that was opened is not sealed any more (`sealed.rs`).
pub(crate) fn rebuild_sealed_cards(ctx: &AppContext) {
    clip::refresh_sealed_cards(ctx);
}

pub fn handle_event(ctx: &AppContext, event: Event) {
    // These events change the voice channel models. See
    // voice::sync_member_audio.
    // A change of who plays what, of the roster or of an avatar moves the
    // live session cards.
    let live_cards_change = matches!(
        event,
        Event::PresenceChanged { .. }
            | Event::CrewStateLoaded { .. }
            | Event::GameDetected { .. }
            | Event::GameEnded { .. }
            | Event::UserAvatarLoaded { .. }
            | Event::CrewJoined { .. }
    );
    let voice_models_change = matches!(
        event,
        Event::VoiceStateChanged { .. }
            | Event::VoiceJoined { .. }
            | Event::VoiceUpdated { .. }
            | Event::VoiceChannelsUpdated { .. }
            | Event::VoiceChannelCreated { .. }
            | Event::VoiceChannelDeleted { .. }
            | Event::CrewStateLoaded { .. }
            | Event::CrewJoined { .. }
    );
    dispatch(ctx, event);
    if live_cards_change {
        clip::refresh_live_session_cards(ctx);
    }
    if voice_models_change {
        voice::sync_member_audio(ctx);
    }
}

fn dispatch(ctx: &AppContext, event: Event) {
    match event {
        // Auth
        Event::Restoring
        | Event::DeviceAuthed { .. }
        | Event::LoggedIn { .. }
        | Event::LoginFailed { .. }
        | Event::EmailLinked
        | Event::EmailLinkFailed { .. }
        | Event::SocialLinked
        | Event::SocialLinkFailed { .. }
        | Event::OnboardingReady { .. }
        | Event::OnboardingFailed { .. }
        | Event::OnboardingInviteFailed { .. }
        | Event::AccountDeleted
        | Event::AccountDeleteFailed { .. } => auth::handle(ctx, event),

        // Crew
        Event::CrewsLoaded { .. }
        | Event::CrewCreated { .. }
        | Event::CrewCreateFailed { .. }
        | Event::CrewJoined { .. }
        | Event::CrewLeft { .. }
        | Event::UserSearchResults { .. }
        | Event::CrewAvatarLoaded { .. }
        | Event::DiscoverCrewsLoaded { .. }
        | Event::DiscoverCrewsFailed { .. }
        | Event::CrewInviteResolved { .. }
        | Event::CrewInviteResolveFailed { .. }
        | Event::InviteJoined { .. }
        | Event::InviteJoinFailed { .. }
        | Event::InviteCodeCreated { .. }
        | Event::InviteCodeCreateFailed { .. }
        | Event::CrewUpdated { .. }
        | Event::CrewUpdateFailed { .. }
        | Event::CrewDeleted { .. }
        | Event::CrewDeleteFailed { .. }
        | Event::CrewRoleChanged { .. }
        | Event::CrewRoleChangeFailed { .. }
        | Event::CrewMemberKicked { .. }
        | Event::CrewMemberKickFailed { .. } => crew::handle(ctx, event),

        // Voice
        Event::VoiceStateChanged { .. }
        | Event::VoiceConnected { .. }
        | Event::VoiceDisconnected { .. }
        | Event::VoiceActivity { .. }
        | Event::VoiceJoined { .. }
        | Event::VoiceUpdated { .. }
        | Event::VoiceChannelsUpdated { .. }
        | Event::VoiceChannelCreated { .. }
        | Event::VoiceChannelRenamed { .. }
        | Event::VoiceChannelBitrateChanged { .. }
        | Event::VoiceChannelDeleted { .. }
        | Event::VoiceSfuDisconnected { .. }
        | Event::SfuRegionsMeasured { .. }
        | Event::VoiceLinkStalled { .. }
        | Event::VoiceMembershipChanged { .. }
        | Event::MicPermissionChanged { .. }
        | Event::MicLevel { .. }
        | Event::AudioDebugStats { .. }
        | Event::AudioDevicesListed { .. }
        | Event::AudioDeviceFallback { .. } => voice::handle(ctx, event),

        // Chat
        Event::MessagesLoaded { .. }
        | Event::MessageReceived { .. }
        | Event::HistoryLoaded { .. }
        | Event::HistoryLoadFailed
        | Event::ChatMessageEdited { .. }
        | Event::ChatMessageDeleted { .. }
        | Event::GifsLoaded { .. }
        | Event::MessagePreviewUpdated { .. } => chat::handle(ctx, event),

        // Streaming
        Event::CaptureSourcesListed { .. }
        | Event::WindowThumbnailsUpdated { .. }
        | Event::StreamStarted { .. }
        | Event::StreamEnded { .. }
        | Event::StreamViewerJoined { .. }
        | Event::StreamViewerLeft { .. }
        | Event::StreamWatching { .. }
        | Event::StreamWatchingStopped
        | Event::StreamMembersChanged { .. }
        | Event::StreamPaused { .. }
        | Event::StreamHostPaused { .. }
        | Event::StreamTargetExited
        | Event::StreamFrame { .. }
        | Event::StreamDebugStats { .. }
        | Event::StreamViewerTimeline { .. }
        | Event::StreamHostPacingStats { .. }
        | Event::StreamError { .. }
        // stage 6 (streaming-quality)
        | Event::StreamStarting { .. }
        | Event::StreamReconnecting { .. }
        | Event::StreamResumed { .. }
        | Event::StreamFailed { .. } => streaming::handle(ctx, event),

        // Presence & crew state
        Event::CrewStateLoaded { .. }
        | Event::SidebarUpdated { .. }
        | Event::CrewEventReceived { .. }
        | Event::FeedChanged { .. }
        | Event::PresenceChanged { .. }
        | Event::PresenceUpdated { .. }
        | Event::MemberJoined { .. }
        | Event::MemberLeft { .. }
        | Event::CatchupLoaded { .. }
        | Event::MomentPosted { .. }
        | Event::MomentPostFailed { .. }
        | Event::UserAvatarLoaded { .. }
        | Event::ProfileUpdated { .. } => presence::handle(ctx, event),

        // Clips
        Event::ClipBufferStarted
        | Event::ClipBufferStopped
        | Event::ClipCaptured { .. }
        | Event::ClipCaptureFailed { .. }
        | Event::ClipPosted { .. }
        | Event::ClipUploaded { .. }
        | Event::ClipPlaybackStarted { .. }
        | Event::ClipPlaybackProgress { .. }
        | Event::ClipPlaybackFinished
        | Event::FeedLoaded { .. }
        // Raw timeline is kept for later deep-scroll; no UI handler yet.
        | Event::TimelineLoaded { .. } => clip::handle(ctx, event),

        // Game sensing + telemetry
        Event::GameDetected { .. }
        | Event::GameEnded { .. }
        | Event::PostGameTimeout
        | Event::MatchEnded { .. }
        | Event::SessionSummary { .. }
        | Event::GameIconLoaded { .. }
        | Event::GameArtLoaded { .. }
        | Event::TelemetrySetupHint { .. } => game::handle(ctx, event),

        // Games settings + Riot account link
        Event::GamesSettings { .. }
        | Event::InstalledGames { .. }
        | Event::RiotStatus { .. }
        | Event::RiotLinkFailed { .. } => games::handle(ctx, event),

        // Personal stats (You strip)
        Event::UserGameStatsLoaded { .. } => stats::handle(ctx, event),

        // Misc
        Event::SignalReceived { .. } => {}
        Event::ConnectionStateChanged {
            connected,
            reconnecting,
        } => {
            let msg = if reconnecting {
                "Reconnecting…"
            } else if !connected {
                "Connection lost"
            } else {
                ""
            };
            ctx.app.set_connection_warning(msg.into());
        }
        Event::ProtocolMismatch { message, .. } => {
            ctx.app.set_protocol_warning(message.into());
        }
        // The notice is a log line in mello-core for now; the UI shows
        // nothing yet (mello-admin increment 4).
        Event::LogsRequested { .. } => {}
        Event::DiagnosticCaptureState { phase, message } => {
            let status = match phase.as_str() {
                "capturing" => "Recording… reproduce the issue, then press Stop.".to_string(),
                "uploading" => "Uploading…".to_string(),
                "done" => "Uploaded — thanks! The team can see it now.".to_string(),
                "skipped" => "Saved locally (upload not configured).".to_string(),
                "failed" => format!("Upload failed: {}", message),
                _ => message,
            };
            if matches!(phase.as_str(), "done" | "failed" | "skipped") {
                ctx.app.set_diag_capturing(false);
            }
            ctx.app.set_diag_status(status.into());
        }
        Event::StatsUpdated { .. } => {}
        Event::Error { message } => {
            log::error!("UI: error: {}", message);
        }
    }
}
