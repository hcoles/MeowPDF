use std::{
    io::{BufRead, BufReader},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
    thread,
    time::Duration,
};

use crossbeam_channel::{unbounded, Receiver};
use mupdf::{pdf::PdfDocument, pdf::PdfObject};
use serde::Deserialize;

use crate::RUNNING;

const SUPPORTED_VERSION: u32 = 1;
const RECONNECT_DELAY: Duration = Duration::from_millis(1500);
/* A change of position larger than this is considered a seek */
const SEEK_THRESHOLD: f64 = 1.0;

#[derive(Clone, Debug, Deserialize)]
pub struct Region {
    pub page: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Deserialize)]
pub struct Cue {
    pub start: f64,
    #[serde(default)]
    pub regions: Vec<Region>,
}

#[derive(Debug, Deserialize)]
struct SyncFile {
    version: u32,
    audio: String,
    #[serde(default)]
    cues: Vec<Cue>,
}

pub struct SyncData {
    pub cues: Vec<Cue>,
    pub socket: PathBuf,
}

/* Tells the main loop that the active cue changed or that following should resume */
pub struct SyncUpdate {
    pub cue: usize,
    pub resume: bool,
}

#[derive(Deserialize)]
struct Message {
    position: f64,
    #[serde(default)]
    state: String,
}

const ATTACHMENT_NAME: &str = "sync.json";

/* Finds the stream of the embedded file called `sync.json` in a name tree */
fn find_attachment(node: &PdfObject, depth: usize) -> Option<PdfObject> {
    if depth > 16 {
        return None;
    }

    if let Ok(Some(names)) = node.get_dict("Names") {
        let len = names.len().unwrap_or(0);
        for i in (0..len).step_by(2) {
            let Ok(Some(key)) = names.get_array(i as i32) else {
                continue;
            };
            if key.as_string().ok() != Some(ATTACHMENT_NAME) {
                continue;
            }
            let Ok(Some(spec)) = names.get_array(i as i32 + 1) else {
                continue;
            };
            let spec = spec.resolve().ok().flatten().unwrap_or(spec);
            let files = spec.get_dict("EF").ok().flatten()?;
            return files
                .get_dict("F")
                .ok()
                .flatten()
                .or_else(|| files.get_dict("UF").ok().flatten());
        }
    }

    if let Ok(Some(kids)) = node.get_dict("Kids") {
        for i in 0..kids.len().unwrap_or(0) {
            if let Ok(Some(kid)) = kids.get_array(i as i32) {
                if let Some(found) = find_attachment(&kid, depth + 1) {
                    return Some(found);
                }
            }
        }
    }
    None
}

impl SyncData {
    /* Reads the `sync.json` attachment of the PDF. Returns `Ok(None)` if the PDF has
     * no such attachment or the version is unknown. */
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        let pdf_err = |e: mupdf::Error| format!("Could not read {}: {}", path.display(), e);
        let doc = PdfDocument::open(path.to_str().ok_or("Invalid PDF path")?).map_err(pdf_err)?;
        let root = doc.catalog().map_err(pdf_err)?;
        let tree = root
            .get_dict("Names")
            .and_then(|n| n.map_or(Ok(None), |n| n.get_dict("EmbeddedFiles")))
            .map_err(pdf_err)?;
        let Some(stream) = tree.and_then(|t| find_attachment(&t, 0)) else {
            return Ok(None);
        };
        let content = stream.read_stream().map_err(pdf_err)?;
        let file: SyncFile = serde_json::from_slice(&content)
            .map_err(|e| format!("Could not parse sync.json in {}: {}", path.display(), e))?;

        if file.version != SUPPORTED_VERSION {
            return Ok(None);
        }

        let mut cues: Vec<Cue> = file
            .cues
            .into_iter()
            .filter(|c| !c.regions.is_empty())
            .collect();
        cues.sort_by(|a, b| a.start.total_cmp(&b.start));

        let socket = path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(Path::new(&file.audio).with_extension("sock"));

        Ok(Some(Self { cues, socket }))
    }

    /* The cue active at the position. Positions after the end map to the last cue
     * and positions before the start to the first one. */
    fn cue_at(&self, position: f64) -> Option<usize> {
        if self.cues.is_empty() {
            return None;
        }
        let after = self.cues.partition_point(|c| c.start <= position);
        Some(after.saturating_sub(1))
    }
}

pub fn spawn(data: Arc<SyncData>) -> Receiver<SyncUpdate> {
    let (sender, receiver) = unbounded::<SyncUpdate>();

    thread::spawn(move || {
        while RUNNING.load(Ordering::Acquire) {
            let Ok(stream) = UnixStream::connect(&data.socket) else {
                thread::sleep(RECONNECT_DELAY);
                continue;
            };

            let mut last_cue = None;
            let mut last_state = String::new();
            let mut last_position = None;

            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                let Ok(msg) = serde_json::from_str::<Message>(&line) else {
                    continue;
                };
                let Some(cue) = data.cue_at(msg.position) else {
                    continue;
                };

                let jumped = last_position
                    .is_some_and(|p: f64| (msg.position - p).abs() > SEEK_THRESHOLD);
                let started = msg.state == "playing" && last_state != "playing";
                let resume = jumped || started;

                let changed = last_cue != Some(cue);
                last_position = Some(msg.position);
                last_state = msg.state;
                last_cue = Some(cue);

                if (changed || resume) && sender.send(SyncUpdate { cue, resume }).is_err()
                {
                    return;
                }
            }

            thread::sleep(RECONNECT_DELAY);
        }
    });

    receiver
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(starts: &[f64]) -> SyncData {
        SyncData {
            cues: starts
                .iter()
                .map(|s| Cue {
                    start: *s,
                    regions: Vec::new(),
                })
                .collect(),
            socket: PathBuf::new(),
        }
    }

    #[test]
    fn finds_active_cue() {
        let d = data(&[0.0, 2.0, 5.0]);
        assert_eq!(d.cue_at(0.0), Some(0));
        assert_eq!(d.cue_at(1.99), Some(0));
        assert_eq!(d.cue_at(2.0), Some(1));
        assert_eq!(d.cue_at(1000.0), Some(2));
        assert_eq!(data(&[]).cue_at(1.0), None);
    }
}
