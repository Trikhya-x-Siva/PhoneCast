//! Optional model downloads. Nothing is fetched until the streamer switches
//! the feature on; every file is checked against a pinned SHA-256 before
//! it is used.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    pub id: &'static str,
    pub name: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
    pub licence: &'static str,
}

/// YAMNet audio event classifier, ONNX export of Google's model
/// (Apache-2.0). Input: 16 kHz mono waveform; output: 521 AudioSet scores.
pub const YAMNET: ModelSpec = ModelSpec {
    id: "yamnet",
    name: "YAMNet sound classifier",
    file: "yamnet.onnx",
    url: "https://huggingface.co/audiomagic/yamnet-onnx/resolve/main/yamnet.onnx",
    sha256: "d3835ffbbd4a1bb3e777f0ca217b5007907f5171dd5d17c4236b95b2af8f908e",
    size: 16_093_355,
    licence: "Apache-2.0 (Google)",
};

/// Qwen3-1.7B, 4-bit GGUF (Apache-2.0), for rephrasing FAQ answers.
pub const LLM: ModelSpec = ModelSpec {
    id: "llm",
    name: "Qwen3-1.7B (4-bit) rephraser",
    file: "Qwen3-1.7B-Q4_K_M.gguf",
    url: "https://huggingface.co/unsloth/Qwen3-1.7B-GGUF/resolve/main/Qwen3-1.7B-Q4_K_M.gguf",
    sha256: "b139949c5bd74937ad8ed8c8cf3d9ffb1e99c866c823204dc42c0d91fa181897",
    size: 1_107_409_472,
    licence: "Apache-2.0 (Alibaba Qwen)",
};

pub const ALL: &[ModelSpec] = &[YAMNET, LLM];

pub fn by_id(id: &str) -> Option<ModelSpec> {
    ALL.iter().copied().find(|m| m.id == id)
}

/// `%APPDATA%\PhoneCast\ai\models`
pub fn models_dir() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("PhoneCast").join("ai").join("models")
}

impl ModelSpec {
    pub fn path(&self) -> PathBuf {
        models_dir().join(self.file)
    }

    pub fn size_mb(&self) -> f64 {
        (self.size as f64 / 1_048_576.0 * 10.0).round() / 10.0
    }

    pub fn is_ready(&self) -> bool {
        std::fs::metadata(self.path()).is_ok_and(|m| m.len() == self.size)
    }

    pub fn remove(&self) {
        std::fs::remove_file(self.path()).ok();
        std::fs::remove_file(self.path().with_extension("part")).ok();
    }

    /// Downloads to a `.part` file, verifies the SHA-256, then renames.
    /// `progress` is called with 0..1.
    pub fn download(&self, progress: &mut dyn FnMut(f64)) -> Result<PathBuf, String> {
        let dest = self.path();
        if let Some(d) = dest.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let part = dest.with_extension("part");
        let resp = ureq::get(self.url)
            .timeout(std::time::Duration::from_secs(30 * 60))
            .call()
            .map_err(|e| format!("download failed: {e}"))?;
        let mut reader = resp.into_reader();
        let mut file = std::fs::File::create(&part).map_err(|e| e.to_string())?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut done = 0u64;
        loop {
            let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            hasher.update(&buf[..n]);
            done += n as u64;
            progress((done as f64 / self.size as f64).min(0.999));
            if done > self.size + 1024 {
                drop(file);
                std::fs::remove_file(&part).ok();
                return Err("download is larger than expected; stopped".into());
            }
        }
        drop(file);
        let got = hex(&hasher.finalize());
        if got != self.sha256 {
            std::fs::remove_file(&part).ok();
            return Err(format!(
                "checksum mismatch (expected {}…, got {}…); the file was deleted",
                &self.sha256[..12],
                &got[..12]
            ));
        }
        std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
        progress(1.0);
        Ok(dest)
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of a file on disk, for a re-check after download.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_of_known_text() {
        let dir = std::env::temp_dir().join(format!("phonecast-ai-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("abc.txt");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn specs_are_well_formed() {
        for m in ALL {
            assert_eq!(m.sha256.len(), 64);
            assert!(m.url.starts_with("https://"));
            assert!(m.size > 0);
            assert!(by_id(m.id).is_some());
        }
        assert!(by_id("nope").is_none());
        assert!(YAMNET.size_mb() > 15.0 && YAMNET.size_mb() < 16.0);
    }
}
