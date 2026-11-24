//! The concrete local constrained-decode backend (feature = "llm-llama").
//!
//! Deployment/ops artifact: `llama-cpp-sys` compiles llama.cpp via bindgen (the
//! flake devshell carries libclang + cmake), and a quantized GGUF model FILE is a
//! RUNTIME artifact, not a repo/CI dependency. [`LlamaBackend`] loads the model
//! lazily on the first `generate`, then decodes GREEDILY under the GBNF grammar
//! sampler, so every emitted token keeps the output inside the grammar: the text
//! can only ever be a plan object [`crate::llm::parse_llm_output`] accepts. The
//! decode is bounded twice - a token cap and a wall-clock deadline - and any
//! failure is an `Err`, never a fabricated plan, so the hybrid degrades to the
//! rules answer (NotUnderstood) rather than to a guess.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend as Runtime;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;

use crate::llm::LlmBackend;

/// Max tokens one plan may take. The longest grammar sentence (a move with a
/// range selector) is well under this; the cap only stops a pathological decode.
const MAX_TOKENS: usize = 96;
/// Wall-clock ceiling for one translation, prompt included. A 0.5B Q4 model on 4
/// threads answers in about a second; past this the request is refused.
const DEADLINE: Duration = Duration::from_secs(20);
/// CPU threads for decode and prompt eval. Hard cap of the machine policy.
const THREADS: i32 = 4;
/// Context window: the instruction prompt plus the answer, with headroom.
const N_CTX: u32 = 2048;

/// The llama.cpp runtime is a process-wide singleton (a second `init` errors), so
/// every backend shares one.
fn runtime() -> Result<&'static Runtime, String> {
    static RT: OnceLock<Result<Runtime, String>> = OnceLock::new();
    RT.get_or_init(|| {
        let mut rt = Runtime::init().map_err(|e| e.to_string())?;
        rt.void_logs();
        Ok(rt)
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// A llama.cpp-backed constrained-decode backend. Holds the model path; the model
/// is loaded lazily on first `generate` and kept for the process lifetime.
pub struct LlamaBackend {
    model_path: PathBuf,
    model: OnceLock<Result<LlamaModel, String>>,
    /// One decode at a time: concurrent asks queue rather than multiply the
    /// context memory and the thread count.
    busy: Mutex<()>,
}

impl LlamaBackend {
    /// Construct from a GGUF model path without touching the file.
    pub fn new(model_path: PathBuf) -> Self {
        Self { model_path, model: OnceLock::new(), busy: Mutex::new(()) }
    }

    /// `Some` only when the model file exists, so a caller wires the model path
    /// only when it can work and otherwise stays rules-only.
    pub fn open(model_path: impl AsRef<Path>) -> Option<Self> {
        let p = model_path.as_ref();
        p.is_file().then(|| Self::new(p.to_path_buf()))
    }

    fn model(&self) -> Result<&LlamaModel, String> {
        self.model
            .get_or_init(|| {
                let rt = runtime()?;
                LlamaModel::load_from_file(rt, &self.model_path, &LlamaModelParams::default())
                    .map_err(|e| format!("load {}: {e}", self.model_path.display()))
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Wrap the instruction in the model's own chat template (falls back to the
    /// raw text when the GGUF carries none).
    fn templated(model: &LlamaModel, prompt: &str) -> String {
        let msg = LlamaChatMessage::new("user".into(), prompt.into());
        match (model.chat_template(None), msg) {
            (Ok(t), Ok(m)) => model.apply_chat_template(&t, &[m], true).unwrap_or_else(|_| prompt.into()),
            _ => prompt.into(),
        }
    }
}

impl LlmBackend for LlamaBackend {
    fn generate(&self, prompt: &str, gbnf: &str) -> Result<String, String> {
        let _turn = self.busy.lock().map_err(|_| "llama backend poisoned".to_string())?;
        let started = Instant::now();
        let rt = runtime()?;
        let model = self.model()?;

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(N_CTX))
            .with_n_batch(N_CTX)
            .with_n_threads(THREADS)
            .with_n_threads_batch(THREADS);
        let mut ctx = model.new_context(rt, ctx_params).map_err(|e| e.to_string())?;

        let text = Self::templated(model, prompt);
        let tokens = model.str_to_token(&text, AddBos::Always).map_err(|e| e.to_string())?;
        if tokens.len() + MAX_TOKENS > N_CTX as usize {
            return Err(format!("prompt too long: {} tokens", tokens.len()));
        }

        let mut batch = LlamaBatch::new(N_CTX as usize, 1);
        let last = tokens.len() as i32 - 1;
        for (i, t) in (0_i32..).zip(tokens.iter().copied()) {
            batch.add(t, i, &[0], i == last).map_err(|e| e.to_string())?;
        }
        ctx.decode(&mut batch).map_err(|e| e.to_string())?;

        // Grammar first, then greedy: the grammar masks every token that would
        // leave the plan language, greedy picks the most likely survivor. No
        // temperature and no seed, so the same ask decodes the same plan.
        let grammar = LlamaSampler::grammar(model, gbnf, "root").map_err(|e| e.to_string())?;
        let mut sampler = LlamaSampler::chain_simple([grammar, LlamaSampler::greedy()]);

        let mut out = Vec::new();
        let mut pos = batch.n_tokens();
        for _ in 0..MAX_TOKENS {
            if started.elapsed() > DEADLINE {
                return Err(format!("decode exceeded {DEADLINE:?}"));
            }
            // `sample` also advances the grammar state with the chosen token.
            let tok = sampler.sample(&ctx, batch.n_tokens() - 1);
            if model.is_eog_token(tok) {
                // The grammar only admits end-of-generation once the root rule is
                // complete, so this is a whole plan.
                return String::from_utf8(out).map_err(|e| e.to_string());
            }
            out.extend(model.token_to_piece_bytes(tok, 64, false, None).map_err(|e| e.to_string())?);
            batch.clear();
            batch.add(tok, pos, &[0], true).map_err(|e| e.to_string())?;
            pos += 1;
            ctx.decode(&mut batch).map_err(|e| e.to_string())?;
        }
        Err(format!("no complete plan within {MAX_TOKENS} tokens"))
    }
}
