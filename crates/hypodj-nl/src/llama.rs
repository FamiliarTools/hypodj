//! The concrete local constrained-decode backend (feature = "llm-llama").
//!
//! Deployment/ops artifact: `llama-cpp-sys` compiles llama.cpp via bindgen (the
//! flake devshell carries libclang + cmake), and a quantized GGUF model FILE is a
//! RUNTIME artifact, not a repo/CI dependency. The default model is
//! Qwen2.5-1.5B-Instruct Q4_K_M (Apache-2.0, ~1.1 GB):
//! <https://huggingface.co/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main/qwen2.5-1.5b-instruct-q4_k_m.gguf>.
//!
//! [`LlamaBackend`] loads the model lazily, keeps ONE context for the process
//! lifetime, and reuses its KV cache across asks: every prompt starts with the
//! same instructions and worked examples, so only the tokens after the longest
//! prefix shared with the previous call are evaluated. That prefix is most of
//! the prompt, and evaluating it was most of the latency. It then decodes
//! GREEDILY under the GBNF grammar sampler, so every emitted token keeps the
//! output inside the grammar: the text can only ever be a plan object
//! [`crate::llm::parse_llm_output`] accepts. The decode is bounded twice - a token
//! cap and a wall-clock deadline - and any failure is an `Err`, never a
//! fabricated plan, so the hybrid degrades to the rules answer (NotUnderstood)
//! rather than to a guess.

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::llama_backend::LlamaBackend as Runtime;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;

use crate::llm::{ChatTurn, LlmBackend};

/// Max tokens one plan may take. The longest grammar sentence (a move with a
/// range selector) is well under this; the cap only stops a pathological decode.
const MAX_TOKENS: usize = 96;
/// Wall-clock ceiling for one translation, prompt included. With the prefix warm
/// a 1.5B Q4 model on 4 threads answers in about a second; a cold first ask
/// (prefix not yet evaluated) takes several.
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

/// The one live context and the tokens whose KV sits in its sequence 0, in
/// position order. Invariant: `cached` is exactly what the cache holds; any
/// failure mid-update clears both rather than leave them disagreeing.
struct Warm {
    ctx: LlamaContext<'static>,
    cached: Vec<LlamaToken>,
}

// SAFETY: a llama_context may move between threads; it must not be used by two at
// once, and `Warm` is only ever reached through `LlamaBackend::warm`'s Mutex.
unsafe impl Send for Warm {}

/// A llama.cpp-backed constrained-decode backend. Holds the model path; the model
/// is loaded lazily on first use and kept for the process lifetime.
pub struct LlamaBackend {
    model_path: PathBuf,
    /// Leaked on load: the context borrows the model for its whole life and both
    /// live as long as the process (one backend per daemon), so `'static` is the
    /// honest lifetime rather than a self-referential struct.
    model: OnceLock<Result<&'static LlamaModel, String>>,
    /// One decode at a time over the one cached context: concurrent asks queue
    /// rather than multiply the context memory and the thread count.
    warm: Mutex<Option<Warm>>,
}

impl LlamaBackend {
    /// Construct from a GGUF model path without touching the file.
    pub fn new(model_path: PathBuf) -> Self {
        Self { model_path, model: OnceLock::new(), warm: Mutex::new(None) }
    }

    /// `Some` only when the model file exists, so a caller wires the model path
    /// only when it can work and otherwise stays rules-only.
    pub fn open(model_path: impl AsRef<Path>) -> Option<Self> {
        let p = model_path.as_ref();
        p.is_file().then(|| Self::new(p.to_path_buf()))
    }

