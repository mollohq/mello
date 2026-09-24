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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedInvite {
    pub crew_name: String,
    pub avatar_seed: String,
    pub crew_id: String,
    pub highlight: String,
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
