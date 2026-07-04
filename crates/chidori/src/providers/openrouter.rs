//! OpenRouter provider + OAuth (PKCE) login — the zero-config fallback.
//!
//! Chidori's default is provider env vars (`ANTHROPIC_API_KEY`,
//! `OPENAI_API_KEY`, …). But the demonstration surfaces (`chidori demo`,
//! `chidori chat`, an interactive `chidori run`) should work with *nothing*
//! configured, so a first-time user can try everything out. When no provider
//! key is present we fall back to OpenRouter: the user logs in once in their
//! browser via OpenRouter's PKCE OAuth flow
//! (<https://openrouter.ai/docs/guides/overview/auth/oauth>), we exchange the
//! returned code for a user-scoped API key, and persist it under
//! `~/.chidori/credentials.json` so subsequent runs need no login.
//!
//! OpenRouter's chat API is OpenAI-compatible, so the provider is a thin
//! wrapper over [`OpenAiProvider`] pointed at OpenRouter's base URL. The only
//! extra work is translating Chidori's model ids (`claude-sonnet-4-6`) into
//! OpenRouter slugs (`anthropic/claude-sonnet-4.6`) on the way out.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use super::openai::OpenAiProvider;
use super::{LlmProvider, LlmRequest, LlmResponse, TokenSink};

/// OpenRouter's OpenAI-compatible chat endpoint.
const OPENROUTER_CHAT_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
/// Browser authorization endpoint for the PKCE flow.
const OPENROUTER_AUTH_URL: &str = "https://openrouter.ai/auth";
/// Token-exchange endpoint: trades the auth `code` (+ verifier) for an API key.
const OPENROUTER_KEYS_URL: &str = "https://openrouter.ai/api/v1/auth/keys";

/// Env var carrying an OpenRouter key directly (mirrors the other providers).
pub const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";

/// An OpenRouter-backed LLM provider. Acts as a catch-all (`supports_model`
/// always true), so it slots in behind any explicit provider as the fallback.
pub struct OpenRouterProvider {
    inner: OpenAiProvider,
}

impl OpenRouterProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            // A single empty prefix makes the inner OpenAI provider match every
            // model; we own routing via `supports_model` below.
            inner: OpenAiProvider::with_base_url(
                api_key,
                OPENROUTER_CHAT_URL.to_string(),
                vec![String::new()],
            ),
        }
    }

    pub fn with_rate_limit(mut self, rpm: u32) -> Self {
        self.inner = self.inner.with_rate_limit(rpm);
        self
    }
}

#[async_trait::async_trait]
impl LlmProvider for OpenRouterProvider {
    fn supports_model(&self, _model: &str) -> bool {
        true
    }

    async fn send(&self, request: &LlmRequest) -> Result<LlmResponse> {
        let mut req = request.clone();
        req.model = to_openrouter_slug(&request.model);
        self.inner.send(&req).await
    }

    async fn stream(&self, request: &LlmRequest, on_delta: &mut TokenSink) -> Result<LlmResponse> {
        let mut req = request.clone();
        req.model = to_openrouter_slug(&request.model);
        self.inner.stream(&req, on_delta).await
    }
}

/// Translate a Chidori model id into an OpenRouter slug.
///
/// - Anything already containing `/` is assumed to be an OpenRouter slug and
///   passes through untouched.
/// - Claude ids are canonicalized via the Anthropic alias table, then the
///   trailing `-<major>-<minor>` version is rewritten with a dot to match
///   OpenRouter (`claude-sonnet-4-6` → `anthropic/claude-sonnet-4.6`).
/// - OpenAI ids (`gpt*`, `o1*`, `o3*`, `o4*`) are prefixed with `openai/`.
/// - Anything else passes through so an explicit slug always wins.
pub fn to_openrouter_slug(model: &str) -> String {
    if model.contains('/') {
        return model.to_string();
    }
    let canonical = super::anthropic::resolve_alias(model);
    let lower = canonical.to_ascii_lowercase();
    if lower.starts_with("claude") {
        return format!("anthropic/{}", hyphen_version_to_dot(canonical));
    }
    if lower.starts_with("gpt")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        return format!("openai/{canonical}");
    }
    canonical.to_string()
}

