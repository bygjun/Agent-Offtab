use crate::model::Mode;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    path::Path,
};
use tauri::{LogicalSize, PhysicalPosition, WebviewWindow};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
struct Geometry {
    width: f64,
    height: f64,
    position: Option<[i32; 2]>,
}

impl Geometry {
    const fn new(width: f64, height: f64) -> Self {
        Self {
            width,
            height,
            position: None,
        }
    }

    fn capture(window: &WebviewWindow) -> tauri::Result<Self> {
        let size = window
            .inner_size()?
            .to_logical::<f64>(window.scale_factor()?);
        let position = window.outer_position()?;
        Ok(Self {
            width: size.width,
            height: size.height,
            position: Some([position.x, position.y]),
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Layout {
    #[serde(skip)]
    pub mode: Mode,
    compact: Geometry,
    full: Geometry,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            mode: Mode::Compact,
            compact: Geometry::new(420.0, 280.0),
            full: Geometry::new(1040.0, 740.0),
        }
    }
}

impl Layout {
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("Window settings need a directory"))?;
        fs::create_dir_all(parent)?;
        let mut pending = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(&mut pending, self)?;
        pending.flush()?;
        pending.persist(path).map_err(|error| error.error)?;
        Ok(())
    }

    fn geometry(&mut self, mode: Mode) -> &mut Geometry {
        match mode {
            Mode::Compact => &mut self.compact,
            Mode::Full => &mut self.full,
        }
    }

    pub fn remember(&mut self, window: &WebviewWindow) -> tauri::Result<()> {
        *self.geometry(self.mode) = Geometry::capture(window)?;
        Ok(())
    }

    pub fn restore(&mut self, window: &WebviewWindow, mode: Mode) -> tauri::Result<()> {
        let geometry = *self.geometry(mode);
        let (min_width, min_height) = match mode {
            Mode::Compact => (290.0, 162.0),
            Mode::Full => (360.0, 360.0),
        };
        let monitors = window.available_monitors()?;
        let visible = geometry.position.and_then(|[x, y]| {
            monitors
                .iter()
                .find(|monitor| {
                    let position = monitor.position();
                    let size = monitor.size();
                    let rel_x = i64::from(x) - i64::from(position.x);
                    let rel_y = i64::from(y) - i64::from(position.y);
                    rel_x >= -150
                        && rel_x + 44 < i64::from(size.width)
                        && rel_y >= 0
                        && rel_y + 28 < i64::from(size.height)
                })
                .map(|monitor| (x, y, monitor))
        });
        let available = visible
            .map(|(_, _, monitor)| monitor)
            .or_else(|| monitors.first());
        let max_size =
            available.map(|monitor| monitor.size().to_logical::<f64>(monitor.scale_factor()));
        let width = geometry
            .width
            .max(min_width)
            .min(max_size.map_or(f64::MAX, |size| size.width.max(min_width)));
        let height = geometry
            .height
            .max(min_height)
            .min(max_size.map_or(f64::MAX, |size| size.height.max(min_height)));
        window.set_min_size(Some(LogicalSize::new(min_width, min_height)))?;
        window.set_size(LogicalSize::new(width, height))?;
        if let Some((x, y, _)) = visible {
            window.set_position(PhysicalPosition::new(x, y))?;
        } else if mode == Mode::Full || geometry.position.is_some() {
            window.center()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_both_modes_independently_and_restores_them_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("window-layout.json");
        let mut layout = Layout::default();
        layout.full = Geometry {
            width: 973.0,
            height: 611.0,
            position: Some([1200, 321]),
        };
        layout.compact = Geometry {
            width: 512.0,
            height: 270.0,
            position: Some([-200, 70]),
        };
        layout.mode = Mode::Full;
        layout.save(&path).unwrap();
        let loaded = Layout::load(&path).unwrap();
        assert_eq!(loaded.full, layout.full);
        assert_eq!(loaded.compact, layout.compact);
        assert_eq!(loaded.mode, Mode::Compact);
    }
}
