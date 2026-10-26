//! The About page: the Third-party notices asset.
//!
//! `build.rs` writes the asset (see `build_notices.rs` for the format). It is
//! embedded as one string and parsed on the first open of the notices view.

use crate::NoticeEntryData;

static NOTICES: &str = include_str!(concat!(env!("OUT_DIR"), "/third_party_notices.txt"));

const RECORD: char = '\u{1e}';
const FIELD: char = '\u{1f}';

/// One parsed record.
#[derive(Debug, PartialEq, Eq)]
pub struct Notice<'a> {
    pub names: &'a str,
    pub license: &'a str,
    pub text: &'a str,
}

/// Parse an asset. A record with a missing field is skipped.
pub fn parse(asset: &str) -> Vec<Notice<'_>> {
    asset
        .split(RECORD)
        .filter_map(|record| {
            let mut fields = record.splitn(3, FIELD);
            Some(Notice {
                names: fields.next()?,
                license: fields.next()?,
                text: fields.next()?,
            })
        })
        .collect()
}

/// The embedded notices, for the Slint model.
pub fn entries() -> Vec<NoticeEntryData> {
    parse(NOTICES)
        .into_iter()
        .map(|n| NoticeEntryData {
            names: n.names.into(),
            license: n.license.into(),
            text: n.text.into(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_has_names_license_and_text() {
        let asset = "a 1.0\nb 2.0\u{1f}MIT\u{1f}Copyright (c) A\u{1e}c 3.0\u{1f}OFL-1.1\u{1f}text";
        let parsed = parse(asset);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].names, "a 1.0\nb 2.0");
        assert_eq!(parsed[0].license, "MIT");
        assert_eq!(parsed[1].text, "text");
    }

    #[test]
    fn a_record_with_a_missing_field_is_skipped() {
        assert!(parse("only a name").is_empty());
    }

    #[test]
    fn the_asset_stays_inside_the_install_budget() {
        assert!(NOTICES.len() < 1_000_000, "{} bytes", NOTICES.len());
    }
}