/// Rewrite a trailing `-<digits>-<digits>` version segment with a dot:
/// `claude-sonnet-4-6` → `claude-sonnet-4.6`. Leaves anything else unchanged.
fn hyphen_version_to_dot(s: &str) -> String {
    let segs: Vec<&str> = s.split('-').collect();
    let n = segs.len();
    if n >= 2
        && !segs[n - 1].is_empty()
        && segs[n - 1].bytes().all(|b| b.is_ascii_digit())
        && !segs[n - 2].is_empty()
        && segs[n - 2].bytes().all(|b| b.is_ascii_digit())
    {
        format!(
            "{}-{}.{}",
            segs[..n - 2].join("-"),
            segs[n - 2],
            segs[n - 1]
        )
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Credential persistence
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct Credentials {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    openrouter_api_key: Option<String>,
}

/// `~/.chidori/credentials.json` — the user-level credential store, distinct
/// from a project's `.chidori/runs`. `None` if no home directory is resolvable.
pub fn credentials_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".chidori").join("credentials.json"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// The OpenRouter API key to use, if any: the `OPENROUTER_API_KEY` env var
/// wins, otherwise a key saved by a prior `chidori login` / demo OAuth flow.
pub fn saved_api_key() -> Option<String> {
    if let Ok(key) = std::env::var(OPENROUTER_API_KEY_ENV) {
        if !key.trim().is_empty() {
            return Some(key);
        }
    }
    let path = credentials_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let creds: Credentials = serde_json::from_str(&raw).ok()?;
    creds.openrouter_api_key.filter(|k| !k.trim().is_empty())
}

/// Persist an OpenRouter API key to the user-level credential store, creating
/// `~/.chidori/` (mode 0700 on Unix) if needed.
pub fn save_api_key(key: &str) -> Result<PathBuf> {
    let path =
        credentials_path().context("could not resolve a home directory to save credentials")?;
    let dir = path.parent().expect("credentials path always has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }

    // Merge into any existing credentials rather than clobbering unrelated keys.
    let mut creds: Credentials = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    creds.openrouter_api_key = Some(key.to_string());
    let body = serde_json::to_string_pretty(&creds)?;
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

// ---------------------------------------------------------------------------
// OAuth (PKCE) login
// ---------------------------------------------------------------------------

/// Run the OpenRouter PKCE OAuth flow to completion and persist the resulting
/// API key. Prints progress, opens the user's browser, waits for the redirect
/// on a loopback callback, exchanges the code for a key, and saves it.
///
/// Synchronous: it owns a short-lived Tokio runtime for the HTTP exchange so it
/// can be called from the (blocking) CLI command handlers.
pub fn login_and_save() -> Result<String> {
    let rt = tokio::runtime::Runtime::new().context("creating tokio runtime for login")?;
    let key = rt.block_on(oauth_login())?;
    let path = save_api_key(&key)?;
    println!("Logged in to OpenRouter. Key saved to {}.", path.display());
    Ok(key)
}

/// The async PKCE flow. Returns the freshly minted OpenRouter API key.
pub async fn oauth_login() -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let (verifier, challenge) = pkce_pair();

    // Bind a loopback listener on an ephemeral port for the OAuth redirect.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding a local callback listener for OAuth")?;
    let port = listener.local_addr()?.port();
    let callback = format!("http://localhost:{port}/callback");

    let auth_url = format!(
        "{OPENROUTER_AUTH_URL}?callback_url={}&code_challenge={}&code_challenge_method=S256",
        urlencode(&callback),
        urlencode(&challenge),
    );

    println!();
    println!("No LLM provider key found — signing in with OpenRouter (opens your browser).");
    println!("If it doesn't open automatically, visit this URL:");
    println!();
    println!("  {auth_url}");
    println!();
    println!("Waiting for you to authorize in the browser…");
    let _ = open_browser(&auth_url);

    // Accept a single callback connection (with a generous timeout), read the
    // request line, and pull the `code` out of the query string.
    let accept = async {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.lines().next().and_then(|line| line.split(' ').nth(1));
            let Some(path) = path else {
                // Not an HTTP request we understand — keep waiting.
                continue;
            };
            let code = query_param(path, "code");
            let error = query_param(path, "error");

            let (status, message) = if code.is_some() {
                (
                    "200 OK",
                    "Signed in to OpenRouter. You can close this tab and return to your terminal.",
                )
            } else {
                (
                    "400 Bad Request",
                    "Sign-in failed. Return to your terminal and try again.",
                )
            };
            let body = callback_html(message);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;

            if let Some(code) = code {
                return Ok::<String, anyhow::Error>(code);
            }
            if let Some(error) = error {
                bail!("OpenRouter denied the authorization: {error}");
            }
            // A stray request (e.g. favicon) — keep listening for the real one.
        }
    };

    let code = tokio::time::timeout(std::time::Duration::from_secs(300), accept)
        .await
        .context("timed out after 5 minutes waiting for OpenRouter authorization")??;

    println!("Authorization received — exchanging it for an API key…");
    exchange_code_for_key(&code, &verifier).await
}

/// Exchange the authorization code + PKCE verifier for a user API key.
async fn exchange_code_for_key(code: &str, verifier: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct KeyResponse {
        key: String,
    }

    let client = reqwest::Client::new();
    let resp = client
        .post(OPENROUTER_KEYS_URL)
        .json(&serde_json::json!({
            "code": code,
            "code_verifier": verifier,
            "code_challenge_method": "S256",
        }))
        .send()
        .await
        .context("exchanging the OAuth code with OpenRouter")?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("OpenRouter key exchange failed ({status}): {text}");
    }
    let parsed: KeyResponse =
        serde_json::from_str(&text).context("parsing the OpenRouter key-exchange response")?;
    if parsed.key.trim().is_empty() {
        bail!("OpenRouter returned an empty API key");
    }
    Ok(parsed.key)
}

/// Generate a PKCE `(code_verifier, code_challenge)` pair. The verifier is
/// random URL-safe base64; the challenge is base64url(SHA-256(verifier)),
/// both unpadded, per RFC 7636 / OpenRouter's flow.
fn pkce_pair() -> (String, String) {
    use rand::RngCore;
    use sha2::{Digest, Sha256};

    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);

    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

/// Minimal percent-encoding for the query values we build (URLs + base64url).
/// Only the characters that actually appear (`:`, `/`) need escaping; base64url
/// output (`A-Za-z0-9-_`) is already safe.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Pull a query parameter value out of an HTTP request target like
/// `/callback?code=abc&scope=x`. Returns the raw (percent-decoded) value.
fn query_param(target: &str, key: &str) -> Option<String> {
    let query = target.split_once('?').map(|(_, q)| q)?;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return Some(percent_decode(v));
        }
    }
    None
}

