//! Built-in AI API providers. Each one is usable as a site preset on its own domain
//! (`preset = "openai"`) and as a path prefix of the `ai` gateway preset (`/openai/...`).
//!
//! AI traffic is user content: bodies, query strings and WebSocket frames are never rewritten,
//! except for the few response paths listed in `rewrite_paths` that return URLs a client must
//! fetch through the proxy (batch results, uploaded files, generated media).

use std::collections::BTreeMap;

use crate::config::SiteConfig;

/// Idle read timeout for AI APIs: long non-streaming reasoning calls can take many minutes.
const AI_READ_TIMEOUT: u64 = 3600;

struct P(SiteConfig);

impl P {
    /// A provider whose main API host is `root`, reachable as `/<name>/...` on the gateway.
    fn new(name: &str, root: &str, base_path: &str, description: &str) -> Self {
        let mut p = P(SiteConfig {
            name: Some(name.into()),
            root: (!root.is_empty()).then(|| root.into()),
            allow: Some(Vec::new()),
            prefixes: Some(BTreeMap::new()),
            credential_hosts: Some(Vec::new()),
            passthrough_hosts: Some(Vec::new()),
            rewrite_paths: Some(Vec::new()),
            rewrite_requests: Some(false),
            read_timeout_secs: Some(AI_READ_TIMEOUT),
            base_path: Some(base_path.into()),
            description: Some(description.into()),
            ..Default::default()
        });
        if !root.is_empty() {
            p = p.api(name, root);
        }
        p
    }

    /// Another API host (receives credentials), reachable as `/<prefix>/...`.
    /// `prefix` may contain `{x}`, then `host` must contain `{x}` too.
    fn api(mut self, prefix: &str, host: &str) -> Self {
        let pattern = host.replace("{x}", "*");
        self.0.allow.get_or_insert_default().push(pattern.clone());
        self.0
            .credential_hosts
            .get_or_insert_default()
            .push(pattern);
        self.0
            .prefixes
            .get_or_insert_default()
            .insert(prefix.into(), host.into());
        self
    }

    /// A host serving generated output / presigned files: proxied, never sent credentials,
    /// never rewritten.
    fn output(mut self, pattern: &str) -> Self {
        self.0.allow.get_or_insert_default().push(pattern.into());
        self.0
            .passthrough_hosts
            .get_or_insert_default()
            .push(pattern.into());
        self
    }

    /// Response bodies on matching paths carry URLs that must point back at the proxy.
    fn rewrite(mut self, path_regex: &str) -> Self {
        self.0
            .rewrite_paths
            .get_or_insert_default()
            .push(path_regex.into());
        self
    }

    fn grpc(mut self) -> Self {
        self.0.grpc = Some(true);
        self
    }

    fn done(self) -> SiteConfig {
        self.0
    }
}

/// Names of all built-in providers, in display order.
pub fn names() -> &'static [&'static str] {
    &[
        "openai",
        "anthropic",
        "gemini",
        "vertex",
        "openrouter",
        "azure",
        "bedrock",
        "xai",
        "mistral",
        "deepseek",
        "groq",
        "together",
        "fireworks",
        "cerebras",
        "perplexity",
        "cohere",
        "nvidia",
        "sambanova",
        "hyperbolic",
        "novita",
        "github-models",
        "hf-router",
        "parallel",
        "moonshot",
        "zhipu",
        "dashscope",
        "siliconflow",
        "minimax",
        "elevenlabs",
        "deepgram",
        "assemblyai",
        "cartesia",
        "replicate",
        "fal",
        "stability",
        "ideogram",
        "bfl",
        "runway",
        "luma",
        "voyage",
        "jina",
    ]
}

