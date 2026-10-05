// The question this crate exists to answer: "is there actually a sketchybar on
// every screen right now?"
//
// It is deliberately separate from the CoreGraphics calls that gather the input,
// because every cheap way of asking that question is a lie. During the outage
// this was written for, sketchybar was running, answering `--query bar`, and
// reporting `drawing=on` / `hidden=off` -- while the bar was not on screen. The
// only signal that tracks what a human sees is the geometry the window server
// hands out, so that is what gets tested here.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn intersects(&self, other: &Rect) -> bool {
        self.x < other.x + other.w
            && other.x < self.x + self.w
            && self.y < other.y + other.h
            && other.y < self.y + self.h
    }
}

#[derive(Debug, Clone)]
pub struct Display {
    pub id: u32,
    pub bounds: Rect,
    pub builtin: bool,
}

#[derive(Debug, Clone)]
pub struct Window {
    pub owner: String,
    pub bounds: Rect,
    pub onscreen: bool,
}

// A bar covering less than this much of its display's width is not a bar anyone
// can read. Well clear of both ends in practice: a healthy bar measures 0.99-1.00
// (it insets by the config's `margin`), and the failure modes seen so far leave
// either nothing at all or 1x1 stubs, i.e. ~0.00.
pub const MIN_COVERAGE: f64 = 0.5;

// sketchybar parks the item windows it is not currently drawing far off-screen at
// (-9999, -9999) rather than destroying them. They are real windows the window
// server will happily report as on-screen, so they have to be excluded by
// geometry or every display looks healthy no matter what.
const PARKED_COORD: f64 = -9000.0;

fn is_bar_window(w: &Window) -> bool {
    w.owner.to_lowercase().contains("sketchybar")
        && w.onscreen
        && w.bounds.w > 1.0
        && w.bounds.h > 1.0
        && w.bounds.x > PARKED_COORD
        && w.bounds.y > PARKED_COORD
}

#[derive(Debug, Clone, PartialEq)]
pub struct DisplayVerdict {
    pub id: u32,
    pub builtin: bool,
    pub width: f64,
    pub height: f64,
    pub coverage: f64,
    pub has_bar: bool,
}

// Widest sketchybar window overlapping the display, as a fraction of the
// display's width. The widest one is the bar's own background window; item
// windows are narrow slices sitting on top of it.
fn coverage(display: &Display, windows: &[Window]) -> f64 {
    if display.bounds.w <= 0.0 {
        return 0.0;
    }
    windows
        .iter()
        .filter(|w| is_bar_window(w) && w.bounds.intersects(&display.bounds))
        .map(|w| w.bounds.w / display.bounds.w)
        .fold(0.0, f64::max)
}

pub fn verdict(displays: &[Display], windows: &[Window]) -> Vec<DisplayVerdict> {
    displays
        .iter()
        .map(|d| {
            let c = coverage(d, windows);
            DisplayVerdict {
                id: d.id,
                builtin: d.builtin,
                width: d.bounds.w,
                height: d.bounds.h,
                coverage: c,
                has_bar: c >= MIN_COVERAGE,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(id: u32, x: f64, y: f64, w: f64, h: f64) -> Display {
        Display { id, bounds: Rect { x, y, w, h }, builtin: id == 1 }
    }

    fn bar_on(d: &Display) -> Window {
        // A real bar insets by the config's `margin`, so it is a few px narrower
        // than the display -- it must still count as present.
        Window {
            owner: "sketchybar".into(),
            bounds: Rect { x: d.bounds.x + 4.0, y: d.bounds.y + 4.0, w: d.bounds.w - 8.0, h: 39.0 },
            onscreen: true,
        }
    }

    #[test]
    fn a_full_width_bar_on_every_display_is_healthy() {
        let ds = vec![display(1, 0.0, 0.0, 1512.0, 982.0), display(2, -73.0, -1050.0, 1680.0, 1050.0)];
        let ws = vec![bar_on(&ds[0]), bar_on(&ds[1])];
        let v = verdict(&ds, &ws);
        assert!(v.iter().all(|d| d.has_bar), "{v:?}");
        assert!(v[0].coverage > 0.98);
    }

    // The failure the user actually reported: bar fine on one screen, gone on the
    // other. A whole-system "is sketchybar alive" check cannot see this.
    #[test]
    fn a_bar_missing_from_one_display_is_caught_even_though_the_other_is_fine() {
        let ds = vec![display(1, 0.0, 0.0, 1512.0, 982.0), display(2, -73.0, -1050.0, 1680.0, 1050.0)];
        let ws = vec![bar_on(&ds[1])];
        let v = verdict(&ds, &ws);
        assert!(!v[0].has_bar, "built-in has no bar and must be reported missing");
        assert!(v[1].has_bar, "external still has its bar");
    }

    // sketchybar keeps ~30 parked windows at (-9999,-9999). Counting those is how
    // a probe reports "all healthy" while the screen is empty.
    #[test]
    fn parked_offscreen_item_windows_never_count_as_a_bar() {
        let ds = vec![display(1, 0.0, 0.0, 1512.0, 982.0)];
        let ws: Vec<Window> = (0..30)
            .map(|_| Window {
                owner: "sketchybar".into(),
                bounds: Rect { x: -9999.0, y: -9999.0, w: 1512.0, h: 39.0 },
                onscreen: true,
            })
            .collect();
        assert!(!verdict(&ds, &ws)[0].has_bar);
    }

    #[test]
    fn collapsed_zero_size_windows_never_count_as_a_bar() {
        let ds = vec![display(1, 0.0, 0.0, 1512.0, 982.0)];
        let ws = vec![Window { owner: "sketchybar".into(), bounds: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }, onscreen: true }];
        assert!(!verdict(&ds, &ws)[0].has_bar);
    }

    #[test]
    fn another_apps_full_width_window_is_not_mistaken_for_the_bar() {
        let ds = vec![display(1, 0.0, 0.0, 1512.0, 982.0)];
        let ws = vec![Window { owner: "Arc".into(), bounds: Rect { x: 0.0, y: 0.0, w: 1512.0, h: 39.0 }, onscreen: true }];
        assert!(!verdict(&ds, &ws)[0].has_bar);
    }

    #[test]
    fn an_offscreen_bar_window_does_not_count() {
        let ds = vec![display(1, 0.0, 0.0, 1512.0, 982.0)];
        let mut w = bar_on(&ds[0]);
        w.onscreen = false;
        assert!(!verdict(&ds, &[w])[0].has_bar);
    }

    // A bar belonging to the *other* display must not rescue this one: the two
    // displays are disjoint in the global coordinate space.
    #[test]
    fn a_bar_on_a_different_display_does_not_satisfy_this_one() {
        let ds = vec![display(1, 0.0, 0.0, 1512.0, 982.0), display(2, -73.0, -1050.0, 1680.0, 1050.0)];
        let v = verdict(&ds, &[bar_on(&ds[1])]);
        assert_eq!(v[0].coverage, 0.0, "the external display's bar must not count toward the built-in");
    }

    #[test]
    fn a_narrow_leftover_sliver_is_not_a_bar() {
        let ds = vec![display(1, 0.0, 0.0, 1512.0, 982.0)];
        let ws = vec![Window { owner: "sketchybar".into(), bounds: Rect { x: 0.0, y: 4.0, w: 200.0, h: 39.0 }, onscreen: true }];
        let v = verdict(&ds, &ws);
        assert!(!v[0].has_bar, "200px of 1512 is a leftover item, not a bar");
    }
}
