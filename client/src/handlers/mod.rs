mod auth;
mod chat;
mod clip;
mod crew;
mod game;
mod presence;
mod stream_cards;
mod streaming;
mod voice;

use mello_core::Event;

use crate::app_context::AppContext;

pub fn handle_event(ctx: &AppContext, event: Event) {
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
        | Event::OnboardingFailed { .. } => auth::handle(ctx, event),

        // Crew
        Event::CrewsLoaded { .. }
        | Event::CrewCreated { .. }
        | Event::CrewCreateFailed { .. }
        | Event::CrewJoined { .. }
        | Event::CrewLeft { .. }
        | Event::UserSearchResults { .. }
        | Event::CrewAvatarLoaded { .. }
        | Event::DiscoverCrewsLoaded { .. }
        | Event::CrewInviteResolved { .. }
        | Event::CrewInviteResolveFailed { .. } => crew::handle(ctx, event),

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
        | Event::VoiceChannelDeleted { .. }
        | Event::VoiceSfuDisconnected { .. }
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
        | Event::StreamFrame { .. }
        | Event::StreamDebugStats { .. }
        | Event::StreamHostPacingStats { .. }
        | Event::StreamError { .. } => streaming::handle(ctx, event),

        // Presence & crew state
        Event::CrewStateLoaded { .. }
        | Event::SidebarUpdated { .. }
        | Event::CrewEventReceived { .. }
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
        | Event::TimelineLoaded { .. } => clip::handle(ctx, event),

        // Game sensing
        Event::GameDetected { .. } | Event::GameEnded { .. } | Event::PostGameTimeout => {
            game::handle(ctx, event)
        }

        // Misc
        Event::SignalReceived { .. } => {}
        Event::ProtocolMismatch { message, .. } => {
            ctx.app.set_protocol_warning(message.into());
        }
        Event::Error { message } => {
            log::error!("UI: error: {}", message);
        }
    }
}
