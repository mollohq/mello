#[derive(Debug, Clone)]
pub enum DeepLink {
    Join { code: String },
    Crew { id: String },
}

pub fn parse(url: &str) -> Option<DeepLink> {
    let url = url.strip_prefix("mello://")?;
    let mut parts = url.splitn(2, '/');
    match parts.next()? {
        "join" => Some(DeepLink::Join {
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
    fn parse_join_link() {
        let link = parse("mello://join/ABCD-1234").unwrap();
        match link {
            DeepLink::Join { code } => assert_eq!(code, "ABCD-1234"),
            _ => panic!("expected Join"),
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
