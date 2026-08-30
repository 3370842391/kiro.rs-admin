//! Kiro Runtime 端点
//!
//! 对应官方 Kiro IDE 1.0.395 实测推理链路（kiro-tap 抓包）：
//! - API: `POST https://runtime.{api_region}.kiro.dev/`
//!   `x-amz-target: KiroRuntimeService.GenerateAssistantResponse`
//! - MCP: `POST https://runtime.{api_region}.kiro.dev/`
//!   `x-amz-target: KiroRuntimeService.InvokeMCP`
//!
//! 与旧 `ide`（`q.amazonaws.com/generateAssistantResponse` + aws-sdk-js/1.0.34）
//! 不是同一条协议。`runtime.kiro.dev` 仍是独立限流桶，429 时沿
//! [`super::KiroEndpoint::fallback_chain`] 回切 q 家族。

use reqwest::RequestBuilder;
use serde_json::Value;
use uuid::Uuid;

use super::ide::inject_profile_arn;
use super::rate_limit::kiro_attempt_header;
use super::{KiroEndpoint, RequestContext};
use crate::kiro::kiro_version;
use crate::kiro::region::{KiroService, data_plane_host};

/// Kiro Runtime 端点名称
pub const RUNTIME_ENDPOINT_NAME: &str = "runtime";

/// 官方 IDE Generate 的 x-amz-target
const KIRO_RUNTIME_GENERATE_TARGET: &str = "KiroRuntimeService.GenerateAssistantResponse";
/// 官方 IDE InvokeMCP 的 x-amz-target
const KIRO_RUNTIME_INVOKE_MCP_TARGET: &str = "KiroRuntimeService.InvokeMCP";
/// 官方 UA 里的 Kiro Agent Service 版本
const KIRO_AGENT_SERVICE_VERSION: &str = "0.54.0";

/// Kiro Runtime 端点
pub struct RuntimeEndpoint;

impl RuntimeEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        ctx.credentials.effective_api_region(ctx.config)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        data_plane_host(KiroService::Runtime, self.api_region(ctx))
            .expect("API region must be validated before building runtime requests")
    }

    fn ide_tag(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "KiroIDE-{}-{}-KAS/{}",
            kiro_version::effective(&ctx.config.kiro_version),
            ctx.machine_id,
            KIRO_AGENT_SERVICE_VERSION
        )
    }

    fn x_amz_user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!("aws-sdk-js/1.0.0 {}", self.ide_tag(ctx))
    }

    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/kiroruntime#1.0.0 m/N {}",
            ctx.config.system_version,
            ctx.config.node_version,
            self.ide_tag(ctx)
        )
    }

    fn apply_token_type(&self, mut req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        if let Some(token_type) = ctx.credentials.token_type_header() {
            req = req.header("tokentype", token_type);
        }
        req
    }
}

impl Default for RuntimeEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for RuntimeEndpoint {
    fn name(&self) -> &'static str {
        RUNTIME_ENDPOINT_NAME
    }

    fn protocol(&self) -> &'static str {
        "ide"
    }

    /// runtime 走 `runtime.kiro.dev`（IDE 协议）。429 时沿链回切到 q 家族的独立限流桶：
    /// ide（q host）→ codewhisperer（独立 host）→ amazonq（同 q host 不同服务）。链内全部 IDE 协议。
    fn fallback_chain(&self) -> &'static [&'static str] {
        use crate::kiro::endpoint::{
            amazonq::AMAZONQ_ENDPOINT_NAME, codewhisperer::CODEWHISPERER_ENDPOINT_NAME,
            ide::IDE_ENDPOINT_NAME,
        };
        &[
            IDE_ENDPOINT_NAME,
            CODEWHISPERER_ENDPOINT_NAME,
            AMAZONQ_ENDPOINT_NAME,
        ]
    }

    fn content_type(&self) -> &'static str {
        "application/x-amz-json-1.0"
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/", self.host(ctx))
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://{}/", self.host(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let req = req
            .header("x-amz-target", KIRO_RUNTIME_GENERATE_TARGET)
            .header("x-amzn-kiro-client-attribution", "kiro-ide")
            .header(
                "x-kiro-attempt",
                kiro_attempt_header(ctx.request_attempt, ctx.request_attempt_max),
            )
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));
        self.apply_token_type(req, ctx)
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amz-target", KIRO_RUNTIME_INVOKE_MCP_TARGET)
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if let Some(arn) = ctx.credentials.effective_profile_arn() {
            req = req.header("x-amzn-kiro-profile-arn", arn);
        }
        self.apply_token_type(req, ctx)
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        transform_runtime_api_body(body, ctx.credentials.streaming_profile_arn().as_deref())
    }

    fn transform_mcp_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        inject_profile_arn(body, ctx.credentials.streaming_profile_arn().as_deref())
    }
}

