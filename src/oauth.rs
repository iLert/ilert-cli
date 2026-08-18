//! OAuth2 Authorization Code + PKCE login for the CLI.
//!
//! Flow: generate a PKCE verifier/challenge, open the
//! browser to the authorize endpoint, catch the redirect on a fixed local
//! loopback port, then exchange the code (with the verifier) for tokens at the
//! token endpoint. No client secret is used — this is a public/native client.

use anyhow::{Context, Result};
use base64::Engine as _;
use chrono::{Duration, Utc};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

use crate::cli::RunContext;
use crate::config::ensure_secure_base_url;
use crate::errors::CliError;
use crate::secret_store::Credential;

/// OAuth2 `client_id` of the ilert CLI application on production.
///
/// Every ilert environment registers its own application, so this is a fallback
/// rather than a constant: the id in use is resolved per profile from
/// `--oauth-client-id`, `ILERT_OAUTH_CLIENT_ID` or `oauth_client_id` in
/// `config.json` (see [`crate::config::ConfigManager::resolve`]). A non-production
/// id therefore lives in the operator's own config, never in this repository.
///
/// The value is a public identifier, not a secret — under PKCE the code verifier
/// is what protects the exchange.
pub const DEFAULT_CLIENT_ID: &str = "a375a46d20f9ee0dca6b";

/// Which environment an OAuth call runs against, and as which registered
/// application.
///
/// The two travel together because a `client_id` is only meaningful at the
/// endpoint it was registered with — pairing a production id with a staging
/// base URL fails at the authorize step, and passing them as two bare `&str`
/// makes that mistake easy to write.
#[derive(Debug, Clone, Copy)]
pub struct OauthConfig<'a> {
    pub base_url: &'a str,
    pub client_id: &'a str,
}

/// Loopback port the CLI listens on for the OAuth redirect —
/// must match the port inside `REDIRECT_URI`.
pub const REDIRECT_PORT: u16 = 4597;

/// Registered redirect URI.
pub const REDIRECT_URI: &str = "http://localhost:4597/callback";

/// Space-separated OAuth scopes. `offline_access` is required to receive a
/// refresh token under PKCE;
///
/// The `:d` suffix on `wildcard` is required: ilert reads the suffix as the
/// permission level, and a bare scope grants read-only. `:d` is read + write +
/// delete — the CLI needs all three, since it exposes destructive operations.
pub const SCOPES: &str = "wildcard:d offline_access";

// OAuth2 endpoint paths.
const AUTHORIZE_PATH: &str = "/api/developers/oauth2/authorize";
const TOKEN_PATH: &str = "/api/developers/oauth2/token";
const REVOKE_PATH: &str = "/api/developers/oauth2/revoke";

/// Network timeout for token/revoke requests (chrono's `Duration` is in scope,
/// so use the fully-qualified std type here).
const TOKEN_HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// How long to wait for the browser to redirect back before giving up.
const CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
/// Max time to read the callback request once a connection is accepted.
const CALLBACK_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// HTTP client for OAuth endpoint calls — always with a timeout so a hung
/// token endpoint can't stall the CLI (silent refresh runs on normal commands).
fn http_client() -> Result<reqwest::Client> {
    crate::client::builder()
        .timeout(TOKEN_HTTP_TIMEOUT)
        // These POSTs carry an authorization code or a refresh token in the
        // body, and a 307/308 replays the body verbatim at the new location —
        // so a redirect from the token endpoint is a way to hand the secret to
        // whoever the response names. Same policy as the API client.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("Failed to build OAuth HTTP client")
}

/// Token endpoint response.
#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub token_type: String,
    pub access_token: String,
    pub expires_in: i64,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    // Captured from the IDP response for completeness; not currently used.
    #[serde(default)]
    #[allow(dead_code)]
    pub refresh_token_expires_in: Option<i64>,
}

impl TokenResponse {
    /// Convert a token response into a stored OAuth credential, computing
    /// `expires_at` from `expires_in` relative to now.
    ///
    /// `base_url` is the endpoint that issued these tokens; it is recorded so
    /// they can never be offered to a different one.
    pub fn into_credential(self, base_url: &str) -> Credential {
        let expires_at = Utc::now() + Duration::seconds(self.expires_in);
        let scopes = self.scope.split_whitespace().map(str::to_string).collect();
        Credential::OAuth {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at,
            token_type: self.token_type,
            scopes,
            base_url: Some(crate::config::normalize_base_url(base_url)),
        }
    }
}

