//! Screenshot implementation - xcap + custom cropping + graphics backend dispatch

use crate::core::*;
use xcap::{Monitor, Window as XcapWindow};

/// Screen rectangle covered by a captured frame.
///
/// `x`/`y` are the screen coordinates of the image's top-left pixel: add them to
/// any image-space coordinate to obtain the screen coordinate to act on. Captures
/// read full-screen pixels and crop them, so a region starting outside the screen
/// is clamped onto it and a region reaching past the edge is shortened - the
/// frame's own width/height stay authoritative.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CaptureGeometry {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl CaptureGeometry {
    /// Translate an image-space point into screen coordinates.
    pub fn to_screen(&self, image_point: Point) -> Point {
        Point {
            x: self.x.saturating_add(image_point.x),
            y: self.y.saturating_add(image_point.y),
        }
    }

    /// Translate an image-space rect into screen coordinates.
    pub fn rect_to_screen(&self, image_rect: Rect) -> Rect {
        Rect {
            x: self.x.saturating_add(image_rect.x),
            y: self.y.saturating_add(image_rect.y),
            w: image_rect.w,
            h: image_rect.h,
        }
    }
}

/// Capture a screenshot together with the desktop geometry it covers.
///
/// The geometry is sampled before and after the capture: if it changed, the
/// window or display moved while the image was being taken and the image can no
/// longer be mapped back to screen coordinates. Such a frame is rejected instead
/// of being handed to callers who would act on the wrong pixels.
pub async fn capture_with_geometry(
    target: &Target,
    scope: Scope,
) -> Result<(Frame, CaptureGeometry)> {
    let before = scope_geometry(target, scope)?;
    let frame = capture(target, scope).await?;
    let after = scope_geometry(target, scope)?;
    Ok((frame, verify_stable_geometry(before, after)?))
}

/// Fail when two samples of the same scope disagree.
fn verify_stable_geometry(
    before: CaptureGeometry,
    after: CaptureGeometry,
) -> Result<CaptureGeometry> {
    if before != after {
        return Err(DesktopError::CaptureFailed(
            "the window or display changed while the screenshot was being taken; capture again"
                .to_string(),
        ));
    }
    Ok(before)
}

/// Screen rectangle that `capture` produces for `scope`.
///
/// The target is unused today - only window scopes need it, and those have no
/// reportable geometry - but it stays in the signature so this mirrors `capture`
/// and a future window implementation needs no call-site changes.
fn scope_geometry(_target: &Target, scope: Scope) -> Result<CaptureGeometry> {
    let error = |e: xcap::XCapError| DesktopError::CaptureFailed(e.to_string());
    match scope {
        // `capture_region` crops full-screen pixels, so an origin off the screen
        // is clamped to the screen origin - report where the image really starts.
        Scope::Element { x, y, w, h } => Ok(CaptureGeometry {
            x: x.max(0),
            y: y.max(0),
            width: w,
            height: h,
        }),
        Scope::Point { x, y, radius } => {
            // `capture` computes `radius * 2` in u32 and `x - radius as i32` in
            // i32: radii that would wrap or overflow there are rejected up front.
            let offset = i32::try_from(radius)
                .map_err(|_| DesktopError::CaptureFailed("invalid radius".to_string()))?;
            let origin = |value: i32| {
                value
                    .checked_sub(offset)
                    .map(|v| v.max(0))
                    .ok_or_else(|| DesktopError::CaptureFailed("invalid point origin".to_string()))
            };
            Ok(CaptureGeometry {
                x: origin(x)?,
                y: origin(y)?,
                width: radius * 2,
                height: radius * 2,
            })
        }
        Scope::Fullscreen => {
            let monitor = first_monitor()?;
            Ok(CaptureGeometry {
                x: monitor.x().map_err(error)?,
                y: monitor.y().map_err(error)?,
                width: monitor.width().map_err(error)?,
                height: monitor.height().map_err(error)?,
            })
        }
        // Window scopes are deliberately unsupported: `capture` picks the window
        // strategy at runtime (`Gdi` returns an xcap window image, the full-screen
        // fallback crops `GetWindowRect`), so no single frame origin exists before
        // the capture runs. A guessed origin would silently act on wrong pixels,
        // so the limitation is reported instead.
        Scope::Window | Scope::ClientArea => Err(DesktopError::CaptureFailed(
            "capture geometry is unavailable for window scopes: the frame origin depends on the capture backend"
                .to_string(),
        )),
    }
}

