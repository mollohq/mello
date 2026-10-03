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

/// The invite code in what a user typed or pasted into an invite field.
///
/// Accepts three forms, and returns the code as `XXXX-XXXX` in upper case:
/// - a web link: `https://m3llo.app/join/CODE`, with or without the scheme,
///   with or without `www.`, and with a trailing slash, query or fragment;
/// - a deep link: `mello://join/CODE`;
/// - a bare code, in any case, with or without the dash.
///
/// Surrounding spaces do not matter. Anything else returns `None`, so the
/// caller can refuse it without a network call.
pub fn parse_invite_input(input: &str) -> Option<String> {
    let input = input.trim();
    let lower = input.to_ascii_lowercase();

    let raw = if lower.starts_with("mello://") {
        match parse(input)? {
            DeepLink::Join { code } => code,
            DeepLink::Crew { .. } => return None,
        }
    } else if let Some(rest) = web_join_path(input, &lower) {
        // Same parser as the deep link: the web path is `join/CODE`.
        match parse(&format!("mello://{rest}"))? {
            DeepLink::Join { code } => code,
            DeepLink::Crew { .. } => return None,
        }
    } else {
        input.to_string()
    };
    normalize_invite_code(&raw)
}

/// The part of `https://m3llo.app/join/CODE` after the host: `join/CODE`.
/// `None` when the input is not a link to the m3llo.app lounge.
fn web_join_path<'a>(input: &'a str, lower: &str) -> Option<&'a str> {
    let mut skip = 0;
    for scheme in ["https://", "http://"] {
        if lower.starts_with(scheme) {
            skip = scheme.len();
            break;
        }
    }
    if lower[skip..].starts_with("www.") {
        skip += "www.".len();
    }
    lower[skip..].strip_prefix("m3llo.app/")?;
    Some(&input[skip + "m3llo.app/".len()..])
}

/// A code as `XXXX-XXXX`: eight letters or digits, with the dash optional.
fn normalize_invite_code(raw: &str) -> Option<String> {
    let compact = raw.replacen('-', "", 1);
    if compact.len() != 8 || !compact.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    // A dash anywhere but the middle is not a code that the server issued.
    if raw.contains('-') && raw.find('-') != Some(4) {
        return None;
    }
    let upper = compact.to_ascii_uppercase();
    Some(format!("{}-{}", &upper[..4], &upper[4..]))
}

/// The longest text that [`lounge_link_code`] reads. A join link is about 35
/// bytes. A longer text on the clipboard is something else.
const MAX_LOUNGE_LINK_LEN: usize = 256;

/// The invite code in a lounge join link on the clipboard (CREW-INVITES §7).
///
/// Stricter than [`parse_invite_input`]: the user did not type this text.
/// Only `https://{lounge_host}/join/{code}` counts. `http`, `www.`, a
/// trailing slash, a query and a fragment are allowed. A bare code, a deep
/// link and any other text return `None`. An empty `lounge_host` accepts
/// any host.
pub fn lounge_link_code(text: &str, lounge_host: &str) -> Option<String> {
    if text.len() > MAX_LOUNGE_LINK_LEN {
        return None;
    }
    let lower = text.trim().to_ascii_lowercase();
    let rest = ["https://", "http://"]
        .iter()
        .find_map(|scheme| lower.strip_prefix(scheme))?;
    let (host, path) = rest.split_once('/')?;
    if !lounge_host.is_empty() {
        let want = lounge_host.to_ascii_lowercase();
        if host != want && host.strip_prefix("www.") != Some(want.as_str()) {
            return None;
        }
    }
    if !path.starts_with("join/") {
        return None;
    }
    match parse(&format!("mello://{path}"))? {
        DeepLink::Join { code } => normalize_invite_code(&code),
        DeepLink::Crew { .. } => None,
    }
}

/// The deep link in the command line of this process, if any.
pub fn extract_deep_link() -> Option<String> {
    deep_link_in(std::env::args().skip(1))
}

