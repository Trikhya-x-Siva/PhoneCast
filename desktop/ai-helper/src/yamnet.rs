//! YAMNet sound classifier through ONNX Runtime (CPU, 2 threads). Groups
//! the 521 AudioSet classes into a few labels the highlight scorer uses.

use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;
use std::path::Path;

/// (label, class indices in yamnet_class_map.csv)
const GROUPS: &[(&str, &[usize])] = &[
    ("gunfire", &[420, 421]),    // Explosion, Gunshot/gunfire
    ("cheer", &[8, 61, 62, 64]), // Whoop, Cheering, Applause, Crowd
    ("laugh", &[13, 14, 15, 16, 17, 18]),
    ("scream", &[6, 9, 11]), // Shout, Yell, Screaming
];

pub struct Classifier {
    session: Session,
}

impl Classifier {
    pub fn load(path: &Path) -> Result<Self, String> {
        let session = Session::builder()
            .and_then(|b| b.with_optimization_level(GraphOptimizationLevel::Level3))
            .and_then(|b| b.with_intra_threads(2))
            .and_then(|b| b.commit_from_file(path))
            .map_err(|e| e.to_string())?;
        Ok(Self { session })
    }

    /// `samples`: 16 kHz mono in -1..1, about one second. Returns the best
    /// grouped label and its score (max over frames).
    pub fn classify(&mut self, samples: &[f32]) -> Result<Option<(String, f64)>, String> {
        if samples.len() < 8000 {
            return Ok(None);
        }
        let input =
            Tensor::from_array(([samples.len()], samples.to_vec())).map_err(|e| e.to_string())?;
        let outputs = self
            .session
            .run(ort::inputs!["waveform" => input])
            .map_err(|e| e.to_string())?;
        let (shape, data) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;
        let classes = *shape.last().unwrap_or(&521) as usize;
        if classes == 0 {
            return Ok(None);
        }
        Ok(best_group(data, classes))
    }
}

/// Max score per group over all frames; the strongest group wins if it is
/// at least 0.2 confident.
pub fn best_group(scores: &[f32], classes: usize) -> Option<(String, f64)> {
    let mut best: Option<(String, f64)> = None;
    for frame in scores.chunks(classes) {
        for (label, idx) in GROUPS {
            let s = idx
                .iter()
                .filter_map(|i| frame.get(*i))
                .fold(0f32, |m, x| m.max(*x)) as f64;
            if s >= 0.2 && best.as_ref().is_none_or(|b| s > b.1) {
                best = Some((label.to_string(), s));
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    #[test]
    fn best_group_picks_strongest() {
        let mut frame = vec![0f32; 521];
        frame[421] = 0.6;
        frame[61] = 0.3;
        let two = [frame.clone(), frame].concat();
        assert_eq!(
            super::best_group(&two, 521),
            Some(("gunfire".to_string(), 0.6f32 as f64))
        );
        assert_eq!(super::best_group(&vec![0.1f32; 521], 521), None);
    }
}
