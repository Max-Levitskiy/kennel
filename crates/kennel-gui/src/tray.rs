use crate::commands::{Commands, UiCommand};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// What the tray is reporting right now. Compared frame to frame so the icon
/// and tooltip are only pushed to the OS when something actually changed,
/// instead of on every one of the GUI's 1 Hz frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// The control socket could not be reached -- nothing is being watched,
    /// which is worse than an unhealthy monitor and is reported first.
    DaemonDown,
    AllHealthy,
    Unhealthy(usize),
}

pub struct Tray {
    icon: TrayIcon,
    reported: Option<Health>,
}

impl Tray {
    /// Builds the menu bar item. Returns `None` if the platform refuses us a
    /// tray icon; the caller must then keep the window closable-to-quit,
    /// otherwise there would be no way left to reach the app at all.
    pub fn new(commands: Commands) -> Option<Tray> {
        let open = MenuItem::new("Open Kennel", true, None);
        let quit = MenuItem::new("Quit Kennel", true, None);
        let open_id = open.id().clone();
        let quit_id = quit.id().clone();

        let menu = Menu::new();
        menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit]).ok()?;

        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(tooltip(Health::DaemonDown))
            .with_icon(icon_for(Health::DaemonDown))
            .build()
            .ok()?;

        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id == open_id {
                commands.send(UiCommand::Open);
            } else if event.id == quit_id {
                commands.send(UiCommand::Quit);
            }
        }));

        Some(Tray { icon, reported: None })
    }

    pub fn report(&mut self, health: Health) {
        if self.reported == Some(health) {
            return;
        }
        if self.reported.is_none() {
            // The tray icon *is* this app's UI when the window is hidden, so
            // where the OS actually put it is worth one line in the log.
            match self.icon.rect() {
                Some(rect) => eprintln!("kennel-gui: menu bar icon at {:?} sized {:?}", rect.position, rect.size),
                None => eprintln!("kennel-gui: menu bar icon has no place in the menu bar"),
            }
        }
        self.reported = Some(health);
        let _ = self.icon.set_icon(Some(icon_for(health)));
        let _ = self.icon.set_tooltip(Some(tooltip(health)));
    }
}

fn tooltip(health: Health) -> String {
    match health {
        Health::DaemonDown => "kennel: daemon not running".to_string(),
        Health::AllHealthy => "kennel: all healthy".to_string(),
        Health::Unhealthy(count) => format!("kennel: {count} unhealthy"),
    }
}

/// Icon side in pixels. The menu bar scales whatever we hand it to 18pt tall,
/// so this is drawn at twice that for retina and supersampled below.
const SIDE: u32 = 36;

/// A doghouse silhouette in the health color: roof triangle over a body, with
/// a round-topped door punched out. Drawn rather than shipped as an asset so
/// the three health colors come from one source of truth.
fn icon_for(health: Health) -> Icon {
    Icon::from_rgba(kennel_glyph(color_for(health)), SIDE, SIDE).expect("kennel_glyph always yields SIDE*SIDE RGBA pixels")
}

fn color_for(health: Health) -> [u8; 3] {
    match health {
        Health::DaemonDown => [0x8e, 0x8e, 0x93],
        Health::AllHealthy => [0x30, 0xb0, 0x60],
        Health::Unhealthy(_) => [0xd7, 0x3a, 0x2e],
    }
}

fn kennel_glyph(color: [u8; 3]) -> Vec<u8> {
    const SAMPLES: u32 = 3; // per axis, so 9 coverage samples per pixel
    let mut pixels = Vec::with_capacity((SIDE * SIDE * 4) as usize);
    for y in 0..SIDE {
        for x in 0..SIDE {
            let mut covered = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let fx = (x as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / SIDE as f32;
                    let fy = (y as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / SIDE as f32;
                    if inside_glyph(fx, fy) {
                        covered += 1;
                    }
                }
            }
            let alpha = (255 * covered / (SAMPLES * SAMPLES)) as u8;
            pixels.extend_from_slice(&[color[0], color[1], color[2], alpha]);
        }
    }
    pixels
}

/// Coverage test in normalized `0.0..1.0` glyph space, origin top-left.
fn inside_glyph(x: f32, y: f32) -> bool {
    const ROOF_APEX_Y: f32 = 0.08;
    const ROOF_BASE_Y: f32 = 0.42;
    const ROOF_HALF_WIDTH: f32 = 0.46;

    let roof = y >= ROOF_APEX_Y && y <= ROOF_BASE_Y && (x - 0.5).abs() <= ROOF_HALF_WIDTH * (y - ROOF_APEX_Y) / (ROOF_BASE_Y - ROOF_APEX_Y);
    let body = (0.20..=0.80).contains(&x) && (ROOF_BASE_Y..=0.92).contains(&y);
    let door_arch = (x - 0.5).powi(2) + (y - 0.66).powi(2) <= 0.16_f32.powi(2);
    let door_shaft = (0.34..=0.66).contains(&x) && (0.66..=0.92).contains(&y);

    (roof || body) && !(door_arch || door_shaft)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel_at(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
        let i = ((y * SIDE + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
    }

    #[test]
    fn glyph_is_a_doghouse_in_the_requested_color() {
        let pixels = kennel_glyph([1, 2, 3]);
        assert_eq!(pixels.len() as u32, SIDE * SIDE * 4);

        // Mid-roof is solid glyph, in the requested color.
        assert_eq!(pixel_at(&pixels, SIDE / 2, SIDE / 3), [1, 2, 3, 255]);
        // The doorway is punched through, so the icon reads as a kennel and
        // not as a plain colored blob.
        assert_eq!(pixel_at(&pixels, SIDE / 2, SIDE - 4)[3], 0);
        // Corners are outside the silhouette.
        assert_eq!(pixel_at(&pixels, 0, 0)[3], 0);
        assert_eq!(pixel_at(&pixels, SIDE - 1, 0)[3], 0);
    }

    #[test]
    fn each_health_state_gets_a_distinct_color() {
        let colors: Vec<[u8; 3]> = [Health::DaemonDown, Health::AllHealthy, Health::Unhealthy(2)].into_iter().map(color_for).collect();
        assert_ne!(colors[0], colors[1]);
        assert_ne!(colors[1], colors[2]);
        assert_ne!(colors[0], colors[2]);
    }
}