/// The monitor that full-screen and region captures read from - the first entry
/// of xcap's monitor list. Kept in one place so `scope_geometry` always describes
/// the frame `capture` actually produced.
fn first_monitor() -> Result<Monitor> {
    Monitor::all()
        .map_err(|e| DesktopError::CaptureFailed(e.to_string()))?
        .into_iter()
        .next()
        .ok_or_else(|| DesktopError::CaptureFailed("no monitor found".to_string()))
}

/// Capture a screenshot - according to the target and scope
pub async fn capture(target: &Target, scope: Scope) -> Result<Frame> {
    match scope {
        Scope::Fullscreen => capture_fullscreen().await,
        Scope::Window => capture_window(target).await,
        Scope::ClientArea => capture_client_area(target).await,
        Scope::Element { x, y, w, h } => capture_region(x, y, w, h).await,
        Scope::Point { x, y, radius } => {
            let size = radius * 2;
            capture_region(x - radius as i32, y - radius as i32, size, size).await
        }
    }
}

/// Full-screen capture
async fn capture_fullscreen() -> Result<Frame> {
    let monitor = first_monitor()?;

    let image = monitor
        .capture_image()
        .map_err(|e| DesktopError::CaptureFailed(e.to_string()))?;
    convert_to_frame(image, Scope::Fullscreen, FrameSource::Screenshot)
}

/// Window capture - dispatch strategy by graphics backend
async fn capture_window(target: &Target) -> Result<Frame> {
    #[cfg(not(windows))]
    let _ = target; // Target::Window only exists on Windows; fall through below.
    #[cfg(windows)]
    {
        if let Target::Window {
            hwnd, gfx_backend, ..
        } = target
        {
            return capture_window_by_backend(*hwnd, *gfx_backend).await;
        }
    }

    // Fallback: full-screen capture (all platforms). Target::Window only exists
    // on Windows, so a non-Windows window capture always lands here.
    capture_fullscreen().await
}

/// Dispatch the capture strategy by graphics backend
async fn capture_window_by_backend(hwnd: isize, gfx: GfxBackend) -> Result<Frame> {
    match gfx {
        GfxBackend::Gdi => capture_window_gdi(hwnd).await,
        GfxBackend::DirectX | GfxBackend::Unknown => {
            // Try GDI first, fall back to full-screen + crop on failure
            match capture_window_gdi(hwnd).await {
                Ok(frame) => Ok(frame),
                Err(_) => capture_fullscreen_and_crop(hwnd).await,
            }
        }
        GfxBackend::OpenGl | GfxBackend::Vulkan => {
            // OGL/Vulkan windows capture black screens via GDI, go straight to full-screen + crop
            capture_fullscreen_and_crop(hwnd).await
        }
    }
}

/// GDI window capture (xcap)
async fn capture_window_gdi(hwnd: isize) -> Result<Frame> {
    let windows = XcapWindow::all().map_err(|e| DesktopError::CaptureFailed(e.to_string()))?;
    let win = windows
        .into_iter()
        .find(|w| w.id().ok().map(|id| id as isize) == Some(hwnd))
        .ok_or_else(|| DesktopError::CaptureFailed(format!("window {} not found", hwnd)))?;

    let image = win
        .capture_image()
        .map_err(|e| DesktopError::CaptureFailed(e.to_string()))?;
    convert_to_frame(image, Scope::Window, FrameSource::WindowCapture)
}

/// Full-screen capture + crop to the window position (fallback strategy)
async fn capture_fullscreen_and_crop(hwnd: isize) -> Result<Frame> {
    let frame = capture_fullscreen().await?;

    #[cfg(windows)]
    {
        use ::windows::Win32::Foundation::{HWND, RECT};
        use ::windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

        let mut rect = RECT::default();
        let _ = unsafe { GetWindowRect(HWND(hwnd), &mut rect) };

        let x = rect.left.max(0) as u32;
        let y = rect.top.max(0) as u32;
        let w = (rect.right - rect.left) as u32;
        let h = (rect.bottom - rect.top) as u32;

        frame
            .crop(x, y, w, h)
            .ok_or_else(|| DesktopError::CaptureFailed("fullscreen crop failed".to_string()))
    }

    #[cfg(not(windows))]
    {
        // macOS/Linux: use xcap to get the window position for cropping
        let windows = XcapWindow::all().map_err(|e| DesktopError::CaptureFailed(e.to_string()))?;
        let win = windows
            .into_iter()
            .find(|w| w.id().ok().map(|id| id as isize) == Some(hwnd))
            .ok_or_else(|| DesktopError::CaptureFailed(format!("window {} not found", hwnd)))?;

        // xcap 0.9 returns Result from x()/y()/width()/height() — default to 0
        // on error so a stale window handle degrades to a full-screen crop.
        let x = win.x().unwrap_or(0).max(0) as u32;
        let y = win.y().unwrap_or(0).max(0) as u32;
        let w = win.width().unwrap_or(0);
        let h = win.height().unwrap_or(0);

        frame
            .crop(x, y, w, h)
            .ok_or_else(|| DesktopError::CaptureFailed("fullscreen crop failed".to_string()))
    }
}

