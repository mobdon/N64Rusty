//! Headless capture of chosen frames to PNG files.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};

use crate::{Frame, Image, RenderError, Renderer};

/// Write an image as an 8-bit RGBA PNG.
pub fn write_png(path: &Path, image: &Image) -> io::Result<()> {
    let mut enc = png::Encoder::new(
        BufWriter::new(File::create(path)?),
        image.width,
        image.height,
    );
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(io::Error::other)?;
    w.write_image_data(&image.rgba).map_err(io::Error::other)?;
    w.finish().map_err(io::Error::other)
}

/// Saves the frames whose numbers were chosen, as `frame_<n>.png` in a
/// folder. Frames that were not chosen are not rendered at all.
pub struct Capture {
    dir: PathBuf,
    frames: BTreeSet<u64>,
    /// Output size; `None` renders at the frame's native size.
    pub size: Option<(u32, u32)>,
}

impl Capture {
    pub fn new(dir: impl Into<PathBuf>, frames: impl IntoIterator<Item = u64>) -> Self {
        Self {
            dir: dir.into(),
            frames: frames.into_iter().collect(),
            size: None,
        }
    }

    /// Whether frame `n` is one to save.
    pub fn wants(&self, n: u64) -> bool {
        self.frames.contains(&n)
    }

    /// Render and save frame `n` if it was chosen; returns the file written.
    pub fn frame(
        &self,
        n: u64,
        renderer: &mut Renderer,
        frame: &Frame,
    ) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
        if !self.wants(n) {
            return Ok(None);
        }
        let (w, h) = self.size.unwrap_or((frame.width, frame.height()));
        let image: Image = renderer
            .render(frame, w, h)
            .map_err(Box::<RenderError>::new)?;
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("frame_{n:06}.png"));
        write_png(&path, &image)?;
        Ok(Some(path))
    }
}