/// The deep link in `args` (argv without the program name).
///
/// The OS passes a `mello://` URL as the first argument. A flag such as
/// `--build-info` or `--reset` in that place is not a deep link.
pub fn deep_link_in<I, S>(args: I) -> Option<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .next()
        .map(|a| a.as_ref().to_string())
        .filter(|arg| arg.to_ascii_lowercase().starts_with("mello://"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_link_in_takes_only_a_mello_url_as_first_argument() {
        assert_eq!(
            deep_link_in(["mello://join/ABCD-1234"]).as_deref(),
            Some("mello://join/ABCD-1234")
        );
        assert_eq!(
            deep_link_in(["MELLO://crew/x", "--reset"]).as_deref(),
            Some("MELLO://crew/x")
        );
        assert_eq!(deep_link_in(["--reset", "mello://join/ABCD-1234"]), None);
        assert_eq!(deep_link_in(Vec::<&str>::new()), None);
    }

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

    fn invite(input: &str) -> Option<String> {
        parse_invite_input(input)
    }

    #[test]
    fn invite_input_accepts_a_web_link_in_every_form() {
        for input in [
            "https://m3llo.app/join/ABCD-1234",
            "http://m3llo.app/join/ABCD-1234",
            "m3llo.app/join/ABCD-1234",
            "www.m3llo.app/join/ABCD-1234",
            "https://www.m3llo.app/join/ABCD-1234",
            "HTTPS://M3LLO.APP/JOIN/ABCD-1234",
            "https://m3llo.app/join/ABCD-1234/",
            "https://m3llo.app/join/ABCD-1234?ref=twitter",
            "https://m3llo.app/join/ABCD-1234#top",
            "https://m3llo.app/join/ABCD-1234/?ref=x#top",
            "  https://m3llo.app/join/ABCD-1234  ",
            "https://m3llo.app/join/abcd1234",
        ] {
            assert_eq!(invite(input).as_deref(), Some("ABCD-1234"), "{input:?}");
        }
    }

    #[test]
    fn invite_input_accepts_a_deep_link() {
        assert_eq!(
            invite("mello://join/ABCD-1234").as_deref(),
            Some("ABCD-1234")
        );
        assert_eq!(
            invite("  mello://join/abcd-1234/?x=1  ").as_deref(),
            Some("ABCD-1234")
        );
    }

    #[test]
    fn invite_input_accepts_a_bare_code_in_any_case_with_or_without_the_dash() {
        for input in [
            "ABCD-1234",
            "abcd-1234",
            "AbCd-1234",
            "ABCD1234",
            "abcd1234",
            "  ABCD-1234\n",
            "\tabcd1234 ",
        ] {
            assert_eq!(invite(input).as_deref(), Some("ABCD-1234"), "{input:?}");
        }
    }

    #[test]
    fn invite_input_refuses_what_is_not_an_invite() {
        for input in [
            "",
            "   ",
            "ABCD",
            "ABCD-123",
            "ABCD-12345",
            "AB-CD1234",
            "ABCD--1234",
            "ABCD 1234",
            "ABCD-12!4",
            "ÅBCD-1234",
            "https://m3llo.app/join/",
            "https://m3llo.app/join",
            "https://m3llo.app/crew/ABCD-1234",
            "https://m3llo.app/",
            "https://example.com/join/ABCD-1234",
            "https://notm3llo.app/join/ABCD-1234",
            "mello://crew/ABCD-1234",
            "mello://join/",
            "mello://join/not a code",
            "hello world",
        ] {
            assert_eq!(invite(input), None, "{input:?}");
        }
    }

    #[test]
    fn lounge_link_code_takes_a_join_link_on_the_lounge_host() {
        for text in [
            "https://m3llo.app/join/ABCD-1234",
            "http://m3llo.app/join/ABCD-1234",
            "https://www.m3llo.app/join/ABCD-1234",
            "HTTPS://M3LLO.APP/JOIN/abcd-1234",
            "https://m3llo.app/join/ABCD-1234/",
            "https://m3llo.app/join/ABCD-1234?ref=lounge#top",
            "  https://m3llo.app/join/abcd1234\n",
        ] {
            assert_eq!(
                lounge_link_code(text, "m3llo.app").as_deref(),
                Some("ABCD-1234"),
                "{text:?}"
            );
        }
    }

    #[test]
    fn lounge_link_code_ignores_everything_else() {
        let long = format!("https://m3llo.app/join/ABCD-1234?{}", "x".repeat(300));
        for text in [
            "",
            "ABCD-1234",
            "mello://join/ABCD-1234",
            "m3llo.app/join/ABCD-1234",
            "https://example.com/join/ABCD-1234",
            "https://notm3llo.app/join/ABCD-1234",
            "https://m3llo.app.example.com/join/ABCD-1234",
            "https://user@m3llo.app/join/ABCD-1234",
            "https://m3llo.app:8443/join/ABCD-1234",
            "https://m3llo.app/crew/ABCD-1234",
            "https://m3llo.app/join/",
            "https://m3llo.app/join/ABCD-1234/extra",
            "https://m3llo.app/join/ABCD-1234 and some text",
            "look at https://m3llo.app/join/ABCD-1234",
            "hunter2",
            long.as_str(),
        ] {
            assert_eq!(lounge_link_code(text, "m3llo.app"), None, "{text:?}");
        }
    }

    #[test]
    fn lounge_link_code_with_no_host_accepts_any_host() {
        assert_eq!(
            lounge_link_code("http://localhost:8788/join/DEVS-0001", "").as_deref(),
            Some("DEVS-0001")
        );
        assert_eq!(
            lounge_link_code("https://m3llo.app/join/ABCD-1234", "").as_deref(),
            Some("ABCD-1234")
        );
        // Still a join link and nothing else.
        assert_eq!(lounge_link_code("ABCD-1234", ""), None);
        assert_eq!(lounge_link_code("mello://join/ABCD-1234", ""), None);
    }

    #[test]
    fn parse_empty_value_returns_none() {
        assert!(parse("mello://join/").is_none());
        assert!(parse("mello://join").is_none());
    }
}
