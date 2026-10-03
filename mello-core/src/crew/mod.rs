use serde::{Deserialize, Serialize};

pub type CrewId = String;
pub type MemberId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Crew {
    pub id: CrewId,
    pub name: String,
    pub description: String,
    pub member_count: i32,
    pub max_members: i32,
    pub open: bool,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub id: MemberId,
    pub username: String,
    pub display_name: String,
    pub online: bool,
}

/// A person shown with an invite: the inviter, or a member preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvitePerson {
    pub display_name: String,
    pub avatar_seed: String,
}

/// Public crew info for an invite code (`resolve_crew_invite`,
/// CREW-INVITES.md §3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedInvite {
    pub crew_name: String,
    pub avatar_seed: String,
    pub crew_id: String,
    pub highlight: String,
    #[serde(default)]
    pub member_count: i32,
    /// Up to 5 member previews, in the order the server sent them.
    #[serde(default)]
    pub members: Vec<InvitePerson>,
    /// The user who made the code. `None` when the code has no inviter, or
    /// the inviter has no name.
    #[serde(default)]
    pub inviter: Option<InvitePerson>,
}

/// Why an invite could not be resolved or joined.
///
/// Built from the gRPC code of the Nakama RPC error, so the UI can tell a bad
/// link from a server failure. See `invite_codes.go` for the codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InviteError {
    /// The code, or the crew it points to, does not exist.
    InvalidCode,
    /// The crew is at its member limit.
    CrewFull,
    /// The server refuses this user, for example after a ban.
    NotAllowed,
    /// A network, server or internal failure. A retry can succeed.
    Failed,
}

impl InviteError {
    /// Classify an error from `resolve_crew_invite` or `join_by_invite_code`.
    pub fn from_error(err: &crate::error::Error) -> Self {
        match err.server_code() {
            // INVALID_ARGUMENT (empty code) and NOT_FOUND.
            Some(3) | Some(5) => Self::InvalidCode,
            Some(8) => Self::CrewFull,
            Some(7) => Self::NotAllowed,
            _ => Self::Failed,
        }
    }

    /// Classify an error from Nakama's group join (`/v2/group/{id}/join`).
    ///
    /// Nakama reports a full group as INVALID_ARGUMENT with the message
    /// "Group is full.", so code 3 alone cannot tell full from a bad id.
    /// `InvalidCode` here means the crew does not exist.
    pub fn from_join_error(err: &crate::error::Error) -> Self {
        match err.server_code() {
            Some(3) => {
                let full = err
                    .server_message()
                    .is_some_and(|m| m.to_ascii_lowercase().contains("full"));
                if full {
                    Self::CrewFull
                } else {
                    Self::InvalidCode
                }
            }
            Some(5) => Self::InvalidCode,
            Some(7) | Some(9) => Self::NotAllowed,
            _ => Self::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    fn server(body: &str) -> Error {
        Error::Server(body.to_string())
    }

    #[test]
    fn invite_error_follows_the_grpc_code() {
        let cases = [
            (
                r#"{"code":5,"message":"invalid invite code"}"#,
                InviteError::InvalidCode,
            ),
            (
                r#"{"code":3,"message":"invite code required"}"#,
                InviteError::InvalidCode,
            ),
            (
                r#"{"code":8,"message":"crew is full"}"#,
                InviteError::CrewFull,
            ),
            (
                r#"{"code":7,"message":"you cannot join this crew"}"#,
                InviteError::NotAllowed,
            ),
        ];
        for (body, want) in cases {
            assert_eq!(InviteError::from_error(&server(body)), want, "{body}");
        }
    }

    /// The e2e spike saw this exact body labelled "Invalid invite code".
    #[test]
    fn an_internal_server_error_is_not_an_invalid_code() {
        let body = r#"{"code":13,"error":{"Message":"failed to join crew","Code":13},"message":"failed to join crew"}"#;
        assert_eq!(InviteError::from_error(&server(body)), InviteError::Failed);
    }

    /// The exact body the local server answered when onboarding joined a full
    /// crew (2026-10-03).
    #[test]
    fn a_full_group_join_is_crew_full_not_invalid() {
        let body = r#"{"code":3,"message":"Group is full."}"#;
        assert_eq!(
            InviteError::from_join_error(&server(body)),
            InviteError::CrewFull
        );
        // The invite classifier reads code 3 as a bad code; the join one must not.
        assert_eq!(
            InviteError::from_error(&server(body)),
            InviteError::InvalidCode
        );
    }

    #[test]
    fn group_join_errors_follow_the_grpc_code() {
        let cases = [
            (
                r#"{"code":5,"message":"Group not found."}"#,
                InviteError::InvalidCode,
            ),
            (
                r#"{"code":3,"message":"Invalid group ID."}"#,
                InviteError::InvalidCode,
            ),
            (r#"{"code":7,"message":"banned"}"#, InviteError::NotAllowed),
            (r#"{"code":13,"message":"internal"}"#, InviteError::Failed),
        ];
        for (body, want) in cases {
            assert_eq!(InviteError::from_join_error(&server(body)), want, "{body}");
        }
        assert_eq!(
            InviteError::from_join_error(&Error::NotConnected),
            InviteError::Failed
        );
    }

    #[test]
    fn errors_without_a_grpc_code_are_failures() {
        assert_eq!(
            InviteError::from_error(&server("<html>502 Bad Gateway</html>")),
            InviteError::Failed
        );
        assert_eq!(
            InviteError::from_error(&Error::NotConnected),
            InviteError::Failed
        );
    }
}
