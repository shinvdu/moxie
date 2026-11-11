//! OIDC 登录（对应文章 4.3 节「第三层：OIDC 登录流程」）。
//!
//! 基于 **openidconnect**（通过 OpenID Relying Party Certification 官方认证）：
//! - Discovery / Authorization Code + PKCE / ID Token 验证（JWKS）全部由库处理；
//! - state（csrf + nonce + pkce_verifier）用 **AES-256-GCM 加密进 cookie**，无服务端存储；
//! - 成功后调用 `OidcAuthHandler`，由业务签发自有 JWT。
//!
//! 自动生成三个路由：
//! - `GET /.well-known/jwks.json`            框架 Ed25519 公钥端点
//! - `GET {prefix}/{provider}`               发起登录（302 到 IdP）
//! - `GET {prefix}/{provider}/callback`      回调：换 token → 验 ID Token → 业务 handler

use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::Aes256Gcm;
use axum::extract::{Extension, Query};
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::reqwest::async_http_client;
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::AppState;

/// OIDC 提供商配置（如 Google / GitHub）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OidcProviderConfig {
    pub name: String,
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub scopes: Vec<String>,
}

impl OidcProviderConfig {
    pub fn new(name: &str, issuer_url: &str) -> Self {
        Self {
            name: name.into(),
            issuer_url: issuer_url.into(),
            client_id: String::new(),
            client_secret: String::new(),
            scopes: vec!["openid".into(), "email".into(), "profile".into()],
        }
    }
}

/// 认证成功后的用户信息。
#[derive(Debug, Clone, Serialize)]
pub struct OidcUserInfo {
    pub sub: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub picture: Option<String>,
}

/// 自有 JWT 的交付方式。
pub enum TokenDelivery {
    /// 写 HttpOnly cookie 后跳转。
    Cookie {
        name: String,
        max_age_secs: u64,
        path: String,
        redirect_url: String,
    },
    /// 直接返回 JSON。
    Json,
}

/// 认证回调：业务在此查找/创建用户、签发自有 JWT（用 `state.jwt`）。
#[async_trait::async_trait]
pub trait OidcAuthHandler: Send + Sync + 'static {
    async fn on_authenticated(
        &self,
        user_info: OidcUserInfo,
        state: &AppState,
    ) -> Result<(serde_json::Value, TokenDelivery), AppError>;
}

// ---------- State cookie（AES-256-GCM 加密，无服务端存储） ----------

#[derive(Serialize, Deserialize)]
struct OidcStateData {
    csrf: String,
    nonce: String,
    pkce_verifier: String,
}

fn encrypt_state(cookie_key: &[u8; 32], data: &OidcStateData) -> Result<String, AppError> {
    let cipher = Aes256Gcm::new(aes_gcm::Key::<Aes256Gcm>::from_slice(cookie_key));
    let mut nonce_bytes = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = aes_gcm::Nonce::from_slice(&nonce_bytes);
    let plaintext = serde_json::to_vec(data).map_err(|e| AppError::Internal(e.to_string()))?;
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_ref())
        .map_err(|_| AppError::Internal("state encrypt failed".into()))?;
    let mut payload = nonce_bytes.to_vec();
    payload.extend_from_slice(&ciphertext);
    Ok(URL_SAFE_NO_PAD.encode(payload))
}

fn decrypt_state(cookie_key: &[u8; 32], token: &str) -> Result<OidcStateData, AppError> {
    let payload = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| AppError::BadRequest("bad oidc cookie".into()))?;
    if payload.len() < 12 {
        return Err(AppError::BadRequest("bad oidc cookie".into()));
    }
    let (nonce_bytes, ciphertext) = payload.split_at(12);
    let cipher = Aes256Gcm::new(aes_gcm::Key::<Aes256Gcm>::from_slice(cookie_key));
    let plaintext = cipher
        .decrypt(aes_gcm::Nonce::from_slice(nonce_bytes), ciphertext)
        .map_err(|_| AppError::BadRequest("oidc cookie tampered".into()))?;
    serde_json::from_slice(&plaintext).map_err(|_| AppError::BadRequest("bad oidc cookie".into()))
}

// ---------- OidcRouter ----------

struct ProviderRuntime {
    cfg: OidcProviderConfig,
    client: Arc<CoreClient>,
    handler: Arc<dyn OidcAuthHandler>,
}

/// OIDC 路由（文章 API：`OidcRouter::builder()...build().await`）。
pub struct OidcRouter {
    route_prefix: String,
    cookie_key: [u8; 32],
    providers: Vec<ProviderRuntime>,
}

impl OidcRouter {
    pub fn builder() -> OidcRouterBuilder {
        OidcRouterBuilder::default()
    }

