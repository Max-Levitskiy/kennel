// What the `window_server` capability hands an extension: every active display,
// the kind of space it is showing, and the windows of one owner. Facts only --
// deciding what they mean (is the bar there? should it be?) is the extension's job.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Display {
    pub id: u32,
    pub uuid: String,
    pub bounds: Rect,
    pub builtin: bool,
    pub space: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Window {
    pub owner: String,
    pub bounds: Rect,
    pub onscreen: bool,
    pub layer: i64,
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub displays: Vec<Display>,
    pub windows: Vec<Window>,
}

// CGSSpaceType values; anything else is reported as "other".
fn space_kind(space_type: Option<i64>) -> &'static str {
    match space_type {
        Some(0) => "desktop",
        Some(4) => "fullscreen",
        Some(_) => "other",
        None => "unknown",
    }
}

// The full list is ~1000 windows, far over the guest's 64 KiB buffer, so callers
// must name the owner they care about.
pub fn snapshot(owner: &str) -> Result<Snapshot, String> {
    if owner.is_empty() {
        return Err("owner must not be empty".into());
    }
    let displays = platform::displays()?;
    let windows = platform::windows()?.into_iter().filter(|w| w.owner.eq_ignore_ascii_case(owner)).collect();
    Ok(Snapshot { displays, windows })
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{space_kind, Display, Rect, Window};
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_graphics::display::CGDisplay;
    use core_graphics::window::{copy_window_info, kCGWindowListOptionAll};
    use std::collections::HashMap;
    use std::ffi::c_void;

    // Private SkyLight API, the only way to read a display's current space. Looked
    // up at runtime: a macOS without it must cost the space field, not kenneld's launch.
    fn copy_managed_display_spaces() -> *const c_void {
        type MainConnection = unsafe extern "C" fn() -> i32;
        type CopySpaces = unsafe extern "C" fn(i32) -> *const c_void;
        unsafe {
            let main = libc::dlsym(libc::RTLD_DEFAULT, c"CGSMainConnectionID".as_ptr());
            let copy = libc::dlsym(libc::RTLD_DEFAULT, c"CGSCopyManagedDisplaySpaces".as_ptr());
            if main.is_null() || copy.is_null() {
                return std::ptr::null();
            }
            let main: MainConnection = std::mem::transmute(main);
            let copy: CopySpaces = std::mem::transmute(copy);
            copy(main())
        }
    }
    #[link(name = "ColorSync", kind = "framework")]
    extern "C" {
        fn CGDisplayCreateUUIDFromDisplayID(display: u32) -> *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFUUIDCreateString(alloc: *const c_void, uuid: *const c_void) -> *const c_void;
        fn CFRelease(cf: *const c_void);
    }

    fn get(d: &CFDictionary<CFString, CFType>, key: &str) -> Option<CFType> {
        d.find(CFString::new(key)).map(|v| v.clone())
    }

    fn as_dict(v: CFType) -> Option<CFDictionary<CFString, CFType>> {
        let d = v.downcast::<CFDictionary>()?;
        Some(unsafe { CFDictionary::wrap_under_get_rule(d.as_concrete_TypeRef()) })
    }

    fn display_uuid(id: u32) -> Option<String> {
        unsafe {
            let uuid = CGDisplayCreateUUIDFromDisplayID(id);
            if uuid.is_null() {
                return None;
            }
            let s = CFUUIDCreateString(std::ptr::null(), uuid);
            CFRelease(uuid);
            if s.is_null() {
                return None;
            }
            Some(CFString::wrap_under_create_rule(s as _).to_string())
        }
    }

    // Current space type per display UUID, upper-cased. "Main" replaces the UUID
    // when "Displays have separate Spaces" is off.
    fn current_space_types() -> HashMap<String, i64> {
        let raw = copy_managed_display_spaces();
        if raw.is_null() {
            return HashMap::new();
        }
        let arr: CFArray<CFDictionary<CFString, CFType>> = unsafe { CFArray::wrap_under_create_rule(raw as _) };
        let mut out = HashMap::new();
        for entry in arr.iter() {
            let Some(ident) = get(&entry, "Display Identifier").and_then(|v| v.downcast::<CFString>()) else { continue };
            let ident = ident.to_string();
            let ident = if ident == "Main" { display_uuid(CGDisplay::main().id).unwrap_or(ident) } else { ident };
            let space_type = get(&entry, "Current Space")
                .and_then(as_dict)
                .and_then(|space| get(&space, "type"))
                .and_then(|v| v.downcast::<CFNumber>())
                .and_then(|n| n.to_i64());
            if let Some(t) = space_type {
                out.insert(ident.to_uppercase(), t);
            }
        }
        out
    }

    pub fn displays() -> Result<Vec<Display>, String> {
        let ids = CGDisplay::active_displays().map_err(|e| format!("CGGetActiveDisplayList failed: {e:?}"))?;
        let spaces = current_space_types();
        Ok(ids
            .into_iter()
            .map(|id| {
                let d = CGDisplay::new(id);
                let b = d.bounds();
                let uuid = display_uuid(id).unwrap_or_default();
                Display {
                    id,
                    space: space_kind(spaces.get(&uuid.to_uppercase()).copied()),
                    uuid,
                    bounds: Rect { x: b.origin.x, y: b.origin.y, w: b.size.width, h: b.size.height },
                    builtin: d.is_builtin(),
                }
            })
            .collect())
    }

    pub fn windows() -> Result<Vec<Window>, String> {
        // No ExcludeDesktopElements: sketchybar draws at layer -20, which CoreGraphics
        // counts as a desktop element, so excluding those hides most of the bar.
        let raw = copy_window_info(kCGWindowListOptionAll, 0).ok_or("CGWindowListCopyWindowInfo returned nothing")?;
        let arr: CFArray<CFDictionary<CFString, CFType>> = unsafe { CFArray::wrap_under_get_rule(raw.as_concrete_TypeRef()) };

        let mut out = Vec::new();
        for entry in arr.iter() {
            let Some(owner) = get(&entry, "kCGWindowOwnerName").and_then(|v| v.downcast::<CFString>()) else { continue };
            // kCGWindowIsOnscreen is a CFBoolean; reading it as a CFNumber yields
            // None for every window, i.e. "nothing is on screen".
            let onscreen = get(&entry, "kCGWindowIsOnscreen")
                .map(|v| {
                    if let Some(b) = v.downcast::<CFBoolean>() {
                        bool::from(b)
                    } else {
                        v.downcast::<CFNumber>().and_then(|n| n.to_i64()).is_some_and(|n| n != 0)
                    }
                })
                .unwrap_or(false);
            let layer = get(&entry, "kCGWindowLayer").and_then(|v| v.downcast::<CFNumber>()).and_then(|n| n.to_i64()).unwrap_or(0);
            let Some(r) = get(&entry, "kCGWindowBounds")
                .and_then(|v| v.downcast::<CFDictionary>())
                .and_then(|d| core_graphics::geometry::CGRect::from_dict_representation(&d))
            else {
                continue;
            };
            out.push(Window {
                owner: owner.to_string(),
                bounds: Rect { x: r.origin.x, y: r.origin.y, w: r.size.width, h: r.size.height },
                onscreen,
                layer,
            });
        }
        Ok(out)
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{Display, Window};
    pub fn displays() -> Result<Vec<Display>, String> {
        Err("the window server is macOS only".into())
    }
    pub fn windows() -> Result<Vec<Window>, String> {
        Err("the window server is macOS only".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space_types_map_to_names() {
        assert_eq!(space_kind(Some(0)), "desktop");
        assert_eq!(space_kind(Some(4)), "fullscreen");
        assert_eq!(space_kind(Some(2)), "other");
        assert_eq!(space_kind(None), "unknown");
    }

    #[test]
    fn an_empty_owner_is_refused() {
        assert!(snapshot("").is_err());
    }

    // Needs a logged-in GUI session: `cargo test -p kennel-daemon -- --ignored`.
    #[test]
    #[ignore]
    fn reads_every_displays_space_from_the_live_window_server() {
        let s = snapshot("Dock").unwrap();
        assert!(!s.displays.is_empty());
        assert!(s.displays.iter().all(|d| d.space != "unknown"), "{:?}", s.displays);
    }
}
