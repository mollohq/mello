#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum DeepLink {
    Invite { code: String },
    Crew { id: String },
}

pub fn parse(url: &str) -> Option<DeepLink> {
    let url = url.strip_prefix("mello://")?;
    let mut parts = url.splitn(2, '/');
    match parts.next()? {
        "invite" => Some(DeepLink::Invite {
            code: parts.next()?.to_string(),
        }),
        "crew" => Some(DeepLink::Crew {
            id: parts.next()?.to_string(),
        }),
        _ => None,
    }
}

pub fn extract_deep_link() -> Option<String> {
    std::env::args()
        .nth(1)
        .filter(|arg| arg.starts_with("mello://"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_invite_link() {
        let link = parse("mello://invite/abc123").unwrap();
        match link {
            DeepLink::Invite { code } => assert_eq!(code, "abc123"),
            _ => panic!("expected Invite"),
        }
    }

    #[test]
    fn parse_crew_link() {
        let link = parse("mello://crew/xyz789").unwrap();
        match link {
            DeepLink::Crew { id } => assert_eq!(id, "xyz789"),
            _ => panic!("expected Crew"),
        }
    }

    #[test]
    fn parse_unknown_returns_none() {
        assert!(parse("mello://unknown/foo").is_none());
    }

    #[test]
    fn parse_non_mello_returns_none() {
        assert!(parse("https://example.com").is_none());
    }
}