/// Client area capture (without the title bar border)
async fn capture_client_area(target: &Target) -> Result<Frame> {
    #[cfg(windows)]
    {
        use ::windows::Win32::Foundation::{HWND, POINT, RECT};
        use ::windows::Win32::Graphics::Gdi::ClientToScreen;
        use ::windows::Win32::UI::WindowsAndMessaging::GetClientRect;

        if let Target::Window { hwnd, .. } = target {
            let hwnd = HWND(*hwnd);
            let mut client_rect = RECT::default();
            let mut point = POINT { x: 0, y: 0 };

            unsafe {
                let _ = GetClientRect(hwnd, &mut client_rect);
                let _ = ClientToScreen(hwnd, &mut point);
            }

            let x = point.x;
            let y = point.y;
            let w = (client_rect.right - client_rect.left) as u32;
            let h = (client_rect.bottom - client_rect.top) as u32;

            return capture_region(x, y, w, h).await;
        }
    }

    // Fallback
    capture_window(target).await
}

/// Region capture
async fn capture_region(x: i32, y: i32, w: u32, h: u32) -> Result<Frame> {
    let monitor = first_monitor()?;

    let image = monitor
        .capture_image()
        .map_err(|e| DesktopError::CaptureFailed(e.to_string()))?;
    let frame = convert_to_frame(image, Scope::Fullscreen, FrameSource::Screenshot)?;

    let x = x.max(0) as u32;
    let y = y.max(0) as u32;

    // Out-of-bounds guard: a region starting beyond the frame edge would make
    // `frame.width - x` / `frame.height - y` underflow (u32) and panic in debug
    // builds. Fail with a clear error instead of crashing the server.
    if x >= frame.width || y >= frame.height {
        return Err(DesktopError::CaptureFailed(format!(
            "capture region out of bounds: x={x}, y={y}, screen={}x{}",
            frame.width, frame.height
        )));
    }

    let w = w.min(frame.width - x);
    let h = h.min(frame.height - y);

    frame
        .crop(x, y, w, h)
        .ok_or_else(|| DesktopError::CaptureFailed("crop failed".to_string()))
}

/// Convert an xcap image to a Frame
fn convert_to_frame(
    image: xcap::image::RgbaImage,
    scope: Scope,
    source: FrameSource,
) -> Result<Frame> {
    let width = image.width();
    let height = image.height();
    let pixels = image.into_raw();

    Ok(Frame {
        id: uuid::Uuid::new_v4(),
        pixels,
        width,
        height,
        scope,
        timestamp: chrono::Utc::now(),
        source,
    })
}

#[cfg(test)]
mod geometry_tests {
    use super::*;

    /// Element/Point/Fullscreen geometry never reads the target, so the tests
    /// stay independent of any real display (CI has no desktop).
    fn unused_target() -> Target {
        Target::Tui {
            hwnd: 0,
            title: String::new(),
        }
    }

    fn geometry(scope: Scope) -> Result<CaptureGeometry> {
        scope_geometry(&unused_target(), scope)
    }

    /// `Point`/`Rect` deliberately do not implement `PartialEq` in `core`, so their
    /// fields are compared instead of adding a derive to the shared crate.
    fn point_of(point: Point) -> (i32, i32) {
        (point.x, point.y)
    }

    fn rect_of(rect: Rect) -> (i32, i32, u32, u32) {
        (rect.x, rect.y, rect.w, rect.h)
    }

