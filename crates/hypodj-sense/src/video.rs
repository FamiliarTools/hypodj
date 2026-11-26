//! Frames from a video FILE, decoded by ffmpeg into 8-bit gray at a fixed size
//! and rate. Never a device: this slice must not be able to open the camera.

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};

/// Analysis geometry: 320x240 at 10 fps. Nyquist is 5 Hz, which still covers the
/// 0.5-4 Hz beat band (2x of 120 bpm is 4 Hz). 15 fps was measured at about 10%
/// of one core for the detector alone on bubble-gum, at the budget's edge; 10 fps
/// keeps it near 7% (see the design doc's measured results).
pub const WIDTH: usize = 320;
pub const HEIGHT: usize = 240;
pub const FPS: f64 = 10.0;

/// The ffmpeg binary: `DJ_SENSE_FFMPEG`, else `ffmpeg` on PATH.
pub fn ffmpeg() -> String {
    std::env::var("DJ_SENSE_FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
}

pub struct Frames {
    child: Child,
    buf: Vec<u8>,
}

impl Frames {
    pub fn open(path: &Path) -> Result<Self> {
        if !path.is_file() {
            bail!("{} is not a regular file (dj-sense reads video files only)", path.display());
        }
        let child = Command::new(ffmpeg())
            .args(["-nostdin", "-v", "error", "-threads", "1", "-i"])
            .arg(path)
            .args([
                "-an",
                "-vf",
                &format!("fps={FPS},scale={WIDTH}:{HEIGHT}:flags=bilinear,format=gray"),
                "-f",
                "rawvideo",
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawning {}", ffmpeg()))?;
        Ok(Frames { child, buf: vec![0; WIDTH * HEIGHT] })
    }

    /// The next frame, or `None` at end of stream.
    pub fn next_frame(&mut self) -> Result<Option<&[u8]>> {
        let out = self.child.stdout.as_mut().expect("piped");
        match out.read_exact(&mut self.buf) {
            Ok(()) => Ok(Some(&self.buf)),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                let st = self.child.wait()?;
                if !st.success() {
                    bail!("ffmpeg failed: {st}");
                }
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
