//! The menu bar / tray widget (design 11, D-23): where its window goes and which screens of the
//! main window it may open. The Tauri side (`apps/desktop/src-tauri/src/tray.rs`) only wires
//! these to the tray icon and the windows, so the logic is tested here on every OS.
//!
//! The widget is read-only like the rest of the desktop (SAFE-10): it shows free space, the
//! forecast, the ready-to-review plan and alerts, and opens the main window. It never runs a
//! cleanup; Quick Clean from the widget is the same refused stub as on the Overview.

use scull_core::{ErrorKind, ScullError};

/// The widget window's label (also listed in `capabilities/default.json`).
pub const WIDGET_LABEL: &str = "menubar";

/// The page the widget window loads (a separate Vite entry, `apps/desktop/menubar.html`).
pub const WIDGET_URL: &str = "menubar.html";

/// The widget's size in logical pixels (design 11: 380 × 660, the paper margin included).
pub const WIDGET_SIZE: (f64, f64) = (380.0, 660.0);

/// Gap between the tray icon (or the screen edge) and the widget, in physical pixels.
pub const WIDGET_MARGIN: i32 = 8;

/// The event that tells the main window which screen to show (payload: the route).
pub const NAVIGATE_EVENT: &str = "scull://navigate";

/// The main-window routes the widget may open: Overview, Cleanup Review (Review), Caches,
/// Worktrees and Insights (alerts) and Settings (the gear).
pub const MAIN_ROUTES: &[&str] = &[
    "/",
    "/cleanup",
    "/caches",
    "/worktrees",
    "/insights",
    "/settings",
];

/// `route` if the widget may open it, else a usage error (a page can't steer the main window
/// anywhere else).
pub fn main_route(route: &str) -> Result<&'static str, ScullError> {
    MAIN_ROUTES
        .iter()
        .copied()
        .find(|r| *r == route)
        .ok_or_else(|| {
            ScullError::new(
                ErrorKind::Usage,
                format!("the menu bar widget cannot open {route:?}"),
            )
        })
}

/// A rectangle in physical pixels (screen coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PxRect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

impl PxRect {
    fn right(&self) -> i64 {
        i64::from(self.x) + i64::from(self.width)
    }
    fn bottom(&self) -> i64 {
        i64::from(self.y) + i64::from(self.height)
    }
}

/// Where the widget's top-left corner goes, in physical pixels.
///
/// * Next to the tray icon (`anchor`) when the platform reports where it is (macOS, Windows):
///   centred under an icon in the top half of the screen (macOS menu bar, a top panel), above
///   one in the bottom half (Windows taskbar).
/// * In the top-right corner of the work area when it doesn't (Linux app indicators report no
///   position; the widget opens from the tray menu there).
///
/// Always kept inside `work_area` (minus [`WIDGET_MARGIN`]) when it fits; a work area smaller
/// than the widget pins it to the top-left corner.
pub fn popover_position(anchor: Option<PxRect>, work_area: PxRect, size: (u32, u32)) -> (i32, i32) {
    let m = i64::from(WIDGET_MARGIN);
    let (w, h) = (i64::from(size.0), i64::from(size.1));
    let (x, y) = match anchor {
        Some(a) => {
            let cx = i64::from(a.x) + i64::from(a.width) / 2;
            let cy = i64::from(a.y) + i64::from(a.height) / 2;
            let mid = i64::from(work_area.y) + i64::from(work_area.height) / 2;
            let y = if cy < mid {
                a.bottom() + m
            } else {
                i64::from(a.y) - h - m
            };
            (cx - w / 2, y)
        }
        None => (work_area.right() - w - m, i64::from(work_area.y) + m),
    };
    let clamp = |v: i64, lo: i64, hi: i64| if hi < lo { lo } else { v.clamp(lo, hi) };
    let x = clamp(x, i64::from(work_area.x) + m, work_area.right() - w - m);
    let y = clamp(y, i64::from(work_area.y) + m, work_area.bottom() - h - m);
    let to_i32 = |v: i64| i32::try_from(v).unwrap_or(if v < 0 { i32::MIN } else { i32::MAX });
    (to_i32(x), to_i32(y))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: (u32, u32) = (760, 1320); // 380 × 660 at 2×

    fn rect(x: i32, y: i32, width: u32, height: u32) -> PxRect {
        PxRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn routes_are_an_allowlist() {
        for r in MAIN_ROUTES {
            assert_eq!(main_route(r).unwrap(), *r);
        }
        for bad in [
            "",
            "/onboarding",
            "https://example.com",
            "/cleanup?x=1",
            "/../x",
            "javascript:alert(1)",
        ] {
            let e = main_route(bad).unwrap_err();
            assert_eq!(e.kind, ErrorKind::Usage, "{bad}");
        }
    }

    #[test]
    fn macos_menu_bar_centres_the_widget_under_the_icon() {
        // 3024 × 1964 Retina screen, 74 px menu bar above the work area, icon near the right.
        let work = rect(0, 74, 3024, 1890);
        let icon = rect(2400, 0, 64, 74);
        let (x, y) = popover_position(Some(icon), work, SIZE);
        assert_eq!(x, 2400 + 32 - 380);
        assert_eq!(y, 74 + WIDGET_MARGIN);
    }

    #[test]
    fn windows_taskbar_opens_the_widget_above_the_icon_inside_the_screen() {
        // 1920 × 1080, 48 px taskbar at the bottom, icon in the notification area.
        let work = rect(0, 0, 1920, 1032);
        let icon = rect(1800, 1040, 32, 32);
        let (x, y) = popover_position(Some(icon), work, (380, 660));
        // Above the icon, and kept above the taskbar (the work area's bottom edge).
        assert_eq!(y, 1032 - 660 - WIDGET_MARGIN);
        // Centred would cross the right edge: pinned inside it.
        assert_eq!(x, 1920 - 380 - WIDGET_MARGIN);
    }

    #[test]
    fn no_anchor_goes_to_the_top_right_corner() {
        let work = rect(0, 27, 1920, 1053);
        let (x, y) = popover_position(None, work, (380, 660));
        assert_eq!((x, y), (1920 - 380 - WIDGET_MARGIN, 27 + WIDGET_MARGIN));
    }

    #[test]
    fn secondary_monitor_coordinates_and_tiny_screens() {
        // A monitor left of the primary one has negative coordinates.
        let work = rect(-1920, 0, 1920, 1080);
        let icon = rect(-1900, 0, 30, 24);
        let (x, y) = popover_position(Some(icon), work, (380, 660));
        assert_eq!(x, -1920 + WIDGET_MARGIN);
        assert_eq!(y, 24 + WIDGET_MARGIN);
        // Smaller than the widget: top-left of the work area, never off-screen to the left/top.
        let tiny = rect(10, 20, 300, 400);
        assert_eq!(
            popover_position(None, tiny, (380, 660)),
            (10 + WIDGET_MARGIN, 20 + WIDGET_MARGIN)
        );
    }
}