/// Definition of a built-in provider.
pub fn provider(name: &str) -> Option<SiteConfig> {
    let p = match name {
        // ---- core ----
        "openai" => P::new("openai", "api.openai.com", "/v1", "OpenAI (REST, SSE, Realtime WebSocket, files, batch)")
            // image `url` responses point at Azure blob storage
            .rewrite(r"^/v1/images/")
            .output("oaidalleapiprodscus.blob.core.windows.net"),
        "anthropic" => P::new("anthropic", "api.anthropic.com", "", "Anthropic Claude API (messages, batches, files)")
            // batch objects carry an absolute `results_url`
            .rewrite(r"^/v1/messages/batches(/[^/]+)?$"),
        "gemini" => P::new("gemini", "generativelanguage.googleapis.com", "/v1beta", "Google AI Studio / Gemini API (REST, SSE, Live WebSocket, files)")
            // uploaded files are referenced by absolute `uri`
            .rewrite(r"^/(upload/)?v1(alpha|beta)?/files")
            .grpc(),
        "vertex" => {
            let mut p = P::new("vertex", "aiplatform.googleapis.com", "/v1", "Google Vertex AI (global endpoint; regional: /vertex-<region>/)")
                .api("vertex-{x}", "{x}-aiplatform.googleapis.com")
                .api("google-oauth", "oauth2.googleapis.com")
                .grpc();
            // wildcard deployments map <region>-aiplatform.<domain> to the regional host
            p.0.sub_root = Some("googleapis.com".into());
            p
        }
        "openrouter" => {
            let mut p = P::new("openrouter", "openrouter.ai", "/api/v1", "OpenRouter");
            // as its own site the web UI is rewritten; the API never is
            p.0.rewrite_paths = None;
            p.0.passthrough_paths = Some(vec![r"^/api/".into()]);
            p
        }
        // ---- clouds (per-resource / per-region hosts) ----
        "azure" => P::new("azure", "", "/openai", "Azure OpenAI / AI Foundry: /azure-<resource>/, /azure-cog-<resource>/, /azure-ai-<resource>/")
            .api("azure-{x}", "{x}.openai.azure.com")
            .api("azure-cog-{x}", "{x}.cognitiveservices.azure.com")
            .api("azure-ai-{x}", "{x}.services.ai.azure.com"),
        "bedrock" => P::new("bedrock", "", "", "Amazon Bedrock with API keys (bearer): /bedrock-<region>/, /bedrock-mantle-<region>/openai/v1")
            .api("bedrock-{x}", "bedrock-runtime.{x}.amazonaws.com")
            .api("bedrock-mantle-{x}", "bedrock-mantle.{x}.api.aws"),
        // ---- OpenAI-compatible and other LLM APIs ----
        "xai" => P::new("xai", "api.x.ai", "/v1", "xAI Grok"),
        "mistral" => P::new("mistral", "api.mistral.ai", "/v1", "Mistral AI"),
        "deepseek" => P::new("deepseek", "api.deepseek.com", "/v1", "DeepSeek (also Anthropic-compatible at /anthropic)"),
        "groq" => P::new("groq", "api.groq.com", "/openai/v1", "Groq"),
        "together" => P::new("together", "api.together.xyz", "/v1", "Together AI")
            .api("together-ai", "api.together.ai"),
        "fireworks" => P::new("fireworks", "api.fireworks.ai", "/inference/v1", "Fireworks AI"),
        "cerebras" => P::new("cerebras", "api.cerebras.ai", "/v1", "Cerebras"),
        "perplexity" => P::new("perplexity", "api.perplexity.ai", "", "Perplexity Sonar"),
        "cohere" => P::new("cohere", "api.cohere.com", "/v2", "Cohere")
            .api("cohere-ai", "api.cohere.ai"),
        "nvidia" => P::new("nvidia", "integrate.api.nvidia.com", "/v1", "NVIDIA NIM API catalog"),
        "sambanova" => P::new("sambanova", "api.sambanova.ai", "/v1", "SambaNova Cloud"),
        "hyperbolic" => P::new("hyperbolic", "api.hyperbolic.xyz", "/v1", "Hyperbolic"),
        "novita" => P::new("novita", "api.novita.ai", "/openai", "Novita AI"),
        "github-models" => P::new("github-models", "models.github.ai", "/inference", "GitHub Models"),
        "hf-router" => P::new("hf-router", "router.huggingface.co", "/v1", "Hugging Face Inference Providers"),
        "parallel" => P::new("parallel", "api.parallel.ai", "/v1", "Parallel web search / tasks"),
        // ---- China ----
        "moonshot" => P::new("moonshot", "api.moonshot.cn", "/v1", "Moonshot Kimi (international: /moonshot-intl/)")
            .api("moonshot-intl", "api.moonshot.ai"),
        "zhipu" => P::new("zhipu", "open.bigmodel.cn", "/api/paas/v4", "Zhipu GLM (international Z.ai: /zai/api/paas/v4)")
            .api("zai", "api.z.ai"),
        "dashscope" => P::new("dashscope", "dashscope.aliyuncs.com", "/compatible-mode/v1", "Alibaba DashScope / Qwen (international: /dashscope-intl/)")
            .api("dashscope-intl", "dashscope-intl.aliyuncs.com"),
        "siliconflow" => P::new("siliconflow", "api.siliconflow.cn", "/v1", "SiliconFlow (international: /siliconflow-intl/)")
            .api("siliconflow-intl", "api.siliconflow.com"),
        "minimax" => P::new("minimax", "api.minimaxi.com", "/v1", "MiniMax (international: /minimax-intl/)")
            .api("minimax-intl", "api.minimax.io"),
        // ---- speech ----
        "elevenlabs" => P::new("elevenlabs", "api.elevenlabs.io", "/v1", "ElevenLabs TTS / STT (incl. WebSocket)"),
        "deepgram" => P::new("deepgram", "api.deepgram.com", "/v1", "Deepgram STT / TTS (incl. WebSocket)"),
        "assemblyai" => P::new("assemblyai", "api.assemblyai.com", "/v2", "AssemblyAI (streaming: /assemblyai-rt/)")
            .api("assemblyai-rt", "streaming.assemblyai.com"),
        "cartesia" => P::new("cartesia", "api.cartesia.ai", "", "Cartesia TTS (incl. WebSocket)"),
        // ---- image / video ----
        "replicate" => P::new("replicate", "api.replicate.com", "/v1", "Replicate (output URLs proxied)")
            .rewrite(r"^/v1/(predictions|deployments/.+/predictions|models/.+/predictions)")
            .output("replicate.delivery")
            .output("*.replicate.delivery"),
        "fal" => P::new("fal", "fal.run", "", "fal.ai (sync: /fal/, queue: /fal-queue/; output URLs proxied)")
            .api("fal-queue", "queue.fal.run")
            .rewrite(".")
            .output("fal.media")
            .output("*.fal.media"),
        "stability" => P::new("stability", "api.stability.ai", "/v2beta", "Stability AI"),
        "ideogram" => P::new("ideogram", "api.ideogram.ai", "", "Ideogram (output URLs proxied)")
            .rewrite(".")
            .output("ideogram.ai"),
        "bfl" => P::new("bfl", "api.bfl.ai", "/v1", "Black Forest Labs FLUX (polling and output URLs proxied)")
            .rewrite(r"^/v1/")
            .output("delivery-*.bfl.ai"),
        "runway" => P::new("runway", "api.dev.runwayml.com", "/v1", "Runway"),
        "luma" => P::new("luma", "api.lumalabs.ai", "/dream-machine/v1", "Luma Dream Machine"),
        // ---- embeddings / search ----
        "voyage" => P::new("voyage", "api.voyageai.com", "/v1", "Voyage AI embeddings / rerank"),
        "jina" => P::new("jina", "api.jina.ai", "/v1", "Jina AI (reader: /jina-reader/, search: /jina-search/)")
            .api("jina-reader", "r.jina.ai")
            .api("jina-search", "s.jina.ai"),
        _ => return None,
    };
    Some(p.done())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_named_providers_exist() {
        for n in names() {
            let p = provider(n).unwrap_or_else(|| panic!("provider {n} missing"));
            assert!(
                !p.prefixes.as_ref().unwrap().is_empty(),
                "{n} has no prefix"
            );
            for r in p.rewrite_paths.iter().flatten() {
                regex::Regex::new(r).unwrap();
            }
        }
        assert!(provider("nope").is_none());
    }
}