    #[test]
    fn element_geometry_is_clamped_onto_the_screen() {
        assert_eq!(
            geometry(Scope::Element {
                x: 100,
                y: 50,
                w: 300,
                h: 200
            })
            .unwrap(),
            CaptureGeometry {
                x: 100,
                y: 50,
                width: 300,
                height: 200
            }
        );
        // `capture_region` clamps a negative origin to the screen origin, so the
        // reported geometry must clamp too instead of echoing the request.
        assert_eq!(
            geometry(Scope::Element {
                x: -50,
                y: -20,
                w: 300,
                h: 200
            })
            .unwrap(),
            CaptureGeometry {
                x: 0,
                y: 0,
                width: 300,
                height: 200
            }
        );
    }

    #[test]
    fn point_geometry_matches_the_region_capture() {
        assert_eq!(
            geometry(Scope::Point {
                x: 100,
                y: 50,
                radius: 10
            })
            .unwrap(),
            CaptureGeometry {
                x: 90,
                y: 40,
                width: 20,
                height: 20
            }
        );
        // A point near the top-left corner is clamped, keeping the same size.
        assert_eq!(
            geometry(Scope::Point {
                x: 5,
                y: 5,
                radius: 10
            })
            .unwrap(),
            CaptureGeometry {
                x: 0,
                y: 0,
                width: 20,
                height: 20
            }
        );
        assert_eq!(
            geometry(Scope::Point {
                x: 30,
                y: 40,
                radius: 0
            })
            .unwrap(),
            CaptureGeometry {
                x: 30,
                y: 40,
                width: 0,
                height: 0
            }
        );
    }

    #[test]
    fn point_geometry_rejects_radii_and_origins_that_would_overflow() {
        // `capture` computes `radius * 2` in u32 and `radius as i32` for the
        // offset: such a radius has no describable geometry, so it is rejected
        // here rather than wrapping (or panicking) inside the capture.
        assert!(geometry(Scope::Point {
            x: 0,
            y: 0,
            radius: u32::MAX,
        })
        .is_err());
        assert!(geometry(Scope::Point {
            x: 0,
            y: 0,
            radius: i32::MAX as u32 + 1,
        })
        .is_err());
        assert!(geometry(Scope::Point {
            x: i32::MIN,
            y: 0,
            radius: 1,
        })
        .is_err());
        // The largest representable radius still has a valid (huge) geometry.
        assert_eq!(
            geometry(Scope::Point {
                x: i32::MAX,
                y: i32::MAX,
                radius: i32::MAX as u32,
            })
            .unwrap(),
            CaptureGeometry {
                x: 0,
                y: 0,
                width: u32::MAX - 1,
                height: u32::MAX - 1,
            }
        );
    }

    #[test]
    fn geometry_that_changed_between_samples_is_rejected() {
        let before = CaptureGeometry {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(verify_stable_geometry(before, before).unwrap(), before);

        let moved = CaptureGeometry {
            x: 0,
            y: 40,
            width: 1920,
            height: 1080,
        };
        let resized = CaptureGeometry {
            x: 0,
            y: 0,
            width: 1280,
            height: 720,
        };
        for after in [moved, resized] {
            match verify_stable_geometry(before, after) {
                Err(DesktopError::CaptureFailed(message)) => {
                    assert!(message.contains("changed"), "unexpected message: {message}");
                }
                other => panic!("expected a capture failure, got {other:?}"),
            }
        }
    }

    #[test]
    fn window_scopes_have_no_geometry() {
        for scope in [Scope::Window, Scope::ClientArea] {
            assert!(
                geometry(scope).is_err(),
                "{scope:?} must not report geometry"
            );
        }
    }

    #[test]
    fn image_coordinates_translate_to_screen_coordinates() {
        let geometry = CaptureGeometry {
            x: 200,
            y: 100,
            width: 800,
            height: 600,
        };
        let image_rect = Rect {
            x: 10,
            y: 20,
            w: 40,
            h: 20,
        };

        // rect center = (10 + 40/2, 20 + 20/2) = (30, 30) → screen (230, 130).
        assert_eq!(
            point_of(geometry.to_screen(image_rect.center())),
            (230, 130)
        );
        assert_eq!(
            rect_of(geometry.rect_to_screen(image_rect)),
            (210, 120, 40, 20)
        );
        // `center` must stay the center of the translated rect, otherwise
        // callers would click a pixel that is not the element's middle.
        assert_eq!(
            point_of(geometry.rect_to_screen(image_rect).center()),
            point_of(geometry.to_screen(image_rect.center()))
        );
    }
}
