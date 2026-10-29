//! RTSP via an `ffmpeg` child process emitting MJPEG on stdout.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use ss_core::{CameraId, Error, Frame, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::task::JoinHandle;

use crate::mjpeg::JpegSplitter;
use crate::{camera_error, make_frame, FrameSource};

/// Program used for decoding: `$SS_FFMPEG` if set and non-empty, else `ffmpeg`.
fn ffmpeg_bin() -> String {
    std::env::var("SS_FFMPEG")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "ffmpeg".to_string())
}

/// True if an `ffmpeg` binary (or `$SS_FFMPEG`) is runnable.
pub fn ffmpeg_available() -> bool {
    std::process::Command::new(ffmpeg_bin())
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Percent-encode everything except RFC 3986 unreserved characters.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Return `url` with its userinfo replaced by the given credentials (percent-encoded).
/// Without a username the URL is returned unchanged.
fn with_credentials(url: &str, username: Option<&str>, password: Option<&str>) -> String {
    let Some(user) = username.filter(|u| !u.is_empty()) else {
        return url.to_string();
    };
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let (scheme, rest) = url.split_at(scheme_end + 3);
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    // Anything before the last '@' in the authority is the old userinfo.
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let mut userinfo = percent_encode(user);
    if let Some(pass) = password.filter(|p| !p.is_empty()) {
        userinfo.push(':');
        userinfo.push_str(&percent_encode(pass));
    }
    format!("{scheme}{userinfo}@{host}{tail}")
}

/// ffmpeg arguments used for a stream (exposed for testing). Credentials are
/// injected into the URL userinfo, percent-encoded.
pub fn ffmpeg_args(
    url: &str,
    username: Option<&str>,
    password: Option<&str>,
    fps: u32,
) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-rtsp_transport".into(),
        "tcp".into(),
        "-i".into(),
        with_credentials(url, username, password),
        "-an".into(),
        "-f".into(),
        "image2pipe".into(),
        "-c:v".into(),
        "mjpeg".into(),
        "-q:v".into(),
        "5".into(),
        "-r".into(),
        fps.max(1).to_string(),
        "pipe:1".into(),
    ]
}

pub struct RtspSource {
    camera_id: CameraId,
    /// Kept so that the process is killed when the source is dropped.
    child: Child,
    stdout: ChildStdout,
    splitter: JpegSplitter,
    pending: VecDeque<Bytes>,
    read_buf: Vec<u8>,
    stderr_task: Option<JoinHandle<()>>,
    /// Last non-empty stderr line written by ffmpeg.
    last_stderr: Arc<Mutex<Option<String>>>,
}

impl RtspSource {
    pub async fn spawn(
        camera_id: CameraId,
        url: &str,
        username: Option<&str>,
        password: Option<&str>,
        fps: u32,
    ) -> Result<Self> {
        let args = ffmpeg_args(url, username, password, fps);
        let mut child = Command::new(ffmpeg_bin())
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| camera_error(format!("cannot start ffmpeg: {e}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| camera_error("ffmpeg stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| camera_error("ffmpeg stderr unavailable"))?;

        let last_stderr = Arc::new(Mutex::new(None::<String>));
        let writer = Arc::clone(&last_stderr);
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let text = line.trim();
                if !text.is_empty() {
                    if let Ok(mut slot) = writer.lock() {
                        *slot = Some(text.to_string());
                    }
                }
            }
        });

        Ok(Self {
            camera_id,
            child,
            stdout,
            splitter: JpegSplitter::new(),
            pending: VecDeque::new(),
            read_buf: vec![0u8; 64 * 1024],
            stderr_task: Some(stderr_task),
            last_stderr,
        })
    }

    /// Called once stdout is closed: reap the process and build the error message.
    async fn end_of_stream(&mut self) -> Error {
        let status = match self.child.wait().await {
            Ok(status) => status.to_string(),
            Err(e) => format!("wait failed: {e}"),
        };
        if let Some(task) = self.stderr_task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
        }
        let reason = self
            .last_stderr
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
            .unwrap_or_else(|| "no error output".to_string());
        camera_error(format!("ffmpeg ended ({status}): {reason}"))
    }
}

#[async_trait]
impl FrameSource for RtspSource {
    async fn next_frame(&mut self) -> Result<Frame> {
        loop {
            if let Some(jpeg) = self.pending.pop_front() {
                return Ok(make_frame(self.camera_id, jpeg));
            }
            match self.stdout.read(&mut self.read_buf).await {
                Ok(0) => return Err(self.end_of_stream().await),
                Ok(n) => {
                    let frames = self.splitter.push(&self.read_buf[..n]);
                    self.pending.extend(frames);
                }
                Err(e) => return Err(camera_error(format!("ffmpeg read failed: {e}"))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_match_spec() {
        let args = ffmpeg_args("rtsp://cam.local/stream1", None, None, 10);
        assert_eq!(
            args.join(" "),
            "-hide_banner -loglevel error -rtsp_transport tcp -i rtsp://cam.local/stream1 \
             -an -f image2pipe -c:v mjpeg -q:v 5 -r 10 pipe:1"
        );
    }

    #[test]
    fn credentials_are_injected_and_encoded() {
        let args = ffmpeg_args(
            "rtsp://cam.local:554/live?x=1",
            Some("ad min"),
            Some("p@ss:w/rd"),
            5,
        );
        assert_eq!(
            args[6],
            "rtsp://ad%20min:p%40ss%3Aw%2Frd@cam.local:554/live?x=1"
        );
    }

    #[test]
    fn existing_userinfo_is_replaced() {
        assert_eq!(
            with_credentials("rtsp://old:secret@cam.local/s", Some("new"), Some("pw")),
            "rtsp://new:pw@cam.local/s"
        );
        assert_eq!(
            with_credentials("rtsp://old:secret@cam.local/s", Some("new"), None),
            "rtsp://new@cam.local/s"
        );
    }

    #[test]
    fn no_username_keeps_url() {
        assert_eq!(
            with_credentials("rtsp://cam.local/s", None, Some("pw")),
            "rtsp://cam.local/s"
        );
        assert_eq!(
            with_credentials("rtsps://u:p@h/s", Some(""), None),
            "rtsps://u:p@h/s"
        );
    }

    #[test]
    fn percent_encode_keeps_unreserved() {
        assert_eq!(percent_encode("Az09-._~"), "Az09-._~");
        assert_eq!(percent_encode("é"), "%C3%A9");
    }
}
