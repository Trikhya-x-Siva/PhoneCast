//! Outputs that consume the phone stream: the recorder and live streams.
//!
//! Each output runs on its own thread behind a bounded queue, so a slow disk
//! or a struggling upload can never stall receiving from the phone (which is
//! what made the phone drop frames). If an output falls behind, it skips to
//! the next keyframe instead of building up delay.

use crate::mkv::{MkvWriter, VideoConfig};
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;

#[derive(Clone)]
pub enum Media {
    Config(VideoConfig),
    Video {
        pts: i64,
        key: bool,
        data: Arc<Vec<u8>>,
    },
    Audio {
        pts: i64,
        pcm: Arc<Vec<u8>>,
    },
}

/// Sending side of an output. Never blocks.
pub struct SinkTx {
    tx: SyncSender<Media>,
    need_key: bool,
}

pub enum Push {
    Ok,
    /// The output fell behind; a keyframe should be requested from the phone.
    NeedKeyframe,
    Closed,
}

impl SinkTx {
    pub fn new(capacity: usize) -> (SinkTx, Receiver<Media>) {
        let (tx, rx) = sync_channel(capacity);
        (SinkTx { tx, need_key: true }, rx)
    }

    pub fn push(&mut self, m: &Media) -> Push {
        let is_key = matches!(m, Media::Video { key: true, .. });
        if self.need_key && matches!(m, Media::Video { key: false, .. }) {
            return Push::Ok; // skip until a clean keyframe
        }
        match self.tx.try_send(m.clone()) {
            Ok(()) => {
                if is_key {
                    self.need_key = false;
                }
                Push::Ok
            }
            Err(TrySendError::Full(_)) => {
                if matches!(m, Media::Video { .. }) && !self.need_key {
                    self.need_key = true;
                    return Push::NeedKeyframe;
                }
                Push::Ok
            }
            Err(TrySendError::Disconnected(_)) => Push::Closed,
        }
    }
}

// ---------------- recorder ----------------

pub struct RecorderInfo {
    pub path: PathBuf,
    pub bytes: u64,
    pub error: Option<String>,
}

/// Writes MKV files; continues in a new "_partN" file if the size changes
/// (rotation). Runs until the channel closes, then finalizes the file.
pub fn spawn_recorder(
    first_path: PathBuf,
    config: VideoConfig,
    audio: (u32, u8),
    rx: Receiver<Media>,
    info: Arc<std::sync::Mutex<RecorderInfo>>,
) {
    std::thread::Builder::new()
        .name("recorder".into())
        .spawn(move || {
            let mut part = 1u32;
            let mut cfg = config;
            let mut path = first_path.clone();
            let mut writer = match MkvWriter::create(&path, &cfg, Some(audio)) {
                Ok(w) => Some(w),
                Err(e) => {
                    info.lock().unwrap().error = Some(e.to_string());
                    None
                }
            };
            while let Ok(m) = rx.recv() {
                let Some(w) = writer.as_mut() else { continue };
                let res = match &m {
                    Media::Config(c) if *c != cfg => {
                        part += 1;
                        cfg = c.clone();
                        path = part_path(&first_path, part);
                        let old = writer.take().unwrap();
                        old.finish().ok();
                        match MkvWriter::create(&path, &cfg, Some(audio)) {
                            Ok(w) => {
                                writer = Some(w);
                                info.lock().unwrap().path = path.clone();
                                Ok(())
                            }
                            Err(e) => Err(e),
                        }
                    }
                    Media::Config(_) => Ok(()),
                    Media::Video { pts, key, data } => w.write_video(*pts, *key, data),
                    Media::Audio { pts, pcm } => w.write_audio(*pts, pcm),
                };
                if let Err(e) = res {
                    crate::diag::log("record", format!("recorder error: {e}"));
                    info.lock().unwrap().error = Some(e.to_string());
                    writer = None;
                } else if let Some(w) = writer.as_ref() {
                    info.lock().unwrap().bytes = w.bytes_written;
                }
            }
            if let Some(w) = writer {
                w.finish().ok();
            }
        })
        .ok();
}

fn part_path(first: &std::path::Path, part: u32) -> PathBuf {
    let stem = first.file_stem().unwrap_or_default().to_string_lossy();
    first.with_file_name(format!("{stem}_part{part}.mkv"))
}
