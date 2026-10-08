//! The other members of a stream session, as a viewer knows them.
//!
//! The SFU lists the members at the join (`joined.members`) and then sends
//! `member_joined {user_id, role}` and `member_left {user_id, reason}` to
//! every stream member (mello-sfu 01-SFU.md section 4.1). An older SFU sends
//! these only to the host: a viewer then keeps the join snapshot.

use crate::events::StreamMember;
use crate::transport::sfu_connection::MemberInfo;
use crate::transport::SfuEvent;

/// The role string of the stream host in SFU signaling.
pub const HOST_ROLE: &str = "host";

/// The other members of one stream session, in join order. Holds each user
/// once and at most one host.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StreamMembers {
    members: Vec<StreamMember>,
}

impl StreamMembers {
    /// The list from the `joined.members` snapshot.
    pub fn from_joined(members: &[MemberInfo]) -> Self {
        let mut list = Self::default();
        for m in members {
            list.join(&m.user_id, &m.role);
        }
        list
    }

    /// Apply one SFU event. Returns true when the list changed. Events other
    /// than `MemberJoined` and `MemberLeft` do not change it.
    pub fn apply(&mut self, event: &SfuEvent) -> bool {
        match event {
            SfuEvent::MemberJoined { user_id, role } => self.join(user_id, role),
            SfuEvent::MemberLeft { user_id, .. } => self.leave(user_id),
            _ => false,
        }
    }

    /// The members, in join order.
    pub fn members(&self) -> &[StreamMember] {
        &self.members
    }

    /// The user id of the host, when the host is in the session.
    pub fn host(&self) -> Option<&str> {
        self.members
            .iter()
            .find(|m| m.role == HOST_ROLE)
            .map(|m| m.user_id.as_str())
    }

    /// Add `user_id`, or update its role. A user is in the list once: the
    /// SFU sends `member_joined` again when a member reconnects and evicts
    /// its old connection. A session has one host: a `member_joined` with
    /// the host role replaces the host entry, it does not add a second one.
    fn join(&mut self, user_id: &str, role: &str) -> bool {
        if user_id.is_empty() {
            return false;
        }
        let before = self.members.clone();
        if role == HOST_ROLE {
            self.members
                .retain(|m| m.role != HOST_ROLE || m.user_id == user_id);
        }
        match self.members.iter_mut().find(|m| m.user_id == user_id) {
            Some(m) => m.role = role.to_string(),
            None => self.members.push(StreamMember {
                user_id: user_id.to_string(),
                role: role.to_string(),
            }),
        }
        self.members != before
    }

    fn leave(&mut self, user_id: &str) -> bool {
        let before = self.members.len();
        self.members.retain(|m| m.user_id != user_id);
        self.members.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(user_id: &str, role: &str) -> SfuEvent {
        SfuEvent::MemberJoined {
            user_id: user_id.into(),
            role: role.into(),
        }
    }

    fn left(user_id: &str) -> SfuEvent {
        SfuEvent::MemberLeft {
            user_id: user_id.into(),
            reason: "disconnect".into(),
        }
    }

    fn pairs(list: &StreamMembers) -> Vec<(&str, &str)> {
        list.members()
            .iter()
            .map(|m| (m.user_id.as_str(), m.role.as_str()))
            .collect()
    }

    #[test]
    fn the_snapshot_is_the_first_list() {
        let list = StreamMembers::from_joined(&[
            MemberInfo {
                user_id: "host-1".into(),
                role: "host".into(),
            },
            MemberInfo {
                user_id: "viewer-0".into(),
                role: "viewer".into(),
            },
        ]);
        assert_eq!(pairs(&list), [("host-1", "host"), ("viewer-0", "viewer")]);
        assert_eq!(list.host(), Some("host-1"));
    }

    #[test]
    fn a_reconnect_does_not_add_the_member_twice() {
        let mut list = StreamMembers::default();
        assert!(list.apply(&joined("viewer-2", "viewer")));
        assert!(
            !list.apply(&joined("viewer-2", "viewer")),
            "the same member again is no change"
        );
        assert_eq!(pairs(&list), [("viewer-2", "viewer")]);
    }

    #[test]
    fn a_late_host_is_the_one_host() {
        let mut list = StreamMembers::default();
        assert!(list.apply(&joined("host-1", "host")));
        assert!(!list.apply(&joined("host-1", "host")), "a host reconnect");
        assert!(list.apply(&joined("host-2", "host")));
        assert_eq!(pairs(&list), [("host-2", "host")]);
    }

    #[test]
    fn a_leave_of_an_unknown_member_is_no_change() {
        let mut list = StreamMembers::default();
        assert!(!list.apply(&left("nobody")));
        assert!(!list.apply(&joined("", "viewer")), "no user id");
        assert!(!list.apply(&SfuEvent::Disconnected { reason: "x".into() }));
        assert!(list.members().is_empty());
    }
}
