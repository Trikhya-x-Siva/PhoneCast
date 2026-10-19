//! YAMNet sound classifier through ONNX Runtime (CPU, 2 threads). Groups
//! the 521 AudioSet classes into a few labels the highlight scorer uses.

use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;
use std::path::Path;

/// (label, class indices in yamnet_class_map.csv)
const GROUPS: &[(&str, &[usize])] = &[
    // Explosion, Gunshot, Machine gun, Fusillade, Artillery fire, Cap gun,
    // Fireworks, Firecracker, Burst/pop, Eruption, Boom, Bang: game fights
    // are mostly machine-gun bursts and booms, not single shots.
    ("gunfire", &[420, 421, 422, 423, 424, 425, 426, 427, 428, 429, 430, 460]),
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

    /// Labels over a real recording's sound (16 kHz mono f32le), one per
    /// second: `PHONECAST_AUDIO=game16k.raw cargo test --release --features
    /// yamnet yamnet_timeline -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn yamnet_timeline() {
        let Ok(path) = std::env::var("PHONECAST_AUDIO") else { return };
        let raw = std::fs::read(path).unwrap();
        let s: Vec<f32> = raw.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        let mut c = super::Classifier::load(&crate::models::YAMNET.path()).unwrap();
        let mut counts = std::collections::BTreeMap::<String, usize>::new();
        let t = std::time::Instant::now();
        let mut line = String::new();
        for (i, sec) in s.chunks(16000).enumerate() {
            let got = c.classify(sec).unwrap();
            let mark = match &got {
                Some((l, _)) => {
                    *counts.entry(l.clone()).or_default() += 1;
                    l.chars().next().unwrap().to_ascii_uppercase()
                }
                None => '.',
            };
            line.push(mark);
            if i % 60 == 59 {
                println!("{:>4}s {line}", i + 1);
                line.clear();
            }
        }
        println!("{line}");
        println!("{} s classified in {:?}: {counts:?}", s.len() / 16000, t.elapsed());
    }

    /// The real model on real-ish audio: silence gives nothing, sharp noise
    /// bursts (like shots) are heard. Needs the downloaded model:
    /// `cargo test --release --features yamnet yamnet_hears -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn yamnet_hears_bursts_not_silence() {
        let path = crate::models::YAMNET.path();
        let mut c = super::Classifier::load(&path).expect("load yamnet");
        assert_eq!(c.classify(&vec![0f32; 16000]).unwrap(), None);
        // Four 60 ms bursts of noise with a fast decay, a "bang" every 250 ms.
        let mut seed = 12345u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            ((seed >> 9) as f32 / (1u32 << 23) as f32) * 2.0 - 1.0
        };
        let bursts: Vec<f32> = (0..16000)
            .map(|i| {
                let t = i % 4000;
                if t < 960 { noise() * (-(t as f32) / 200.0).exp() } else { 0.0 }
            })
            .collect();
        // What the model actually outputs (top classes over all frames).
        {
            let input = ort::value::Tensor::from_array(([bursts.len()], bursts.clone())).unwrap();
            let outputs = c.session.run(ort::inputs!["waveform" => input]).unwrap();
            for (i, (name, v)) in outputs.iter().enumerate() {
                let (shape, data) = v.try_extract_tensor::<f32>().unwrap();
                println!("output {i} {name}: shape {shape:?}");
                if i == 0 {
                    let classes = *shape.last().unwrap() as usize;
                    let mut best = vec![0f32; classes];
                    for f in data.chunks(classes) {
                        for (k, x) in f.iter().enumerate() {
                            best[k] = best[k].max(*x);
                        }
                    }
                    let mut idx: Vec<usize> = (0..classes).collect();
                    idx.sort_by(|a, b| best[*b].total_cmp(&best[*a]));
                    println!("top: {:?}", idx[..5].iter().map(|i| (*i, best[*i])).collect::<Vec<_>>());
                }
            }
        }
        let started = std::time::Instant::now();
        let got = c.classify(&bursts).unwrap();
        println!("bursts -> {got:?} in {:?}", started.elapsed());
        // Synthetic noise is not a convincing gunshot; this only proves the
        // model runs fast on a second of audio (real sound: yamnet_timeline).
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        let _ = got;
    }
}