/// Run the interactive browser + loopback login flow and return an OAuth
/// credential. Honors `ILERT_OAUTH_TEST_CODE` to bypass the browser in tests.
pub async fn run_login_flow(oauth: OauthConfig<'_>, ctx: &RunContext) -> Result<Credential> {
    // Never start an OAuth exchange against a cleartext endpoint.
    ensure_secure_base_url(oauth.base_url)?;

    let verifier = gen_verifier();
    let challenge = code_challenge(&verifier);
    let state = gen_state();
    let authorize_url = build_authorize_url(oauth, &challenge, &state)?;

    // Test seam: skip browser + loopback when an authorization code is injected.
    // Compiled out of release builds to keep it off the production attack surface.
    #[cfg(debug_assertions)]
    {
        if let Ok(code) = std::env::var("ILERT_OAUTH_TEST_CODE")
            && !code.is_empty()
        {
            let token = exchange_code(oauth, &code, &verifier).await?;
            return Ok(token.into_credential(oauth.base_url));
        }
    }

    // Bind the loopback first so we fail fast (and clearly) if the port is busy.
    let listener = TcpListener::bind(("127.0.0.1", REDIRECT_PORT))
        .await
        .map_err(|e| {
            CliError::user(format!(
                "Could not start the local login server on 127.0.0.1:{REDIRECT_PORT} ({e}). \
                 Close whatever is using that port and try again, or log in with \
                 `ilert auth login --with-token`."
            ))
        })?;

    ctx.info("Opening your browser to complete login...");
    ctx.info(&format!(
        "If it doesn't open, visit this URL:\n  {authorize_url}"
    ));
    let _ = open::that(&authorize_url);

    let (code, returned_state) = wait_for_callback(&listener).await?;
    if returned_state != state {
        return Err(CliError::user("OAuth state mismatch — possible CSRF, aborting.").into());
    }

    let token = exchange_code(oauth, &code, &verifier).await?;
    Ok(token.into_credential(oauth.base_url))
}

/// Refresh an access token using a refresh token (PKCE: no client secret).
pub async fn refresh(oauth: OauthConfig<'_>, refresh_token: &str) -> Result<TokenResponse> {
    post_token(
        oauth.base_url,
        &[
            ("client_id", oauth.client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ],
    )
    .await
}

/// Best-effort revocation of a refresh token (used on logout). Ignores errors,
/// but never sends the token over a cleartext endpoint.
pub async fn revoke(base_url: &str, refresh_token: &str) {
    if ensure_secure_base_url(base_url).is_err() {
        return;
    }
    let Ok(client) = http_client() else { return };
    let url = format!("{}{}", base_url.trim_end_matches('/'), REVOKE_PATH);
    let _ = client
        .post(&url)
        .header("Accept", "application/json")
        .form(&[("token", refresh_token)])
        .send()
        .await;
}

async fn exchange_code(
    oauth: OauthConfig<'_>,
    code: &str,
    verifier: &str,
) -> Result<TokenResponse> {
    post_token(
        oauth.base_url,
        &[
            ("client_id", oauth.client_id),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", verifier),
        ],
    )
    .await
}

async fn post_token(base_url: &str, params: &[(&str, &str)]) -> Result<TokenResponse> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), TOKEN_PATH);
    let resp = http_client()?
        .post(&url)
        .header("Accept", "application/json")
        .form(params)
        .send()
        .await
        .context("Token request failed")?;

    let status = resp.status();
    if status.is_redirection() {
        return Err(CliError::user(format!(
            "The token endpoint answered with a redirect (HTTP {}), which this client will not \
             follow — replaying the request would repeat the credential in its body at the new \
             location. Check that the base URL names the API host itself.",
            status.as_u16()
        ))
        .into());
    }
    let text = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        let msg = parse_oauth_error(&text)
            .unwrap_or_else(|| format!("Token endpoint returned HTTP {}", status.as_u16()));
        return Err(CliError::user(msg).into());
    }

    serde_json::from_str(&text).context("Failed to parse token response")
}

/// Map an IDP OAuth error body (`{ code, error }`) to a user-facing message.
fn parse_oauth_error(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let code = v.get("code").and_then(|c| c.as_str());
    let error = v
        .get("error")
        .and_then(|e| e.as_str())
        .or_else(|| v.get("error_description").and_then(|e| e.as_str()));
    match (code, error) {
        (Some("invalid_grant"), _) => {
            Some("Session expired or token revoked. Run `ilert auth login` again.".to_string())
        }
        (Some(c), Some(e)) => Some(format!("OAuth error ({c}): {e}")),
        (None, Some(e)) => Some(e.to_string()),
        (Some(c), None) => Some(format!("OAuth error: {c}")),
        (None, None) => None,
    }
}