/// 官方 Generate 请求体：profileArn + agentMode，并在缺失时补 rootConversationId。
/// 已有 agentMode / rootConversationId 不覆盖（Continue 轮的 root 可能不同于 conversationId）。
fn transform_runtime_api_body(request_body: &str, profile_arn: Option<&str>) -> String {
    let Ok(mut json) = serde_json::from_str::<Value>(request_body) else {
        return request_body.to_string();
    };
    if let Some(arn) = profile_arn {
        json["profileArn"] = Value::String(arn.to_string());
    }
    let agent_mode_empty = json
        .get("agentMode")
        .and_then(Value::as_str)
        .map(str::is_empty)
        .unwrap_or(true);
    if agent_mode_empty {
        json["agentMode"] = Value::String("vibe".to_string());
    }
    if let Some(state) = json.get_mut("conversationState") {
        if state.get("rootConversationId").is_none() {
            if let Some(id) = state
                .get("conversationId")
                .and_then(Value::as_str)
                .map(str::to_string)
            {
                state["rootConversationId"] = Value::String(id);
            }
        }
    }
    serde_json::to_string(&json).unwrap_or_else(|_| request_body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::Config;
    use serde_json::Value;

    fn ctx<'a>(
        creds: &'a KiroCredentials,
        config: &'a Config,
        machine_id: &'a str,
    ) -> RequestContext<'a> {
        RequestContext {
            credentials: creds,
            token: "tok",
            machine_id,
            config,
            request_attempt: 1,
            request_attempt_max: 3,
        }
    }

    fn social_creds() -> KiroCredentials {
        let mut creds = KiroCredentials::default();
        creds.auth_method = Some("social".to_string());
        creds.profile_arn = Some("arn:aws:codewhisperer:us-east-1:123:profile/ABC".to_string());
        creds
    }

    fn runtime_ctx<'a>(
        creds: &'a KiroCredentials,
        config: &'a mut Config,
        machine_id: &'a str,
    ) -> RequestContext<'a> {
        config.api_region = Some("us-east-1".to_string());
        config.kiro_version = "1.0.395".to_string();
        config.system_version = "win32#10.0.26200".to_string();
        config.node_version = "22.22.0".to_string();
        ctx(creds, config, machine_id)
    }

    fn header<'a>(req: &'a reqwest::Request, name: &str) -> Option<&'a str> {
        req.headers().get(name).and_then(|v| v.to_str().ok())
    }

    fn has_header(req: &reqwest::Request, name: &str) -> bool {
        req.headers().contains_key(name)
    }

    #[test]
    fn test_runtime_urls_use_root_path() {
        let endpoint = RuntimeEndpoint::new();
        let creds = social_creds();
        let mut config = Config::default();
        let rctx = runtime_ctx(&creds, &mut config, "machine");

        assert_eq!(
            endpoint.api_url(&rctx),
            "https://runtime.us-east-1.kiro.dev/"
        );
        assert_eq!(
            endpoint.mcp_url(&rctx),
            "https://runtime.us-east-1.kiro.dev/"
        );
        assert_eq!(endpoint.host(&rctx), "runtime.us-east-1.kiro.dev");
        assert_eq!(endpoint.content_type(), "application/x-amz-json-1.0");
    }

    #[test]
    fn test_runtime_fallback_chain_starts_with_ide() {
        assert_eq!(
            RuntimeEndpoint::new().fallback_chain().first().copied(),
            Some(super::super::ide::IDE_ENDPOINT_NAME)
        );
    }

    #[test]
    fn test_runtime_generate_headers_match_official_ide() {
        let endpoint = RuntimeEndpoint::new();
        let creds = social_creds();
        let mut config = Config::default();
        let rctx = runtime_ctx(&creds, &mut config, "aabbcc");
        let req = endpoint
            .decorate_api(
                reqwest::Client::new().post(endpoint.api_url(&rctx)),
                &rctx,
            )
            .build()
            .unwrap();

        assert_eq!(
            header(&req, "x-amz-target"),
            Some("KiroRuntimeService.GenerateAssistantResponse")
        );
        assert_eq!(
            header(&req, "x-amzn-kiro-client-attribution"),
            Some("kiro-ide")
        );
        assert_eq!(header(&req, "x-kiro-attempt"), Some("1;max=3"));
        assert_eq!(header(&req, "amz-sdk-request"), Some("attempt=1; max=3"));
        assert_eq!(header(&req, "authorization"), Some("Bearer tok"));
        assert_eq!(
            header(&req, "x-amz-user-agent"),
            Some("aws-sdk-js/1.0.0 KiroIDE-1.0.395-aabbcc-KAS/0.54.0")
        );
        assert_eq!(
            header(&req, "user-agent"),
            Some(
                "aws-sdk-js/1.0.0 ua/2.1 os/win32#10.0.26200 lang/js md/nodejs#22.22.0 api/kiroruntime#1.0.0 m/N KiroIDE-1.0.395-aabbcc-KAS/0.54.0"
            )
        );
        assert!(!has_header(&req, "x-amzn-kiro-agent-mode"));
        assert!(!has_header(&req, "x-amzn-codewhisperer-optout"));
        assert!(!has_header(&req, "tokentype"));
    }

    #[test]
    fn test_runtime_generate_headers_bump_attempt() {
        let endpoint = RuntimeEndpoint::new();
        let creds = social_creds();
        let mut config = Config::default();
        let mut rctx = runtime_ctx(&creds, &mut config, "aabbcc");
        rctx.request_attempt = 2;
        rctx.request_attempt_max = 3;
        let req = endpoint
            .decorate_api(
                reqwest::Client::new().post(endpoint.api_url(&rctx)),
                &rctx,
            )
            .build()
            .unwrap();
        assert_eq!(header(&req, "x-kiro-attempt"), Some("2;max=3"));
    }

    #[test]
    fn test_runtime_mcp_uses_invoke_mcp_target() {
        let endpoint = RuntimeEndpoint::new();
        let creds = social_creds();
        let mut config = Config::default();
        let rctx = runtime_ctx(&creds, &mut config, "aabbcc");
        let req = endpoint
            .decorate_mcp(
                reqwest::Client::new().post(endpoint.mcp_url(&rctx)),
                &rctx,
            )
            .build()
            .unwrap();

        assert_eq!(
            header(&req, "x-amz-target"),
            Some("KiroRuntimeService.InvokeMCP")
        );
        assert_eq!(
            header(&req, "x-amz-user-agent"),
            Some("aws-sdk-js/1.0.0 KiroIDE-1.0.395-aabbcc-KAS/0.54.0")
        );
        assert!(!has_header(&req, "x-amzn-kiro-client-attribution"));
        assert!(!has_header(&req, "x-kiro-attempt"));
        assert_eq!(
            header(&req, "x-amzn-kiro-profile-arn"),
            Some("arn:aws:codewhisperer:us-east-1:123:profile/ABC")
        );
    }

    #[test]
    fn test_runtime_api_key_still_sends_tokentype() {
        let endpoint = RuntimeEndpoint::new();
        let mut creds = social_creds();
        creds.kiro_api_key = Some("ksk_test".to_string());
        creds.auth_method = Some("api_key".to_string());
        let mut config = Config::default();
        let rctx = runtime_ctx(&creds, &mut config, "aabbcc");
        let req = endpoint
            .decorate_api(
                reqwest::Client::new().post(endpoint.api_url(&rctx)),
                &rctx,
            )
            .build()
            .unwrap();
        assert_eq!(header(&req, "tokentype"), Some("API_KEY"));
    }

    #[test]
    fn test_transform_api_body_injects_agent_mode_and_root_conversation() {
        let endpoint = RuntimeEndpoint::new();
        let creds = social_creds();
        let mut config = Config::default();
        let rctx = runtime_ctx(&creds, &mut config, "aabbcc");
        let body = r#"{"conversationState":{"conversationId":"c1","currentMessage":{}}}"#;
        let json: Value = serde_json::from_str(&endpoint.transform_api_body(body, &rctx)).unwrap();
        assert_eq!(json["agentMode"], "vibe");
        assert_eq!(
            json["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/ABC"
        );
        assert_eq!(json["conversationState"]["rootConversationId"], "c1");
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }

    #[test]
    fn test_transform_api_body_keeps_existing_agent_mode_and_root() {
        let endpoint = RuntimeEndpoint::new();
        let creds = social_creds();
        let mut config = Config::default();
        let rctx = runtime_ctx(&creds, &mut config, "aabbcc");
        let body = r#"{"conversationState":{"conversationId":"c1","rootConversationId":"root-9"},"agentMode":"spec"}"#;
        let json: Value = serde_json::from_str(&endpoint.transform_api_body(body, &rctx)).unwrap();
        assert_eq!(json["agentMode"], "spec");
        assert_eq!(json["conversationState"]["rootConversationId"], "root-9");
    }

    #[test]
    fn test_transform_mcp_body_injects_profile_arn() {
        let endpoint = RuntimeEndpoint::new();
        let creds = social_creds();
        let mut config = Config::default();
        let rctx = runtime_ctx(&creds, &mut config, "aabbcc");
        let body = r#"{"id":"tools_list","method":"tools/list","jsonrpc":"2.0"}"#;
        let json: Value = serde_json::from_str(&endpoint.transform_mcp_body(body, &rctx)).unwrap();
        assert_eq!(
            json["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/ABC"
        );
        assert_eq!(json["method"], "tools/list");
    }

}