    fn model(&self) -> Result<&'static LlamaModel, String> {
        self.model
            .get_or_init(|| {
                let rt = runtime()?;
                LlamaModel::load_from_file(rt, &self.model_path, &LlamaModelParams::default())
                    .map(|m| &*Box::leak(Box::new(m)))
                    .map_err(|e| format!("load {}: {e}", self.model_path.display()))
            })
            .clone()
    }

    /// Wrap the conversation in the model's own chat template (falls back to a
    /// plain transcript when the GGUF carries none).
    fn templated(model: &LlamaModel, chat: &[ChatTurn]) -> String {
        let plain = || {
            let mut s: String = chat.iter().map(|t| format!("{}: {}\n", t.role, t.content)).collect();
            s.push_str("assistant: ");
            s
        };
        let msgs: Result<Vec<_>, _> = chat
            .iter()
            .map(|t| LlamaChatMessage::new(t.role.into(), t.content.clone()))
            .collect();
        match (model.chat_template(None), msgs) {
            (Ok(tmpl), Ok(m)) => model.apply_chat_template(&tmpl, &m, true).unwrap_or_else(|_| plain()),
            _ => plain(),
        }
    }

    /// Run `f` on the warm context, creating it on first use. A failure inside
    /// `f` drops the cache so the next call starts from a known-empty state.
    fn with_warm<T>(&self, f: impl FnOnce(&'static LlamaModel, &mut Warm) -> Result<T, String>) -> Result<T, String> {
        let mut slot = self.warm.lock().map_err(|_| "llama backend poisoned".to_string())?;
        let model = self.model()?;
        if slot.is_none() {
            let ctx_params = LlamaContextParams::default()
                .with_n_ctx(NonZeroU32::new(N_CTX))
                .with_n_batch(N_CTX)
                .with_n_threads(THREADS)
                .with_n_threads_batch(THREADS);
            let ctx = model.new_context(runtime()?, ctx_params).map_err(|e| e.to_string())?;
            *slot = Some(Warm { ctx, cached: Vec::new() });
        }
        let warm = slot.as_mut().expect("just filled");
        let r = f(model, warm);
        if r.is_err() {
            warm.ctx.clear_kv_cache();
            warm.cached.clear();
        }
        r
    }

    /// Bring the cache to exactly `tokens`, evaluating only what follows the
    /// prefix already cached, and return the batch index holding the logits of
    /// the last token. At least the last token is always (re)evaluated, because
    /// its logits are what the first sample reads.
    fn prefill(warm: &mut Warm, tokens: &[LlamaToken], batch: &mut LlamaBatch) -> Result<i32, String> {
        let shared = warm.cached.iter().zip(tokens).take_while(|(a, b)| a == b).count();
        let mut keep = shared.min(tokens.len().saturating_sub(1));
        let trimmed = warm.ctx
            .clear_kv_cache_seq(Some(0), Some(keep as u32), None)
            .map_err(|e| e.to_string())?;
        if !trimmed {
            // A cache that cannot drop a tail (not the case for a transformer)
            // starts over instead of decoding on top of stale positions.
            warm.ctx.clear_kv_cache();
            keep = 0;
        }
        warm.cached.truncate(keep);
        batch.clear();
        let last = tokens.len() - 1;
        for (i, t) in tokens.iter().copied().enumerate().skip(keep) {
            batch.add(t, i as i32, &[0], i == last).map_err(|e| e.to_string())?;
        }
        warm.ctx.decode(batch).map_err(|e| e.to_string())?;
        warm.cached.extend_from_slice(&tokens[keep..]);
        Ok(batch.n_tokens() - 1)
    }

    fn tokens(model: &LlamaModel, chat: &[ChatTurn]) -> Result<Vec<LlamaToken>, String> {
        let text = Self::templated(model, chat);
        let tokens = model.str_to_token(&text, AddBos::Always).map_err(|e| e.to_string())?;
        if tokens.is_empty() || tokens.len() + MAX_TOKENS > N_CTX as usize {
            return Err(format!("prompt length {} tokens out of range", tokens.len()));
        }
        Ok(tokens)
    }
}

impl LlmBackend for LlamaBackend {
    fn prefill(&self, chat: &[ChatTurn]) -> Result<(), String> {
        self.with_warm(|model, warm| {
            let tokens = Self::tokens(model, chat)?;
            let mut batch = LlamaBatch::new(N_CTX as usize, 1);
            Self::prefill(warm, &tokens, &mut batch).map(|_| ())
        })
    }

    fn generate(&self, chat: &[ChatTurn], gbnf: &str) -> Result<String, String> {
        let started = Instant::now();
        self.with_warm(|model, warm| {
            let tokens = Self::tokens(model, chat)?;
            let mut batch = LlamaBatch::new(N_CTX as usize, 1);
            let mut logits_at = Self::prefill(warm, &tokens, &mut batch)?;

            // Grammar first, then greedy: the grammar masks every token that would
            // leave the plan language, greedy picks the most likely survivor. No
            // temperature and no seed, so the same ask decodes the same plan.
            let grammar = LlamaSampler::grammar(model, gbnf, "root").map_err(|e| e.to_string())?;
            let mut sampler = LlamaSampler::chain_simple([grammar, LlamaSampler::greedy()]);

            let mut out = Vec::new();
            for _ in 0..MAX_TOKENS {
                if started.elapsed() > DEADLINE {
                    return Err(format!("decode exceeded {DEADLINE:?}"));
                }
                // `sample` also advances the grammar state with the chosen token.
                let tok = sampler.sample(&warm.ctx, logits_at);
                if model.is_eog_token(tok) {
                    // The grammar only admits end-of-generation once the root rule
                    // is complete, so this is a whole plan.
                    return String::from_utf8(out).map_err(|e| e.to_string());
                }
                out.extend(model.token_to_piece_bytes(tok, 64, false, None).map_err(|e| e.to_string())?);
                batch.clear();
                batch.add(tok, warm.cached.len() as i32, &[0], true).map_err(|e| e.to_string())?;
                warm.ctx.decode(&mut batch).map_err(|e| e.to_string())?;
                warm.cached.push(tok);
                logits_at = 0;
            }
            Err(format!("no complete plan within {MAX_TOKENS} tokens"))
        })
    }
}
