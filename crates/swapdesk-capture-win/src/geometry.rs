//! Pure display geometry: raw outputs in, C3 `DisplayInfo` out. No OS calls, builds on every host.
//!
//! Everything is in physical pixels of the virtual desktop (origin at the primary display's top-left, so a display to
//! the left of or above the primary has negative coordinates). The scale is the effective DPI over 96, in thousandths.

use std::collections::HashSet;
use swapdesk_platform::{AdapterId, DisplayId, DisplayInfo, Rect};

/// One output as the OS reports it, before ids are assigned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawOutput {
    /// The adapter LUID as `(high << 32) | low`.
    pub adapter_luid: u64,
    pub adapter_name: String,
    /// Position of the output on its adapter (`EnumOutputs` index).
    pub output_index: u32,
    /// GDI device name, e.g. `\\.\DISPLAY2`.
    pub device_name: String,
    /// Friendly monitor name, may be empty.
    pub monitor_name: String,
    /// Physical pixels in the virtual desktop.
    pub rect: Rect,
    /// Effective DPI of the monitor (96 = 100%).
    pub dpi: u32,
    pub refresh_hz: u32,
    /// 0, 90, 180 or 270.
    pub rotation_deg: u16,
    pub primary: bool,
}

/// Where a `DisplayId` lives: which adapter and which output on it. Needed to open the duplication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputKey {
    pub adapter_luid: u64,
    pub output_index: u32,
}

/// Effective DPI to the C3 `scale_milli` (1000 = 100%). A zero or absurd DPI reads as 100%.
pub fn scale_milli_from_dpi(dpi: u32) -> u16 {
    if dpi == 0 {
        return 1000;
    }
    let milli = (u64::from(dpi) * 1000 + 48) / 96;
    u16::try_from(milli.clamp(250, u64::from(u16::MAX))).unwrap_or(1000)
}

