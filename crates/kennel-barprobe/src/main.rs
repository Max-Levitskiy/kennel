// kennel-barprobe -- answers "does every screen actually have a sketchybar on it?"
//
// Printed for kennel's sketchybar-watchdog to parse. First line is the machine
// readable summary; the rest is detail for the log, so that when the bar goes
// missing again there is a record of what the window server thought at the time.
//
//   displays=2 with_bar=1
//   display id=1 builtin=1 1512x982 coverage=99% bar=present
//   display id=2 builtin=0 1680x1050 coverage=0% bar=MISSING
//
// Exit codes are the important contract: 0 means "the probe ran and the summary
// is trustworthy" (whether or not a bar is missing), and 2 means "the probe could
// not read the window server". A watchdog must never restart anything on a 2 --
// acting on no information is how you get a restart loop.

mod verdict;

use verdict::{verdict, Display, Rect, Window};

#[cfg(target_os = "macos")]
mod platform {
    use super::{Display, Rect, Window};
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_graphics::display::CGDisplay;
    use core_graphics::window::{copy_window_info, kCGWindowListOptionAll};

    pub fn displays() -> Result<Vec<Display>, String> {
        let ids = CGDisplay::active_displays().map_err(|e| format!("CGGetActiveDisplayList failed: {e:?}"))?;
        Ok(ids
            .into_iter()
            .map(|id| {
                let d = CGDisplay::new(id);
                let b = d.bounds();
                Display {
                    id,
                    bounds: Rect { x: b.origin.x, y: b.origin.y, w: b.size.width, h: b.size.height },
                    builtin: d.is_builtin(),
                }
            })
            .collect())
    }

    fn get(d: &CFDictionary<CFString, CFType>, key: &str) -> Option<CFType> {
        d.find(CFString::new(key)).map(|v| v.clone())
    }

    pub fn windows() -> Result<Vec<Window>, String> {
        // kCGWindowListOptionAll with NO ExcludeDesktopElements. sketchybar draws
        // at window layer -20 -- below normal windows, which is exactly what
        // "desktop element" means to CoreGraphics -- so excluding those hides most
        // of the bar from the probe and reports a healthy machine as barless.
        let opts = kCGWindowListOptionAll;
        let raw = copy_window_info(opts, 0).ok_or("CGWindowListCopyWindowInfo returned nothing")?;
        let arr: CFArray<CFDictionary<CFString, CFType>> = unsafe { CFArray::wrap_under_get_rule(raw.as_concrete_TypeRef()) };

        let mut out = Vec::new();
        for entry in arr.iter() {
            let owner = match get(&entry, "kCGWindowOwnerName").and_then(|v| v.downcast::<CFString>()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            // kCGWindowIsOnscreen is a CFBoolean. Downcasting it to CFNumber
            // silently yields None for every window, which reads as "nothing is
            // on screen" -- a probe that reports the bar missing on a perfectly
            // healthy machine, i.e. an endless restart loop. The CFNumber arm is
            // kept as a fallback rather than an assumption about the encoding.
            let onscreen = get(&entry, "kCGWindowIsOnscreen")
                .map(|v| {
                    if let Some(b) = v.downcast::<CFBoolean>() {
                        bool::from(b)
                    } else if let Some(n) = v.downcast::<CFNumber>().and_then(|n| n.to_i64()) {
                        n != 0
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            let bounds = match get(&entry, "kCGWindowBounds").and_then(|v| v.downcast::<CFDictionary>()) {
                Some(d) => match core_graphics::geometry::CGRect::from_dict_representation(&d) {
                    Some(r) => Rect { x: r.origin.x, y: r.origin.y, w: r.size.width, h: r.size.height },
                    None => continue,
                },
                None => continue,
            };
            out.push(Window { owner, bounds, onscreen });
        }
        Ok(out)
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{Display, Window};
    pub fn displays() -> Result<Vec<Display>, String> { Err("macOS only".into()) }
    pub fn windows() -> Result<Vec<Window>, String> { Err("macOS only".into()) }
}

fn main() {
    let (displays, windows) = match (platform::displays(), platform::windows()) {
        (Ok(d), Ok(w)) => (d, w),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("kennel-barprobe: {e}");
            std::process::exit(2);
        }
    };

    // No displays at all (every screen asleep, or the lid shut on a laptop with
    // nothing else attached) is not a missing bar. Report it and let the caller
    // do nothing, rather than kickstarting sketchybar at a black screen.
    if displays.is_empty() {
        println!("displays=0 with_bar=0");
        return;
    }

    if std::env::args().any(|a| a == "--debug") {
        eprintln!("windows parsed: {}", windows.len());
        let sb: Vec<_> = windows.iter().filter(|w| w.owner.to_lowercase().contains("sketchybar")).collect();
        eprintln!("sketchybar windows: {}", sb.len());
        eprintln!("  onscreen: {}", sb.iter().filter(|w| w.onscreen).count());
        for w in sb.iter().filter(|w| w.bounds.w > 100.0).take(5) {
            eprintln!("  {:?}", w);
        }
        let owners: std::collections::BTreeSet<_> = windows.iter().map(|w| w.owner.as_str()).collect();
        eprintln!("distinct owners: {}", owners.len());
    }

    let v = verdict(&displays, &windows);
    let with_bar = v.iter().filter(|d| d.has_bar).count();
    println!("displays={} with_bar={}", v.len(), with_bar);
    for d in &v {
        println!(
            "display id={} builtin={} {:.0}x{:.0} coverage={:.0}% bar={}",
            d.id,
            if d.builtin { 1 } else { 0 },
            d.width,
            d.height,
            d.coverage * 100.0,
            if d.has_bar { "present" } else { "MISSING" }
        );
    }
}
