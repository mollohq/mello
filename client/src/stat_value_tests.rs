//! StatValue puts a unit on the baseline of the number beside it.
//!
//! Two `Text` elements in different fonts and sizes are box-aligned, not
//! baseline-aligned: the bigger face has a deeper box under its baseline.
//! `StatValue` (ui/components/stat.slint) lifts the unit by the difference.
//! This test reads the laid-out geometry of the probe window and derives each
//! baseline from the font metrics, independently of the component's own
//! `lift` property.

use i_slint_backend_testing::ElementHandle;
use slint::ComponentHandle;

mod probe {
    // Debug builds only: build.rs compiles the probe when debug info is on,
    // and `cargo test` is a debug build.
    #![allow(clippy::all, dead_code, unused_imports)]
    include!(concat!(env!("OUT_DIR"), "/stat_probe.rs"));
}

/// hhea metrics, in font units, of the faces in client/ui/fonts (unitsPerEm 1000).
/// Slint's Text box is ascender + descender + lineGap tall, with half the gap
/// below the descender: baseline sits `(descender + lineGap / 2) / 1000` of the
/// font size above the bottom of the box.
const OXANIUM_BELOW_BASELINE: f32 = (210.0 + 250.0 / 2.0) / 1000.0;
const BARLOW_BELOW_BASELINE: f32 = 200.0 / 1000.0;

/// Baseline y of a Text element, from its box and font size.
fn baseline(e: &ElementHandle, size: f32, below_baseline: f32) -> f32 {
    e.absolute_position().y + e.size().height - size * below_baseline
}

#[test]
fn unit_sits_on_the_number_baseline_at_two_size_pairs() {
    crate::testkit::init_test_backend();
    let win = probe::StatProbe::new().expect("probe window");
    win.show().expect("show probe");

    let mut values: Vec<_> =
        ElementHandle::find_by_element_id(&win, "StatValue::value-text").collect();
    let mut units: Vec<_> =
        ElementHandle::find_by_element_id(&win, "StatValue::unit-text").collect();
    assert_eq!(values.len(), 2, "two StatValue values in the probe");
    assert_eq!(units.len(), 2, "two StatValue units in the probe");
    // Top to bottom: the 120/34 pair, then the 20/11 pair.
    values.sort_by(|a, b| a.absolute_position().y.total_cmp(&b.absolute_position().y));
    units.sort_by(|a, b| a.absolute_position().y.total_cmp(&b.absolute_position().y));

    for (i, (value_size, unit_size)) in [(120.0_f32, 34.0_f32), (20.0, 11.0)]
        .into_iter()
        .enumerate()
    {
        let v = baseline(&values[i], value_size, OXANIUM_BELOW_BASELINE);
        let u = baseline(&units[i], unit_size, BARLOW_BELOW_BASELINE);
        assert!(
            (v - u).abs() <= 1.0,
            "pair {value_size}/{unit_size}: number baseline {v}, unit baseline {u}"
        );
        // The unit is beside the number, not under it.
        assert!(
            units[i].absolute_position().x
                >= values[i].absolute_position().x + values[i].size().width
        );
    }
}