/// The N of `\\.\DISPLAYn` (also with a trailing `\Monitor0` part), if the name has that shape.
pub fn display_number(device_name: &str) -> Option<u16> {
    let upper = device_name.to_ascii_uppercase();
    let rest = upper.strip_prefix("\\\\.\\DISPLAY")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// `DXGI_MODE_ROTATION` (0 unspecified, 1 identity, 2 90, 3 180, 4 270) to degrees.
pub fn rotation_degrees(dxgi_rotation: i32) -> u16 {
    match dxgi_rotation {
        2 => 90,
        3 => 180,
        4 => 270,
        _ => 0,
    }
}

/// Builds the C3 display list. Ids come from the GDI display number (stable while the display stays attached, whatever
/// the adapter order); a name without a number gets an id above 1000 in input order. The same GDI device seen twice
/// (a clone listed by two adapters) is kept once, first wins. The primary display comes first, the rest by id.
pub fn normalize(raw: &[RawOutput]) -> Vec<(DisplayInfo, OutputKey)> {
    let mut seen_names: HashSet<String> = HashSet::new();
    let mut used_ids: HashSet<u16> = HashSet::new();
    let mut out: Vec<(DisplayInfo, OutputKey)> = Vec::new();
    let mut fallback = 1000u16;
    for r in raw {
        if !seen_names.insert(r.device_name.to_ascii_uppercase()) {
            continue;
        }
        let mut id = display_number(&r.device_name).unwrap_or(0);
        if id == 0 || used_ids.contains(&id) {
            while used_ids.contains(&fallback) {
                fallback = fallback.saturating_add(1);
            }
            id = fallback;
        }
        used_ids.insert(id);
        let name = if r.monitor_name.is_empty() { r.device_name.clone() } else { r.monitor_name.clone() };
        out.push((
            DisplayInfo {
                id: DisplayId(id),
                name,
                bounds: r.rect,
                scale_milli: scale_milli_from_dpi(r.dpi),
                refresh_mhz: r.refresh_hz.saturating_mul(1000),
                rotation: r.rotation_deg,
                primary: r.primary,
                adapter: AdapterId(r.adapter_luid),
            },
            OutputKey { adapter_luid: r.adapter_luid, output_index: r.output_index },
        ));
    }
    out.sort_by_key(|(d, _)| (!d.primary, d.id.0));
    out
}

pub fn right(r: &Rect) -> i64 {
    i64::from(r.x) + i64::from(r.width)
}

pub fn bottom(r: &Rect) -> i64 {
    i64::from(r.y) + i64::from(r.height)
}

/// Bounding box of every display, in virtual-desktop pixels (negative origin when a display sits left of or above the
/// primary). `None` for an empty list.
pub fn virtual_desktop(displays: &[DisplayInfo]) -> Option<Rect> {
    let first = displays.first()?;
    let (mut l, mut t) = (i64::from(first.bounds.x), i64::from(first.bounds.y));
    let (mut r, mut b) = (right(&first.bounds), bottom(&first.bounds));
    for d in displays {
        l = l.min(i64::from(d.bounds.x));
        t = t.min(i64::from(d.bounds.y));
        r = r.max(right(&d.bounds));
        b = b.max(bottom(&d.bounds));
    }
    Some(Rect {
        x: i32::try_from(l).ok()?,
        y: i32::try_from(t).ok()?,
        width: u32::try_from(r - l).ok()?,
        height: u32::try_from(b - t).ok()?,
    })
}

/// Whether the virtual-desktop point lies inside the rectangle.
pub fn contains(r: &Rect, x: i32, y: i32) -> bool {
    i64::from(x) >= i64::from(r.x)
        && i64::from(x) < right(r)
        && i64::from(y) >= i64::from(r.y)
        && i64::from(y) < bottom(r)
}

/// The display that holds a virtual-desktop point.
pub fn display_at(displays: &[DisplayInfo], x: i32, y: i32) -> Option<&DisplayInfo> {
    displays.iter().find(|d| contains(&d.bounds, x, y))
}

/// A virtual-desktop point as pixels inside the display (what `CursorUpdate` carries).
pub fn to_display_local(display: &DisplayInfo, x: i32, y: i32) -> (i32, i32) {
    (x.saturating_sub(display.bounds.x), y.saturating_sub(display.bounds.y))
}

/// The display's size in logical (DPI-scaled) pixels, what a DPI-aware window layout sees.
pub fn logical_size(display: &DisplayInfo) -> (u32, u32) {
    let s = u64::from(display.scale_milli.max(1));
    let f = |v: u32| u32::try_from((u64::from(v) * 1000 + s / 2) / s).unwrap_or(u32::MAX);
    (f(display.bounds.width), f(display.bounds.height))
}

/// Whether two rectangles share at least one pixel.
pub fn intersects(a: &Rect, b: &Rect) -> bool {
    i64::from(a.x) < right(b) && i64::from(b.x) < right(a) && i64::from(a.y) < bottom(b) && i64::from(b.y) < bottom(a)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayChange {
    Added(DisplayId),
    Removed(DisplayId),
    /// Size, position, scale, refresh rate, rotation or primary flag changed.
    ModeChanged(DisplayId),
    /// The display now hangs off another adapter: the duplication must be rebuilt on the new one.
    AdapterChanged(DisplayId),
}

/// What differs between two display lists, by id: removed first, then added, then changed, each in id order.
pub fn diff_displays(old: &[DisplayInfo], new: &[DisplayInfo]) -> Vec<DisplayChange> {
    let mut changes = Vec::new();
    let mut removed: Vec<u16> = old.iter().filter(|o| !new.iter().any(|n| n.id == o.id)).map(|o| o.id.0).collect();
    removed.sort_unstable();
    changes.extend(removed.into_iter().map(|id| DisplayChange::Removed(DisplayId(id))));
    let mut added: Vec<u16> = new.iter().filter(|n| !old.iter().any(|o| o.id == n.id)).map(|n| n.id.0).collect();
    added.sort_unstable();
    changes.extend(added.into_iter().map(|id| DisplayChange::Added(DisplayId(id))));
    let mut both: Vec<(&DisplayInfo, &DisplayInfo)> =
        old.iter().filter_map(|o| new.iter().find(|n| n.id == o.id).map(|n| (o, n))).collect();
    both.sort_by_key(|(o, _)| o.id.0);
    for (o, n) in both {
        if o.adapter != n.adapter {
            changes.push(DisplayChange::AdapterChanged(o.id));
        } else if o.bounds != n.bounds
            || o.scale_milli != n.scale_milli
            || o.refresh_mhz != n.refresh_mhz
            || o.rotation != n.rotation
            || o.primary != n.primary
        {
            changes.push(DisplayChange::ModeChanged(o.id));
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str, luid: u64, idx: u32, (x, y, w, h): (i32, i32, u32, u32), dpi: u32, primary: bool) -> RawOutput {
        RawOutput {
            adapter_luid: luid,
            adapter_name: "gpu".into(),
            output_index: idx,
            device_name: name.into(),
            monitor_name: String::new(),
            rect: Rect { x, y, width: w, height: h },
            dpi,
            refresh_hz: 60,
            rotation_deg: 0,
            primary,
        }
    }

    /// 200% laptop panel (primary), 100% monitor to its right, 125% monitor to its left and above the origin.
    fn mixed() -> Vec<RawOutput> {
        vec![
            raw("\\\\.\\DISPLAY3", 7, 0, (-1920, -300, 2400, 1350), 120, false),
            raw("\\\\.\\DISPLAY1", 7, 1, (0, 0, 2560, 1600), 192, true),
            raw("\\\\.\\DISPLAY2", 9, 0, (2560, 100, 1920, 1080), 96, false),
        ]
    }

    #[test]
    fn scale_from_dpi_rounds_to_thousandths() {
        assert_eq!(scale_milli_from_dpi(96), 1000);
        assert_eq!(scale_milli_from_dpi(120), 1250);
        assert_eq!(scale_milli_from_dpi(144), 1500);
        assert_eq!(scale_milli_from_dpi(168), 1750);
        assert_eq!(scale_milli_from_dpi(192), 2000);
        assert_eq!(scale_milli_from_dpi(288), 3000);
        assert_eq!(scale_milli_from_dpi(0), 1000);
        assert_eq!(scale_milli_from_dpi(u32::MAX), u16::MAX);
    }

    #[test]
    fn display_number_reads_gdi_names() {
        assert_eq!(display_number("\\\\.\\DISPLAY1"), Some(1));
        assert_eq!(display_number("\\\\.\\display12"), Some(12));
        assert_eq!(display_number("\\\\.\\DISPLAY2\\Monitor0"), Some(2));
        assert_eq!(display_number("\\\\.\\DISPLAY"), None);
        assert_eq!(display_number("HDMI-1"), None);
    }

    #[test]
    fn rotation_maps_dxgi_values() {
        assert_eq!([0, 1, 2, 3, 4, 9].map(rotation_degrees), [0, 0, 90, 180, 270, 0]);
    }

    #[test]
    fn normalize_puts_the_primary_first_and_ids_follow_gdi_numbers() {
        let list = normalize(&mixed());
        let ids: Vec<u16> = list.iter().map(|(d, _)| d.id.0).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert!(list[0].0.primary && !list[1].0.primary && !list[2].0.primary);
        assert_eq!(list[0].0.scale_milli, 2000);
        assert_eq!(list[1].0.scale_milli, 1000);
        assert_eq!(list[2].0.scale_milli, 1250);
        assert_eq!(list[0].0.adapter, AdapterId(7));
        assert_eq!(list[1].0.adapter, AdapterId(9));
        assert_eq!(list[2].1, OutputKey { adapter_luid: 7, output_index: 0 });
        assert_eq!(list[1].0.refresh_mhz, 60_000);
    }

    #[test]
    fn primary_comes_first_even_when_its_number_is_not_the_lowest() {
        let mut r = mixed();
        r[1].device_name = "\\\\.\\DISPLAY5".into();
        let ids: Vec<u16> = normalize(&r).iter().map(|(d, _)| d.id.0).collect();
        assert_eq!(ids, vec![5, 2, 3]);
    }

    #[test]
    fn negative_coordinates_survive_and_the_virtual_desktop_spans_them() {
        let list: Vec<DisplayInfo> = normalize(&mixed()).into_iter().map(|(d, _)| d).collect();
        let v = virtual_desktop(&list).expect("three displays");
        // Left edge -1920, right edge 2560 + 1920 = 4480; top -300, bottom 1600 (the primary's).
        assert_eq!(v, Rect { x: -1920, y: -300, width: 6400, height: 1900 });
    }

    #[test]
    fn point_lookup_and_display_local_coordinates_with_a_negative_origin() {
        let list: Vec<DisplayInfo> = normalize(&mixed()).into_iter().map(|(d, _)| d).collect();
        let left = display_at(&list, -1000, -200).expect("left display");
        assert_eq!(left.id, DisplayId(3));
        assert_eq!(to_display_local(left, -1000, -200), (920, 100));
        let primary = display_at(&list, 0, 0).expect("primary");
        assert_eq!(primary.id, DisplayId(1));
        assert_eq!(to_display_local(primary, 5, 7), (5, 7));
        let right = display_at(&list, 2560, 100).expect("right display");
        assert_eq!(right.id, DisplayId(2));
        assert_eq!(to_display_local(right, 2600, 150), (40, 50));
        // The gap above the right display and the far outside belong to nobody.
        assert!(display_at(&list, 2600, 50).is_none());
        assert!(display_at(&list, -5000, 0).is_none());
        // The right edge is exclusive.
        assert!(display_at(&list, 2559, 0).is_some_and(|d| d.id == DisplayId(1)));
    }

    #[test]
    fn logical_size_divides_by_the_scale() {
        let list: Vec<DisplayInfo> = normalize(&mixed()).into_iter().map(|(d, _)| d).collect();
        assert_eq!(logical_size(&list[0]), (1280, 800));
        assert_eq!(logical_size(&list[1]), (1920, 1080));
        assert_eq!(logical_size(&list[2]), (1920, 1080));
    }

    #[test]
    fn a_clone_seen_by_two_adapters_is_kept_once() {
        let mut r = mixed();
        r.push(raw("\\\\.\\DISPLAY1", 9, 3, (0, 0, 2560, 1600), 192, true));
        let list = normalize(&r);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].1.adapter_luid, 7);
    }

    #[test]
    fn names_without_a_number_get_ids_above_one_thousand() {
        let r = vec![raw("odd-a", 1, 0, (0, 0, 800, 600), 96, true), raw("odd-b", 1, 1, (800, 0, 800, 600), 96, false)];
        let ids: Vec<u16> = normalize(&r).iter().map(|(d, _)| d.id.0).collect();
        assert_eq!(ids, vec![1000, 1001]);
    }

    #[test]
    fn friendly_name_wins_over_the_device_name() {
        let mut r = mixed();
        r[1].monitor_name = "Panel".into();
        let list = normalize(&r);
        assert_eq!(list[0].0.name, "Panel");
        assert_eq!(list[1].0.name, "\\\\.\\DISPLAY2");
    }

    #[test]
    fn intersection_is_pixel_exact() {
        let a = Rect { x: 0, y: 0, width: 10, height: 10 };
        assert!(intersects(&a, &Rect { x: 9, y: 9, width: 5, height: 5 }));
        assert!(!intersects(&a, &Rect { x: 10, y: 0, width: 5, height: 5 }));
        assert!(intersects(&a, &Rect { x: -5, y: -5, width: 6, height: 6 }));
        assert!(!intersects(&a, &Rect { x: -5, y: -5, width: 5, height: 5 }));
    }

    #[test]
    fn diff_reports_removed_added_and_changed_in_that_order() {
        let old: Vec<DisplayInfo> = normalize(&mixed()).into_iter().map(|(d, _)| d).collect();
        let mut new = old.clone();
        // Display 3 unplugged, display 4 plugged, display 2 changes mode, display 1 moves adapters.
        new.retain(|d| d.id != DisplayId(3));
        new.push(DisplayInfo { id: DisplayId(4), ..old[2].clone() });
        new[1].bounds.width = 1280;
        new[0].adapter = AdapterId(9);
        assert_eq!(
            diff_displays(&old, &new),
            vec![
                DisplayChange::Removed(DisplayId(3)),
                DisplayChange::Added(DisplayId(4)),
                DisplayChange::AdapterChanged(DisplayId(1)),
                DisplayChange::ModeChanged(DisplayId(2)),
            ]
        );
        assert!(diff_displays(&old, &old).is_empty());
    }

    #[test]
    fn diff_sees_scale_refresh_and_rotation() {
        let old: Vec<DisplayInfo> = normalize(&mixed()).into_iter().map(|(d, _)| d).collect();
        for tweak in [
            (|d: &mut DisplayInfo| d.scale_milli = 1500) as fn(&mut DisplayInfo),
            |d| d.refresh_mhz = 144_000,
            |d| d.rotation = 90,
            |d| d.primary = !d.primary,
        ] {
            let mut new = old.clone();
            tweak(&mut new[1]);
            assert_eq!(diff_displays(&old, &new), vec![DisplayChange::ModeChanged(old[1].id)]);
        }
    }

    #[test]
    fn empty_list_has_no_virtual_desktop() {
        assert!(virtual_desktop(&[]).is_none());
    }
}