    /// 把 OIDC 路由合并进主 Router（AppState 通过 Extension 注入）。
    pub fn into_router(self) -> Router {
        let prefix = self.route_prefix.clone();
        let mut router = Router::new().route("/.well-known/jwks.json", get(jwks_handler));
        for p in &self.providers {
            let name = p.cfg.name.clone();
            let login_ctx = Arc::new(LoginCtx {
                cfg: p.cfg.clone(),
                client: p.client.clone(),
                cookie_key: self.cookie_key,
                name: name.clone(),
            });
            let callback_ctx = Arc::new(CallbackCtx {
                cfg: p.cfg.clone(),
                client: p.client.clone(),
                cookie_key: self.cookie_key,
                name: name.clone(),
                handler: p.handler.clone(),
            });
            router = router
                .route(
                    &format!("{prefix}/{name}"),
                    get(login_route).layer(Extension(login_ctx)),
                )
                .route(
                    &format!("{prefix}/{name}/callback"),
                    get(callback_route).layer(Extension(callback_ctx)),
                );
        }
        router
    }
}

#[derive(Default)]
pub struct OidcRouterBuilder {
    base_url: Option<String>,
    route_prefix: String,
    cookie_key: Option<[u8; 32]>,
    providers: Vec<(OidcProviderConfig, Arc<dyn OidcAuthHandler>)>,
}

impl OidcRouterBuilder {
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }
    pub fn route_prefix(mut self, p: impl Into<String>) -> Self {
        self.route_prefix = p.into();
        self
    }
    pub fn cookie_key(mut self, key: [u8; 32]) -> Self {
        self.cookie_key = Some(key);
        self
    }
    pub fn provider<H: OidcAuthHandler>(mut self, cfg: OidcProviderConfig, handler: H) -> Self {
        self.providers.push((cfg, Arc::new(handler)));
        self
    }
    /// 构建：为每个 provider 做 Discovery，构建 CoreClient。
    pub async fn build(self) -> anyhow::Result<OidcRouter> {
        let base_url = self
            .base_url
            .ok_or_else(|| anyhow::anyhow!("base_url required"))?;
        let prefix = if self.route_prefix.is_empty() {
            "/api/auth".to_string()
        } else {
            self.route_prefix
        };
        let mut providers = Vec::new();
        for (cfg, handler) in self.providers {
            let redirect_uri = format!("{base_url}{prefix}/{}/callback", cfg.name);
            let meta = CoreProviderMetadata::discover_async(
                IssuerUrl::new(cfg.issuer_url.clone())?,
                async_http_client,
            )
            .await
            .map_err(|e| anyhow::anyhow!("oidc discovery failed: {e}"))?;
            let client = CoreClient::from_provider_metadata(
                meta,
                ClientId::new(cfg.client_id.clone()),
                Some(ClientSecret::new(cfg.client_secret.clone())),
            )
            .set_redirect_uri(RedirectUrl::new(redirect_uri)?);
            providers.push(ProviderRuntime {
                cfg,
                client: Arc::new(client),
                handler,
            });
        }
        Ok(OidcRouter {
            route_prefix: prefix,
            cookie_key: self
                .cookie_key
                .ok_or_else(|| anyhow::anyhow!("cookie_key required"))?,
            providers,
        })
    }
}

// ---------- 路由 handler ----------

/// JWKS 端点：输出框架 Ed25519 公钥（供第三方验证框架签发的 JWT）。
async fn jwks_handler(Extension(state): Extension<AppState>) -> Json<serde_json::Value> {
    let x = URL_SAFE_NO_PAD.encode(state.jwt.verifying_key_bytes());
    Json(serde_json::json!({
        "keys": [{
            "kty": "OKP",
            "crv": "Ed25519",
            "kid": "operon",
            "use": "sig",
            "alg": "EdDSA",
            "x": x,
        }]
    }))
}

struct LoginCtx {
    cfg: OidcProviderConfig,
    client: Arc<CoreClient>,
    cookie_key: [u8; 32],
    name: String,
}

async fn login_route(Extension(ctx): Extension<Arc<LoginCtx>>) -> Result<Response, AppError> {
    login(&ctx).await
}

/// 发起登录：openidconnect 生成授权 URL（含 PKCE/CSRF/nonce），state 加密进 cookie。
async fn login(ctx: &LoginCtx) -> Result<Response, AppError> {
    // oauth2 4.x：new_random_sha256() 返回 (challenge, verifier) 元组
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (auth_url, csrf, nonce) = ctx
        .client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scopes(ctx.cfg.scopes.iter().map(|s| Scope::new(s.clone())))
        .set_pkce_challenge(pkce_challenge)
        .url();

    let state_data = OidcStateData {
        csrf: csrf.secret().to_string(),
        nonce: nonce.secret().to_string(),
        pkce_verifier: pkce_verifier.secret().to_string(),
    };
    let cookie = encrypt_state(&ctx.cookie_key, &state_data)?;

    let mut response = Redirect::temporary(auth_url.as_str()).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "operon_oidc_{}={}; HttpOnly; SameSite=Lax; Path=/; Max-Age=600",
            ctx.name, cookie
        ))
        .unwrap(),
    );
    Ok(response)
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

struct CallbackCtx {
    cfg: OidcProviderConfig,
    client: Arc<CoreClient>,
    cookie_key: [u8; 32],
    name: String,
    handler: Arc<dyn OidcAuthHandler>,
}

async fn callback_route(
    Extension(ctx): Extension<Arc<CallbackCtx>>,
    Extension(state): Extension<AppState>,
    Query(q): Query<CallbackQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    callback(&ctx, state, q, headers).await
}