fn build_authorize_url(oauth: OauthConfig<'_>, challenge: &str, state: &str) -> Result<String> {
    let endpoint = format!("{}{}", oauth.base_url.trim_end_matches('/'), AUTHORIZE_PATH);
    let url = Url::parse_with_params(
        &endpoint,
        &[
            ("client_id", oauth.client_id),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPES),
            ("state", state),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ],
    )
    .context("Failed to build authorize URL")?;
    Ok(url.to_string())
}

/// Placeholder in [`FAILURE_PAGE`] where the (HTML-escaped) error detail is
/// injected before the page is served.
const DETAIL_PLACEHOLDER: &str = "<!--DETAIL-->";

/// Placeholder in [`SUCCESS_PAGE`] replaced with the human-readable name of
/// the active credential store (see `secret_store::storage_label`).
const STORAGE_PLACEHOLDER: &str = "<!--STORAGE-->";

/// Self-contained success page shown in the browser after login completes.
/// Everything is inline (CSS, SVG, grain) — no network requests, no web fonts,
/// no tracking — so it renders instantly off the one-shot loopback socket.
/// Colors and logo are the ilert brand values, inlined for the same reason.
const SUCCESS_PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ilert · authenticated</title>
<style>
  :root{
    /* ilert brand tokens (see DESIGN.md) */
    --ilert-black:#19213D;
    --ilert-blue:#0375E5;
    --ilert-green:#34A853;
    --text-grey:#5D6382;
    /* derived dark-theme scale */
    --bg:#0e1428;
    --bg2:#121a33;
    --card:#19213D;
    --card2:#141b34;
    --line:#2a3358;
    --line-soft:#222a48;
    --txt:#f2f5fb;
    --muted:#9aa2c0;
    --faint:#5d6382;
    --ok:var(--ilert-green);
    --ok-glow:rgba(52,168,83,.5);
    --blue:var(--ilert-blue);
    --sans:'Inter',-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;
    --mono:"JetBrains Mono","SFMono-Regular",ui-monospace,"Cascadia Code",Menlo,Consolas,monospace;
  }
  *{box-sizing:border-box}
  html,body{height:100%}
  body{
    margin:0;
    font-family:var(--sans);
    color:var(--txt);
    background:
      radial-gradient(120% 80% at 50% -10%, rgba(52,168,83,.12), transparent 60%),
      radial-gradient(90% 60% at 50% 120%, rgba(3,117,229,.08), transparent 60%),
      var(--bg);
    display:grid;
    place-items:center;
    min-height:100svh;
    padding:28px;
    overflow:hidden;
  }
  /* faint dotted grid + grain for depth */
  body::before{
    content:"";position:fixed;inset:0;pointer-events:none;
    background-image:radial-gradient(rgba(255,255,255,.04) 1px, transparent 1px);
    background-size:22px 22px;
    -webkit-mask-image:radial-gradient(120% 90% at 50% 30%, #000 30%, transparent 75%);
            mask-image:radial-gradient(120% 90% at 50% 30%, #000 30%, transparent 75%);
  }
  body::after{
    content:"";position:fixed;inset:0;pointer-events:none;opacity:.45;mix-blend-mode:overlay;
    background-image:url("data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' width='120' height='120'><filter id='n'><feTurbulence type='fractalNoise' baseFrequency='.9' numOctaves='2'/></filter><rect width='100%25' height='100%25' filter='url(%23n)' opacity='.4'/></svg>");
  }

  .card{
    position:relative;
    width:min(480px,100%);
    background:linear-gradient(180deg, var(--card), var(--card2));
    border:1px solid var(--line);
    border-radius:18px;
    padding:34px 34px 26px;
    box-shadow:
      0 1px 0 rgba(255,255,255,.05) inset,
      0 40px 90px -40px rgba(0,0,0,.9),
      0 0 0 1px rgba(52,168,83,.05);
    animation:rise .7s cubic-bezier(.2,.8,.2,1) both;
  }
  /* top hairline accent */
  .card::before{
    content:"";position:absolute;left:24px;right:24px;top:0;height:1px;
    background:linear-gradient(90deg,transparent,var(--ok-glow),transparent);
  }

  .topbar{display:flex;align-items:center;justify-content:space-between;margin-bottom:26px}
  .brand{display:flex;align-items:center;gap:11px;font-size:16px;letter-spacing:-.01em}
  .brand .mark{
    width:28px;height:28px;border-radius:8px;
    background:var(--blue);
    display:grid;place-items:center;
    box-shadow:0 6px 16px -6px rgba(3,117,229,.8), 0 0 0 1px rgba(255,255,255,.06) inset;
  }
  .brand .mark svg{width:17px;height:17px;display:block}
  .brand b{color:var(--txt);font-weight:800}
  .brand span{color:var(--faint);font-weight:500}

  .chip{
    font-family:var(--sans);font-size:11px;font-weight:600;letter-spacing:.05em;text-transform:uppercase;
    color:var(--ok);display:flex;align-items:center;gap:7px;
    padding:5px 11px;border:1px solid rgba(52,168,83,.3);border-radius:999px;
    background:rgba(52,168,83,.08);
  }
  .dot{width:7px;height:7px;border-radius:50%;background:var(--ok);box-shadow:0 0 0 0 var(--ok-glow);animation:pulse 2s infinite}

  /* hero: heart-monitor sweep */
  .pulse{position:relative;height:74px;margin:6px -6px 20px}
  .pulse svg{width:100%;height:100%;display:block;overflow:visible}
  .pulse .trace{
    fill:none;stroke:url(#g);stroke-width:2.5;stroke-linecap:round;stroke-linejoin:round;
    stroke-dasharray:1100;stroke-dashoffset:1100;
    filter:drop-shadow(0 0 6px var(--ok-glow));
    animation:draw 2.4s cubic-bezier(.65,0,.35,1) .25s infinite;
  }
  .pulse .base{fill:none;stroke:var(--line);stroke-width:1.5;stroke-dasharray:2 6;opacity:.7}

  h1{font-size:25px;line-height:1.15;letter-spacing:-.02em;margin:0 0 8px;font-weight:800}
  .sub{color:var(--muted);font-size:14.5px;line-height:1.55;margin:0 0 22px}
  .sub b{color:var(--txt);font-weight:600}

  .next{
    border:1px solid var(--line-soft);border-radius:12px;overflow:hidden;
    background:#0c1226;
  }
  .next .hdr{
    display:flex;align-items:center;gap:7px;padding:9px 13px;border-bottom:1px solid var(--line-soft);
    font-family:var(--sans);font-size:11px;font-weight:600;letter-spacing:.05em;color:var(--faint);text-transform:uppercase;
  }
  .next .hdr i{width:9px;height:9px;border-radius:50%;background:#2a3358}
  .next .hdr span{margin-left:6px}
  .cmds{padding:12px 14px;font-family:var(--mono);font-size:13px;line-height:1.95}
  .cmds div{color:var(--txt);white-space:nowrap}
  .cmds .p{color:var(--ok);user-select:none;margin-right:8px}
  .cmds .c{color:var(--muted)}

  .foot{
    display:flex;align-items:center;justify-content:space-between;gap:12px;
    margin-top:18px;font-family:var(--sans);font-size:12px;color:var(--faint);
  }
  .foot .sec{display:flex;align-items:center;gap:6px}
  .foot .sec svg{width:13px;height:13px;opacity:.95}
  .foot .note{color:var(--muted)}

  .stagger{opacity:0;animation:up .6s cubic-bezier(.2,.8,.2,1) both}
  .d1{animation-delay:.08s}.d2{animation-delay:.16s}.d3{animation-delay:.26s}.d4{animation-delay:.36s}.d5{animation-delay:.46s}

  @keyframes rise{from{opacity:0;transform:translateY(14px) scale(.985)}to{opacity:1;transform:none}}
  @keyframes up{from{opacity:0;transform:translateY(9px)}to{opacity:1;transform:none}}
  @keyframes pulse{0%{box-shadow:0 0 0 0 var(--ok-glow)}70%{box-shadow:0 0 0 9px rgba(52,168,83,0)}100%{box-shadow:0 0 0 0 rgba(52,168,83,0)}}
  @keyframes draw{0%{stroke-dashoffset:1100}55%{stroke-dashoffset:0}100%{stroke-dashoffset:-1100}}

  @media (prefers-reduced-motion: reduce){
    *{animation:none!important}
    .stagger{opacity:1}
    .pulse .trace{stroke-dashoffset:0}
  }
</style>
</head>
<body>
  <main class="card">
    <div class="topbar stagger d1">
      <div class="brand">
        <span class="mark"><svg viewBox="0 0 71 71" fill="none"><path fill-rule="evenodd" clip-rule="evenodd" d="M15.0316 47.1243C19.5228 47.1281 23.1651 50.7681 23.1659 55.2612C23.1651 59.7517 19.5228 63.3948 15.0297 63.3948C10.5411 63.3951 6.89802 59.7513 6.89575 55.2616C6.89726 50.7685 10.5403 47.1254 15.0316 47.1243ZM50.8766 50.8774C59.5631 42.1909 59.5631 28.1031 50.8766 19.4162C42.1909 10.7305 28.1027 10.7305 19.4162 19.4173C15.0735 23.76 12.9018 29.4535 12.9018 35.1466L3.12221e-08 35.147C-0.000377922 26.1509 3.43068 17.1557 10.2928 10.2935C24.017 -3.43105 46.2765 -3.43106 60 10.2928C73.7246 24.0174 73.7246 46.2765 60 60.0011C53.1383 66.8625 44.143 70.2935 35.1477 70.2935L35.1473 57.3918C40.8404 57.3914 46.5335 55.22 50.8766 50.8774Z" fill="#fff"/></svg></span>
        <b>ilert</b><span>CLI</span>
      </div>
      <div class="chip"><span class="dot"></span>operational</div>
    </div>

    <div class="pulse stagger d2" aria-hidden="true">
      <svg viewBox="0 0 560 74" preserveAspectRatio="none">
        <defs>
          <linearGradient id="g" x1="0" y1="0" x2="1" y2="0">
            <stop offset="0" stop-color="#34A853" stop-opacity="0"/>
            <stop offset=".15" stop-color="#34A853" stop-opacity="1"/>
            <stop offset=".85" stop-color="#34A853" stop-opacity="1"/>
            <stop offset="1" stop-color="#34A853" stop-opacity="0"/>
          </linearGradient>
        </defs>
        <path class="base" d="M0,37 H560"/>
        <path class="trace" d="M0,37 H150 l14,-26 l11,49 l13,-44 l10,21 H300 l16,-31 l11,52 l13,-40 l9,18 H560"/>
      </svg>
    </div>

    <h1 class="stagger d2">You&rsquo;re authenticated.</h1>
    <p class="sub stagger d3">The ilert CLI is now connected to <b>your account</b>. Close this tab and head back to your terminal &mdash; you&rsquo;re on call.</p>

    <div class="next stagger d4">
      <div class="hdr"><i></i><i></i><span>try next</span></div>
      <div class="cmds">
        <div><span class="p">&rsaquo;</span>ilert alerts list <span class="c"># what&rsquo;s firing right now</span></div>
        <div><span class="p">&rsaquo;</span>ilert on-call now <span class="c"># who&rsquo;s holding the pager</span></div>
        <div><span class="p">&rsaquo;</span>ilert incidents create <span class="c"># declare an incident</span></div>
      </div>
    </div>

    <div class="foot stagger d5">
      <div class="sec">
        <svg viewBox="0 0 24 24" fill="none" stroke="#0375E5" stroke-width="2"><rect x="4" y="10" width="16" height="11" rx="2"/><path d="M8 10V7a4 4 0 0 1 8 0v3"/></svg>
        saving to <!--STORAGE-->
      </div>
      <div class="note">you can close this tab</div>
    </div>
  </main>
</body>
</html>"##;

/// Self-contained failure page. [`DETAIL_PLACEHOLDER`] is replaced with the
/// HTML-escaped error detail before serving.
const FAILURE_PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>ilert · login failed</title>
<style>
  :root{
    /* ilert brand tokens (see DESIGN.md) */
    --ilert-black:#19213D;
    --ilert-blue:#0375E5;
    --text-grey:#5D6382;
    /* derived dark-theme scale */
    --bg:#0e1428;--bg2:#121a33;--card:#19213D;--card2:#141b34;--line:#2a3358;--line-soft:#222a48;
    --txt:#f2f5fb;--muted:#9aa2c0;--faint:#5d6382;
    --blue:var(--ilert-blue);
    --err:#e5484d;--err-dim:#c93b40;--err-glow:rgba(229,72,77,.4);
    --sans:'Inter',-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;
    --mono:"JetBrains Mono","SFMono-Regular",ui-monospace,"Cascadia Code",Menlo,Consolas,monospace;
  }
  *{box-sizing:border-box}html,body{height:100%}
  body{
    margin:0;font-family:var(--sans);color:var(--txt);
    background:
      radial-gradient(120% 80% at 50% -10%, rgba(229,72,77,.10), transparent 60%),
      radial-gradient(90% 60% at 50% 120%, rgba(3,117,229,.07), transparent 60%),
      var(--bg);
    display:grid;place-items:center;min-height:100svh;padding:28px;overflow:hidden;
  }
  body::before{content:"";position:fixed;inset:0;pointer-events:none;
    background-image:radial-gradient(rgba(255,255,255,.04) 1px, transparent 1px);background-size:22px 22px;
    -webkit-mask-image:radial-gradient(120% 90% at 50% 30%, #000 30%, transparent 75%);mask-image:radial-gradient(120% 90% at 50% 30%, #000 30%, transparent 75%);}
  .card{position:relative;width:min(480px,100%);background:linear-gradient(180deg,var(--card),var(--card2));
    border:1px solid var(--line);border-radius:18px;padding:34px;
    box-shadow:0 1px 0 rgba(255,255,255,.05) inset,0 40px 90px -40px rgba(0,0,0,.9);
    animation:rise .7s cubic-bezier(.2,.8,.2,1) both;}
  .card::before{content:"";position:absolute;left:24px;right:24px;top:0;height:1px;
    background:linear-gradient(90deg,transparent,var(--err-glow),transparent);}
  .topbar{display:flex;align-items:center;justify-content:space-between;margin-bottom:26px}
  .brand{display:flex;align-items:center;gap:11px;font-size:16px;letter-spacing:-.01em}
  .brand .mark{width:28px;height:28px;border-radius:8px;background:var(--blue);
    display:grid;place-items:center;box-shadow:0 6px 16px -6px rgba(3,117,229,.8),0 0 0 1px rgba(255,255,255,.06) inset}
  .brand .mark svg{width:17px;height:17px;display:block}
  .brand b{color:var(--txt);font-weight:800}
  .brand span{color:var(--faint);font-weight:500}
  .chip{font-family:var(--sans);font-size:11px;font-weight:600;letter-spacing:.05em;text-transform:uppercase;color:var(--err);
    display:flex;align-items:center;gap:7px;padding:5px 11px;border:1px solid rgba(229,72,77,.3);
    border-radius:999px;background:rgba(229,72,77,.08)}
  .dot{width:7px;height:7px;border-radius:50%;background:var(--err);animation:pulse 1.6s infinite}

  .pulse{position:relative;height:74px;margin:6px -6px 20px}
  .pulse svg{width:100%;height:100%;display:block;overflow:visible}
  .pulse .base{fill:none;stroke:var(--line);stroke-width:1.5;stroke-dasharray:2 6;opacity:.7}
  .pulse .trace{fill:none;stroke:var(--err);stroke-width:2.5;stroke-linecap:round;stroke-linejoin:round;
    filter:drop-shadow(0 0 6px var(--err-glow));opacity:.9}
  .pulse .flat{fill:none;stroke:var(--err);stroke-width:2.5;stroke-linecap:round;opacity:.9;
    filter:drop-shadow(0 0 6px var(--err-glow));stroke-dasharray:6 7;animation:flat 1.2s linear infinite}

  h1{font-size:25px;line-height:1.15;letter-spacing:-.02em;margin:0 0 8px;font-weight:800}
  .sub{color:var(--muted);font-size:14.5px;line-height:1.55;margin:0 0 18px}

  .err{border:1px solid rgba(229,72,77,.25);border-radius:12px;background:rgba(229,72,77,.06);
    padding:12px 14px;font-family:var(--mono);font-size:12.5px;line-height:1.6;color:#ffd7d8;
    display:flex;gap:10px;align-items:flex-start}
  .err svg{width:15px;height:15px;flex:0 0 auto;margin-top:2px;stroke:var(--err)}
  .err .label{font-family:var(--sans);color:var(--err);text-transform:uppercase;letter-spacing:.05em;font-weight:600;font-size:10.5px;display:block;margin-bottom:3px}

  .retry{margin-top:18px;border:1px solid var(--line-soft);border-radius:12px;background:#0c1226;overflow:hidden}
  .retry .hdr{padding:9px 13px;border-bottom:1px solid var(--line-soft);font-family:var(--sans);font-size:11px;font-weight:600;
    letter-spacing:.05em;color:var(--faint);text-transform:uppercase}
  .retry .cmd{padding:12px 14px;font-family:var(--mono);font-size:13px;color:var(--txt)}
  .retry .cmd .p{color:var(--blue);user-select:none;margin-right:8px}

  .foot{margin-top:18px;font-family:var(--sans);font-size:12px;color:var(--faint);text-align:center}

  @keyframes rise{from{opacity:0;transform:translateY(14px) scale(.985)}to{opacity:1;transform:none}}
  @keyframes pulse{0%{box-shadow:0 0 0 0 var(--err-glow)}70%{box-shadow:0 0 0 9px rgba(229,72,77,0)}100%{box-shadow:0 0 0 0 rgba(229,72,77,0)}}
  @keyframes flat{to{stroke-dashoffset:-26}}
  @media (prefers-reduced-motion: reduce){*{animation:none!important}}
</style>
</head>
<body>
  <main class="card">
    <div class="topbar">
      <div class="brand">
        <span class="mark"><svg viewBox="0 0 71 71" fill="none"><path fill-rule="evenodd" clip-rule="evenodd" d="M15.0316 47.1243C19.5228 47.1281 23.1651 50.7681 23.1659 55.2612C23.1651 59.7517 19.5228 63.3948 15.0297 63.3948C10.5411 63.3951 6.89802 59.7513 6.89575 55.2616C6.89726 50.7685 10.5403 47.1254 15.0316 47.1243ZM50.8766 50.8774C59.5631 42.1909 59.5631 28.1031 50.8766 19.4162C42.1909 10.7305 28.1027 10.7305 19.4162 19.4173C15.0735 23.76 12.9018 29.4535 12.9018 35.1466L3.12221e-08 35.147C-0.000377922 26.1509 3.43068 17.1557 10.2928 10.2935C24.017 -3.43105 46.2765 -3.43106 60 10.2928C73.7246 24.0174 73.7246 46.2765 60 60.0011C53.1383 66.8625 44.143 70.2935 35.1477 70.2935L35.1473 57.3918C40.8404 57.3914 46.5335 55.22 50.8766 50.8774Z" fill="#fff"/></svg></span>
        <b>ilert</b><span>CLI</span>
      </div>
      <div class="chip"><span class="dot"></span>not connected</div>
    </div>

    <div class="pulse" aria-hidden="true">
      <svg viewBox="0 0 560 74" preserveAspectRatio="none">
        <path class="base" d="M0,37 H560"/>
        <path class="trace" d="M0,37 H170 l14,-22 l11,40 l13,-32 l9,14 H250"/>
        <path class="flat" d="M250,37 H560"/>
      </svg>
    </div>

    <h1>Login didn&rsquo;t complete.</h1>
    <p class="sub">We couldn&rsquo;t finish authenticating the ilert CLI. Your terminal is safe to return to &mdash; nothing was changed.</p>

    <div class="err">
      <svg viewBox="0 0 24 24" fill="none" stroke-width="2"><path d="M12 9v4m0 4h.01M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0Z"/></svg>
      <div><span class="label">what happened</span><!--DETAIL--></div>
    </div>

    <div class="retry">
      <div class="hdr">try again</div>
      <div class="cmd"><span class="p">&rsaquo;</span>ilert auth login</div>
    </div>

    <p class="foot">You can close this tab.</p>
  </main>
</body>
</html>"##;

/// Accept a single connection on the loopback, parse the OAuth redirect query,
/// reply with a small HTML page, and return `(code, state)`.
async fn wait_for_callback(listener: &TcpListener) -> Result<(String, String)> {
    let (mut stream, _) = tokio::time::timeout(CALLBACK_TIMEOUT, listener.accept())
        .await
        .map_err(|_| {
            CliError::user(
                "Timed out waiting for the browser to complete login. Run `ilert auth login` again.",
            )
        })?
        .context("Failed to accept the OAuth callback connection")?;

    let mut buf = vec![0u8; 8192];
    let n = tokio::time::timeout(CALLBACK_READ_TIMEOUT, stream.read(&mut buf))
        .await
        .map_err(|_| CliError::user("Timed out reading the OAuth callback request."))?
        .context("Failed to read the OAuth callback request")?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // First request line: "GET /callback?code=...&state=... HTTP/1.1"
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| CliError::user("Malformed OAuth callback request"))?;

    let parsed = Url::parse(&format!("http://localhost{target}"))
        .map_err(|_| CliError::user("Malformed OAuth callback URL"))?;

    let (mut code, mut state, mut error, mut error_desc) = (None, None, None, None);
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            "error_description" => error_desc = Some(v.into_owned()),
            _ => {}
        }
    }

    let success = error.is_none() && code.is_some() && state.is_some();
    let body = if success {
        SUCCESS_PAGE.replace(STORAGE_PLACEHOLDER, &crate::secret_store::storage_label())
    } else {
        let detail = error_desc
            .clone()
            .or_else(|| error.clone())
            .unwrap_or_else(|| "Missing authorization code".to_string());
        FAILURE_PAGE.replace(DETAIL_PLACEHOLDER, &html_escape(&detail))
    };
    let status_line = if success {
        "HTTP/1.1 200 OK"
    } else {
        "HTTP/1.1 400 Bad Request"
    };
    let response = format!(
        "{status_line}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;

    if let Some(err) = error {
        let detail = error_desc.unwrap_or(err);
        return Err(CliError::user(format!("Authorization denied: {detail}")).into());
    }
    match (code, state) {
        (Some(c), Some(s)) => Ok((c, s)),
        _ => Err(CliError::user("OAuth callback missing code or state").into()),
    }
}

fn gen_verifier() -> String {
    random_string(64)
}

fn gen_state() -> String {
    random_string(32)
}

fn random_string(len: usize) -> String {
    use rand::RngExt;
    use rand::distr::Alphanumeric;
    rand::rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}

fn code_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// Minimal HTML escaping for values reflected into the local callback page.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_is_base64url_sha256_no_pad() {
        // Known S256 vector from RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(code_challenge(verifier), expected);
    }

    #[test]
    fn scopes_request_write_and_delete_permission() {
        // Regression: ilert reads the `:w`/`:d` suffix as the permission level,
        // so a bare `wildcard` mints a read-only token.
        assert!(
            SCOPES.split_whitespace().any(|s| s == "wildcard:d"),
            "SCOPES must request wildcard:d, got: {SCOPES}"
        );
        // Required for a refresh token under PKCE.
        assert!(SCOPES.split_whitespace().any(|s| s == "offline_access"));
    }

    /// An authorize URL for an arbitrary environment, as the flow would build it.
    fn authorize_url(base_url: &str, client_id: &str) -> Url {
        let raw = build_authorize_url(
            OauthConfig {
                base_url,
                client_id,
            },
            "challenge",
            "state",
        )
        .unwrap();
        Url::parse(&raw).unwrap()
    }

    fn query_param(url: &Url, key: &str) -> String {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
            .unwrap_or_else(|| panic!("{key} param"))
    }

    #[test]
    fn authorize_url_carries_suffixed_scope() {
        let url = authorize_url("https://app.ilert.com", DEFAULT_CLIENT_ID);
        assert_eq!(query_param(&url, "scope"), SCOPES);
    }

    #[test]
    fn authorize_url_uses_the_configured_client_id() {
        // A non-production environment registers its own application, so the id
        // must come from the resolved config and not from the compiled default.
        let url = authorize_url("https://api.example.test", "staging-client-id");
        assert_eq!(query_param(&url, "client_id"), "staging-client-id");
        assert!(url.as_str().starts_with("https://api.example.test/"));
    }

    #[test]
    fn verifier_length_within_pkce_bounds() {
        let v = gen_verifier();
        assert!((43..=128).contains(&v.len()));
    }

    #[test]
    fn html_escape_neutralizes_markup() {
        let escaped = html_escape("<script>alert('x')</script>&\"");
        assert!(!escaped.contains('<'));
        assert!(!escaped.contains('>'));
        assert_eq!(
            escaped,
            "&lt;script&gt;alert(&#x27;x&#x27;)&lt;/script&gt;&amp;&quot;"
        );
    }

    #[test]
    fn success_page_storage_label_is_templated() {
        // The page must carry the placeholder, and serving must replace it with
        // the real backend label (so we never hardcode "keychain" everywhere).
        assert!(SUCCESS_PAGE.contains(STORAGE_PLACEHOLDER));
        let rendered =
            SUCCESS_PAGE.replace(STORAGE_PLACEHOLDER, &crate::secret_store::storage_label());
        assert!(!rendered.contains(STORAGE_PLACEHOLDER));
        assert!(rendered.contains("saving to "));
    }

    #[test]
    fn failure_page_detail_is_templated() {
        assert!(FAILURE_PAGE.contains(DETAIL_PLACEHOLDER));
        let rendered = FAILURE_PAGE.replace(DETAIL_PLACEHOLDER, &html_escape("access_denied"));
        assert!(!rendered.contains(DETAIL_PLACEHOLDER));
        assert!(rendered.contains("access_denied"));
    }

    #[test]
    fn token_response_into_credential_parses_scopes() {
        let resp = TokenResponse {
            token_type: "Bearer".to_string(),
            access_token: "at".to_string(),
            expires_in: 3600,
            scope: "wildcard offline_access".to_string(),
            refresh_token: Some("rt".to_string()),
            refresh_token_expires_in: Some(31536000),
        };
        let cred = resp.into_credential("https://api.example.test/");
        match cred {
            Credential::OAuth {
                scopes,
                token_type,
                refresh_token,
                base_url,
                ..
            } => {
                assert_eq!(scopes, vec!["wildcard", "offline_access"]);
                assert_eq!(token_type, "Bearer");
                assert_eq!(refresh_token.as_deref(), Some("rt"));
                // Bound to the issuing environment, normalized.
                assert_eq!(base_url.as_deref(), Some("https://api.example.test"));
            }
            _ => panic!("expected OAuth credential"),
        }
    }
}