/// Decode `%XX` escapes and `+` in a query value. OpenRouter codes are
/// URL-safe, but decode defensively so nothing is silently corrupted.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn callback_html(message: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Chidori · OpenRouter</title>\
<style>body{{font-family:system-ui,-apple-system,sans-serif;background:#0b0b0f;color:#e7e7ea;\
display:flex;align-items:center;justify-content:center;height:100vh;margin:0}}\
.card{{text-align:center;max-width:32rem;padding:2rem}}h1{{font-size:1.25rem}}\
p{{color:#a1a1aa}}</style></head><body><div class=\"card\"><h1>Chidori</h1><p>{message}</p></div></body></html>"
    )
}

/// Best-effort open of `url` in the user's default browser. Failure is fine —
/// the URL is always printed for manual copy/paste.
fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };

    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .context("launching a browser")
}

/// Prompt the user (y/N) to sign in with OpenRouter, returning their choice.
/// Reads a single line from stdin; a non-interactive/empty read is a "no".
pub fn confirm_login() -> bool {
    print!("Sign in with OpenRouter now? [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_default_claude_model_to_openrouter_slug() {
        assert_eq!(
            to_openrouter_slug("claude-sonnet-4-6"),
            "anthropic/claude-sonnet-4.6"
        );
        assert_eq!(
            to_openrouter_slug("claude-opus-4-7"),
            "anthropic/claude-opus-4.7"
        );
        assert_eq!(
            to_openrouter_slug("claude-haiku-4-5"),
            "anthropic/claude-haiku-4.5"
        );
    }

    #[test]
    fn maps_claude_aliases_before_slugging() {
        assert_eq!(
            to_openrouter_slug("claude-sonnet"),
            "anthropic/claude-sonnet-4.6"
        );
        assert_eq!(
            to_openrouter_slug("claude-3-5-sonnet"),
            "anthropic/claude-sonnet-4.6"
        );
    }

    #[test]
    fn maps_openai_models() {
        assert_eq!(to_openrouter_slug("gpt-4o"), "openai/gpt-4o");
        assert_eq!(to_openrouter_slug("gpt-4.1-mini"), "openai/gpt-4.1-mini");
        assert_eq!(to_openrouter_slug("o3-mini"), "openai/o3-mini");
    }

    #[test]
    fn passes_through_explicit_slugs_and_unknowns() {
        assert_eq!(
            to_openrouter_slug("anthropic/claude-sonnet-4.5"),
            "anthropic/claude-sonnet-4.5"
        );
        assert_eq!(
            to_openrouter_slug("meta-llama/llama-3.1-70b"),
            "meta-llama/llama-3.1-70b"
        );
        assert_eq!(to_openrouter_slug("some-local-model"), "some-local-model");
    }

    #[test]
    fn hyphen_version_only_touches_trailing_numeric_pair() {
        assert_eq!(
            hyphen_version_to_dot("claude-sonnet-4-6"),
            "claude-sonnet-4.6"
        );
        assert_eq!(hyphen_version_to_dot("claude-3-opus"), "claude-3-opus");
        assert_eq!(hyphen_version_to_dot("no-version"), "no-version");
    }

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        use sha2::{Digest, Sha256};
        let (verifier, challenge) = pkce_pair();
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected);
        // Unpadded URL-safe base64 has no '=', '+', or '/'.
        assert!(!challenge.contains(['=', '+', '/']));
        assert!(!verifier.contains(['=', '+', '/']));
    }

    #[test]
    fn extracts_query_param_from_callback_target() {
        assert_eq!(
            query_param("/callback?code=abc123&scope=x", "code").as_deref(),
            Some("abc123")
        );
        assert_eq!(query_param("/callback?error=denied", "code"), None);
        assert_eq!(query_param("/callback", "code"), None);
    }

    #[test]
    fn percent_decoding_roundtrips_callback_url() {
        let cb = "http://localhost:51423/callback";
        let encoded = urlencode(cb);
        assert!(!encoded.contains('/'));
        assert_eq!(percent_decode(&encoded), cb);
    }
}