/// 回调：验 state cookie → 换 token（含 PKCE）→ 库验证 ID Token → 调业务 handler。
async fn callback(
    ctx: &CallbackCtx,
    state: AppState,
    q: CallbackQuery,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(err) = q.error {
        return Err(AppError::BadRequest(format!("oidc error: {err}")));
    }
    let code = q.code.ok_or_else(|| AppError::BadRequest("missing code".into()))?;
    let csrf_q = q.state.ok_or_else(|| AppError::BadRequest("missing state".into()))?;

    // 读并解密 state cookie
    let cookie_name = format!("operon_oidc_{}", ctx.name);
    let cookie_val = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| {
            c.split(';')
                .find(|p| p.trim().starts_with(&format!("{cookie_name}=")))
                .map(|p| p.splitn(2, '=').nth(1).unwrap_or("").trim().to_string())
        })
        .ok_or_else(|| AppError::BadRequest("missing oidc cookie".into()))?;
    let state_data = decrypt_state(&ctx.cookie_key, &cookie_val)?;
    if state_data.csrf != csrf_q {
        return Err(AppError::BadRequest("state mismatch (csrf)".into()));
    }

    // 换 token（openidconnect 处理 Authorization Code + PKCE）
    let token_resp = ctx
        .client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(PkceCodeVerifier::new(state_data.pkce_verifier))
        .request_async(async_http_client)
        .await
        .map_err(|e| AppError::Internal(format!("token exchange failed: {e:?}")))?;
    let id_token = token_resp
        .id_token()
        .ok_or_else(|| AppError::BadRequest("no id_token in token response".into()))?;

    // 库验证 ID Token（JWKS + exp/aud/iss/nonce）
    let claims = id_token
        .claims(&ctx.client.id_token_verifier(), &Nonce::new(state_data.nonce))
        .map_err(|e| AppError::Unauthorized(format!("id token invalid: {e}")))?;

    let user_info = OidcUserInfo {
        sub: claims.subject().as_str().to_string(),
        email: claims.email().map(|e| e.as_str().to_string()),
        name: claims.name().and_then(|n| n.get(None)).map(|v| v.as_str().to_string()),
        picture: claims.picture().and_then(|p| p.get(None)).map(|v| v.as_str().to_string()),
    };
    let (payload, delivery) = ctx.handler.on_authenticated(user_info, &state).await?;

    match delivery {
        TokenDelivery::Cookie {
            name,
            max_age_secs,
            path,
            redirect_url,
        } => {
            let token = payload
                .get("token")
                .and_then(|t| t.as_str())
                .ok_or_else(|| AppError::Internal("handler 需返回 token 字段".into()))?;
            let mut response = Redirect::temporary(&redirect_url).into_response();
            response.headers_mut().insert(
                header::SET_COOKIE,
                HeaderValue::from_str(&format!(
                    "{name}={token}; HttpOnly; SameSite=Lax; Path={path}; Max-Age={max_age_secs}"
                ))
                .unwrap(),
            );
            Ok(response)
        }
        // 浏览器登录场景：返回 HTML 把 token 存到 localStorage 并跳转（而非裸 JSON）
        TokenDelivery::Json => {
            let token = payload.get("token").and_then(|t| t.as_str()).unwrap_or("");
            // operon_user 存 JSON 对象（{"sub": ...}），前端 JSON.parse 后取 .sub
            let user_obj = serde_json::json!({ "sub": payload.get("sub") }).to_string();
            let html = format!(
                r#"<!doctype html><html><meta charset="utf-8"><script>
                localStorage.setItem('operon_token', '{}');
                localStorage.setItem('operon_user', '{}');
                location.href = '/my.html';
                </script></html>"#,
                token.replace('\\', "\\\\").replace('\'', "\\'"),
                user_obj.replace('\\', "\\\\").replace('\'', "\\'")
            );
            Ok(([(header::CONTENT_TYPE, "text/html")], html).into_response())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; 32] {
        [9u8; 32]
    }

    fn sample_data() -> OidcStateData {
        OidcStateData {
            csrf: "csrf1".into(),
            nonce: "nonce1".into(),
            pkce_verifier: "verifier123".into(),
        }
    }

    #[test]
    fn state_encrypt_decrypt_roundtrip() {
        let token = encrypt_state(&key(), &sample_data()).unwrap();
        let back = decrypt_state(&key(), &token).unwrap();
        assert_eq!(back.csrf, "csrf1");
        assert_eq!(back.pkce_verifier, "verifier123");
    }

    #[test]
    fn state_tampered_rejected() {
        let token = encrypt_state(&key(), &sample_data()).unwrap();
        let mut chars: Vec<char> = token.chars().collect();
        let mid = chars.len() / 2;
        chars[mid] = if chars[mid] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        assert!(matches!(
            decrypt_state(&key(), &tampered),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn state_wrong_key_rejected() {
        let token = encrypt_state(&key(), &sample_data()).unwrap();
        let other = [8u8; 32];
        assert!(matches!(decrypt_state(&other, &token), Err(AppError::BadRequest(_))));
    }
}
