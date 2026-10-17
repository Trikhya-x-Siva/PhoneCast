//! Optional rephrasing of FAQ answers with a small local model
//! (Qwen3-1.7B, 4-bit) through llama.cpp on the CPU, at most 4 threads.
//! The facts always come from the streamer's YAML; the model only rewords.

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel, Special};
use llama_cpp_2::sampling::LlamaSampler;
use std::num::NonZeroU32;
use std::path::Path;

pub const MAX_THREADS: i32 = 4;

pub struct Rephraser {
    backend: LlamaBackend,
    model: LlamaModel,
}

impl Rephraser {
    pub fn load(path: &Path) -> Result<Self, String> {
        let backend = LlamaBackend::init().map_err(|e| e.to_string())?;
        let params = LlamaModelParams::default();
        let model =
            LlamaModel::load_from_file(&backend, path, &params).map_err(|e| e.to_string())?;
        Ok(Self { backend, model })
    }

    /// Rewrites `answer` as a short, friendly chat reply to `question`.
    /// Falls back to the original answer on any problem.
    pub fn rephrase(&self, question: &str, answer: &str) -> String {
        match self.generate(question, answer) {
            Ok(s) if !s.trim().is_empty() && s.chars().count() <= 400 => s.trim().to_string(),
            _ => answer.to_string(),
        }
    }

    fn generate(&self, question: &str, answer: &str) -> Result<String, String> {
        let prompt = format!(
            "<|im_start|>system\nYou help a game streamer answer chat. Rewrite the given answer as one \
             friendly chat message (max 2 sentences). Keep every fact exactly; add nothing new. \
             Reply with the message only. /no_think<|im_end|>\n\
             <|im_start|>user\nViewer asked: {question}\nAnswer to reword: {answer}<|im_end|>\n\
             <|im_start|>assistant\n<think>\n\n</think>\n\n"
        );
        let threads = (std::thread::available_parallelism()
            .map(|n| n.get() as i32)
            .unwrap_or(2)
            / 2)
        .clamp(1, MAX_THREADS);
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(1024))
            .with_n_threads(threads)
            .with_n_threads_batch(threads);
        let mut ctx = self
            .model
            .new_context(&self.backend, ctx_params)
            .map_err(|e| e.to_string())?;
        let tokens = self
            .model
            .str_to_token(&prompt, AddBos::Never)
            .map_err(|e| e.to_string())?;
        let mut batch = LlamaBatch::new(1024, 1);
        let last = tokens.len() - 1;
        for (i, t) in tokens.iter().enumerate() {
            batch
                .add(*t, i as i32, &[0], i == last)
                .map_err(|e| e.to_string())?;
        }
        ctx.decode(&mut batch).map_err(|e| e.to_string())?;
        let mut sampler =
            LlamaSampler::chain_simple([LlamaSampler::temp(0.6), LlamaSampler::dist(42)]);
        let mut out = String::new();
        let mut pos = tokens.len() as i32;
        for _ in 0..120 {
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            sampler.accept(token);
            if self.model.is_eog_token(token) {
                break;
            }
            let piece = self
                .model
                .token_to_str(token, Special::Tokenize)
                .map_err(|e| e.to_string())?;
            out.push_str(&piece);
            batch.clear();
            batch
                .add(token, pos, &[0], true)
                .map_err(|e| e.to_string())?;
            pos += 1;
            ctx.decode(&mut batch).map_err(|e| e.to_string())?;
        }
        Ok(out)
    }
}
