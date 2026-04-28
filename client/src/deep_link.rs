#[derive(Debug, Clone)]
pub enum DeepLink {
    Join { code: String },
    Crew { id: String },
}

pub fn parse(url: &str) -> Option<DeepLink> {
    let lower = url.to_ascii_lowercase();
    let path = lower.strip_prefix("mello://")?;
    // Strip query string and fragment
    let path = path.split('?').next()?;
    let path = path.split('#').next()?;
    let path = path.trim_end_matches('/');

    let mut parts = path.splitn(2, '/');
    let action = parts.next()?;
    let value = parts.next().filter(|v| !v.is_empty())?;

    // Preserve original casing for the value by extracting from the original URL
    let original_value = extract_value(url, value.len())?;

    match action {
        "join" => Some(DeepLink::Join {
            code: original_value,
        }),
        "crew" => Some(DeepLink::Crew { id: original_value }),
        _ => None,
    }
}

/// Extract the value portion from the original URL, preserving its casing.
fn extract_value(url: &str, len: usize) -> Option<String> {
    let after_scheme = url.find("://")?;
    let path = &url[after_scheme + 3..];
    let path = path.split('?').next()?;
    let path = path.split('#').next()?;
    let path = path.trim_end_matches('/');
    let slash = path.find('/')?;
    let val = &path[slash + 1..];
    if val.len() >= len {
        Some(val[..len].to_string())
    } else {
        Some(val.to_string())
    }
}

pub fn extract_deep_link() -> Option<String> {
    std::env::args()
        .nth(1)
        .filter(|arg| arg.to_ascii_lowercase().starts_with("mello://"))
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

    #[test]
    fn parse_trailing_slash() {
        let link = parse("mello://join/ABCD-1234/").unwrap();
        match link {
            DeepLink::Join { code } => assert_eq!(code, "ABCD-1234"),
            _ => panic!("expected Join"),
        }
    }

    #[test]
    fn parse_uppercase_scheme() {
        let link = parse("MELLO://join/ABCD-1234").unwrap();
        match link {
            DeepLink::Join { code } => assert_eq!(code, "ABCD-1234"),
            _ => panic!("expected Join"),
        }
    }

    #[test]
    fn parse_query_string() {
        let link = parse("mello://join/ABCD-1234?ref=twitter").unwrap();
        match link {
            DeepLink::Join { code } => assert_eq!(code, "ABCD-1234"),
            _ => panic!("expected Join"),
        }
    }

    #[test]
    fn parse_fragment() {
        let link = parse("mello://join/ABCD-1234#section").unwrap();
        match link {
            DeepLink::Join { code } => assert_eq!(code, "ABCD-1234"),
            _ => panic!("expected Join"),
        }
    }

    #[test]
    fn parse_preserves_code_casing() {
        let link = parse("mello://join/AbCd-1234").unwrap();
        match link {
            DeepLink::Join { code } => assert_eq!(code, "AbCd-1234"),
            _ => panic!("expected Join"),
        }
    }

    #[test]
    fn parse_empty_value_returns_none() {
        assert!(parse("mello://join/").is_none());
        assert!(parse("mello://join").is_none());
    }
}
