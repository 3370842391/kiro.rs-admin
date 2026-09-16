use super::*;
use crate::kiro::token_manager::CallContext;
use crate::model::config::EnterpriseRetrySettings;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use tokio::time::{Instant as Deadline, timeout_at};

const MAX_PREFIX_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub(super) struct EnterprisePolicy {
    pub settings: EnterpriseRetrySettings,
    default_endpoint: String,
    pub max_calls: usize,
    pub deadline: Deadline,
    pub enterprise_deadline: Deadline,
}

/// 一个入站请求共享，不随 provider / handler 重试重新分配预算。
#[derive(Default)]
pub struct EnterpriseRequestControl {
    policy: OnceLock<Option<EnterprisePolicy>>,
    calls: AtomicU32,
    upstream_calls: AtomicU32,
    pub(super) finished: AtomicBool,
    accepted: Mutex<Option<(Arc<MultiTokenManager>, u64)>>,
}

impl EnterpriseRequestControl {
    pub(super) fn note_upstream_send(&self) -> u32 {
        self.upstream_calls.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn upstream_call_count(&self) -> u32 {
        self.upstream_calls.load(Ordering::Relaxed)
    }
    pub(super) fn policy(
        &self,
        provider: &KiroProvider,
        model: Option<&str>,
        group: Option<&str>,
    ) -> anyhow::Result<Option<EnterprisePolicy>> {
        if let Some(policy) = self.policy.get() {
            return Ok(policy.clone());
        }
        let enabled = provider.token_manager.enterprise_special_handling_enabled();
        let policy = if enabled
            && provider
                .token_manager
                .has_available_enterprise(model, group, &HashSet::new())
        {
            let settings = provider.token_manager.get_enterprise_retry_settings();
            settings.validate()?;
            let total = Duration::from_millis(settings.total_timeout_ms);
            let reserve = Duration::from_millis((settings.total_timeout_ms / 3).min(5_000));
            let now = Deadline::now();
            let bucket_cap = provider.token_manager.max_bucket_attempts_per_request();
            let configured = provider.token_manager.enterprise_max_retries().min(256) as usize;
            Some(EnterprisePolicy {
                default_endpoint: provider.token_manager.get_enterprise_default_endpoint(),
                settings,
                max_calls: if bucket_cap == 0 {
                    configured
                } else {
                    configured.min(bucket_cap.saturating_add(1))
                },
                deadline: now + total,
                enterprise_deadline: now + total - reserve,
            })
        } else {
            None
        };
        // 初次未进入专项的请求保持个人路径，容量恢复不能让它绕过专项进入企业号。
        if enabled && policy.is_none() {
            self.finished.store(true, Ordering::Relaxed);
        }
        let _ = self.policy.set(policy);
        Ok(self.policy.get().expect("policy initialized").clone())
    }

    fn take_prepared_call(&self, expected: usize, max_calls: usize) -> Option<usize> {
        if expected >= max_calls {
            return None;
        }
        self.calls
            .compare_exchange(
                expected as u32,
                expected as u32 + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .ok()
            .map(|n| n as usize)
    }

    pub(super) fn clear_accepted(&self) {
        self.accepted.lock().take();
    }

    /// 由业务响应最终确认完成；首事件到达本身不增加账号成功数。
    pub fn complete(&self, success: bool) {
        if let Some((manager, id)) = self.accepted.lock().take() {
            if success {
                manager.report_success(id);
            }
        }
    }
}

pub(super) enum EnterprisePhaseResult {
    Accepted(KiroCallResult),
    Failover(anyhow::Error),
    Rejected(anyhow::Error),
}

#[derive(Default)]
struct FirstEvents {
    content: bool,
    terminal: bool,
}

fn inspect_events(decoder: &mut EventStreamDecoder, chunk: &[u8]) -> anyhow::Result<FirstEvents> {
    decoder.feed(chunk)?;
    let mut found = FirstEvents::default();
    for frame in decoder.decode_iter() {
        match Event::from_frame(frame?)? {
            Event::AssistantResponse(event) => found.content |= !event.content.is_empty(),
            Event::ReasoningContent(event) => {
                found.content |= event.text.as_deref().is_some_and(|s| !s.is_empty())
                    || event
                        .redacted_content
                        .as_deref()
                        .is_some_and(|s| !s.is_empty())
            }
            Event::ToolUse(event) => {
                found.content |= !event.name.is_empty() && !event.tool_use_id.is_empty()
            }
            Event::Error {
                error_code,
                error_message,
            } => anyhow::bail!("{error_code}: {error_message}"),
            Event::Exception {
                exception_type,
                message,
            } if exception_type != "ContentLengthExceededException" => {
                anyhow::bail!("{exception_type}: {message}")
            }
            // 显式终止由 handler 保留既有语义；不得把内容过滤当作瞬态再打一轮。
            Event::Metadata(event)
                if event.stop_reason.eq_ignore_ascii_case("CONTENT_FILTERED") =>
            {
                found.terminal = true
            }
            Event::ContextUsage(event) if event.context_usage_percentage >= 100.0 => {
                found.terminal = true
            }
            _ => {}
        }
    }
    Ok(found)
}

async fn first_effective_event(
    mut response: reqwest::Response,
    credential_id: u64,
    deadline: Deadline,
    sink: Option<&dyn TraceSink>,
) -> anyhow::Result<KiroCallResult> {
    let mut prefix = bytes::BytesMut::new();
    let mut decoder = EventStreamDecoder::new();
    let mut terminal_seen = false;
    loop {
        let chunk = match timeout_at(deadline, response.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) | Err(_) if terminal_seen => {
                // 元数据后可能还有拒绝正文：先在原截止时间内等待，只有确认EOF或时间到才封口。
                return Ok(KiroCallResult {
                    response,
                    credential_id,
                    body_prefix: Some(prefix.freeze()),
                    in_flight: None,
                    terminal_prefix: true,
                });
            }
            Ok(Ok(None)) => anyhow::bail!("enterprise_first_event_empty"),
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => anyhow::bail!("enterprise_first_event_timeout"),
        };
        if !chunk.is_empty() {
            if let Some(sink) = sink {
                sink.on_upstream_first_byte();
            }
        }
        anyhow::ensure!(
            prefix.len().saturating_add(chunk.len()) <= MAX_PREFIX_BYTES,
            "enterprise_first_event_buffer_limit"
        );
        prefix.extend_from_slice(&chunk);
        let found = inspect_events(&mut decoder, &chunk)?;
        terminal_seen |= found.terminal;
        if found.content {
            return Ok(KiroCallResult {
                response,
                credential_id,
                body_prefix: Some(prefix.freeze()),
                in_flight: None,
                terminal_prefix: false,
            });
        }
    }
}

async fn error_body(mut response: reqwest::Response, deadline: Deadline) -> anyhow::Result<String> {
    let mut body = Vec::new();
    loop {
        let chunk = match timeout_at(deadline, response.chunk()).await {
            Ok(Ok(Some(chunk))) => chunk,
            Ok(Ok(None)) => break,
            other => {
                let text = String::from_utf8_lossy(&body);
                if !body.is_empty()
                    && (serde_json::from_slice::<serde_json::Value>(&body).is_ok()
                        || (!text.trim_start().starts_with('{')
                            && !text.trim_start().starts_with('[')))
                {
                    return Ok(text.into_owned());
                }
                return Err(match other {
                    Ok(Err(error)) => error.into(),
                    _ => anyhow::anyhow!("enterprise_error_body_timeout"),
                });
            }
        };
        if body.len().saturating_add(chunk.len()) > MAX_ERROR_BYTES {
            body.extend_from_slice(&chunk[..MAX_ERROR_BYTES.saturating_sub(body.len())]);
            let text = String::from_utf8_lossy(&body);
            let plain = !text.trim_start().starts_with('{') && !text.trim_start().starts_with('[');
            if serde_json::from_slice::<serde_json::Value>(&body).is_ok()
                || (plain
                    && (crate::kiro::endpoint::default_is_monthly_request_limit(&text)
                        || crate::kiro::endpoint::default_is_account_throttled(&text)
                        || crate::kiro::endpoint::default_is_bearer_token_invalid(&text)))
            {
                return Ok(text.into_owned());
            }
            anyhow::bail!("enterprise_error_body_limit");
        }
        body.extend_from_slice(&chunk);
        if serde_json::from_slice::<serde_json::Value>(&body).is_ok() {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

pub(super) async fn mcp_success(
    mut response: reqwest::Response,
    credential_id: u64,
    deadline: Deadline,
) -> anyhow::Result<KiroCallResult> {
    let mut body = bytes::BytesMut::new();
    while let Some(chunk) = timeout_at(deadline, response.chunk())
        .await
        .map_err(|_| anyhow::anyhow!("enterprise_mcp_body_timeout"))??
    {
        anyhow::ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_PREFIX_BYTES,
            "enterprise_mcp_body_limit"
        );
        body.extend_from_slice(&chunk);
    }
    let value: crate::anthropic::McpResponse = serde_json::from_slice(&body)?;
    if let Some(error) = value.error {
        anyhow::bail!(
            "MCP response error {:?}: {}",
            error.code,
            error.message.unwrap_or_default()
        );
    }
    let result = value
        .result
        .ok_or_else(|| anyhow::anyhow!("MCP result missing"))?;
    if result.is_error {
        anyhow::bail!(
            "MCP tool error: {}",
            result
                .content
                .iter()
                .map(|content| content.text.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    Ok(KiroCallResult {
        response,
        credential_id,
        body_prefix: Some(body.freeze()),
        in_flight: None,
        terminal_prefix: true,
    })
}

impl KiroProvider {
    pub(super) async fn run_enterprise_phase(
        &self,
        ctx: &CallContext,
        request_body: &str,
        sink: Option<&dyn TraceSink>,
        policy: &EnterprisePolicy,
        control: &EnterpriseRequestControl,
        mcp: bool,
    ) -> EnterprisePhaseResult {
        let mut ctx = ctx.clone();
        let mut refreshed_token = false;
        let mut last_error = anyhow::anyhow!("enterprise_retry_budget_exhausted");
        let mut has_attempt_failure = false;
        let mut nodes = policy.settings.endpoints.clone();
        if mcp {
            nodes = vec![
                self.endpoint_for(&ctx.credentials, None)
                    .map(|e| e.name().to_string())
                    .unwrap_or_else(|_| "ide".into()),
            ];
        }
        let default = policy.default_endpoint.clone();
        if let Some(index) = nodes.iter().position(|node| node == &default) {
            nodes.rotate_left(index);
        }
        let offset = self.enterprise_rr_seq.fetch_add(1, Ordering::Relaxed) as usize % nodes.len();
        nodes.rotate_left(offset);
        let proxies = self.proxy_candidates_for(ctx.id, &ctx.credentials);
        if proxies.is_empty() {
            return EnterprisePhaseResult::Failover(anyhow::anyhow!(
                "no available proxy candidates"
            ));
        }
        let mut proxy_index = 0usize;
        let machine =
            machine_id::generate_from_credentials(&ctx.credentials, self.token_manager.config());
        let mut node_index = 0usize;
        let mut preparation_failures = 0usize;
        loop {
            if Deadline::now() >= policy.enterprise_deadline {
                break;
            }
            let expected_sequence = control.calls.load(Ordering::Relaxed) as usize;
            if expected_sequence >= policy.max_calls {
                break;
            }
            let name = &nodes[node_index % nodes.len()];
            let Some(endpoint) = self.endpoints.get(name).cloned() else {
                return EnterprisePhaseResult::Rejected(anyhow::anyhow!("企业端点未注册: {name}"));
            };
            let proxy = proxies[proxy_index % proxies.len()].clone();
            // 转换正文、诊断回调、client 缓存锁及 request.build 均可耗时，必须放在发送门之前。
            let prepared: anyhow::Result<_> = if mcp {
                let rctx = RequestContext {
                    credentials: &ctx.credentials,
                    token: &ctx.token,
                    machine_id: &machine,
                    config: self.token_manager.config(),
                    request_attempt: expected_sequence as u32 + 1,
                    request_attempt_max: policy.max_calls as u32,
                };
                (|| {
                    let client = self.client_for_proxy(proxy.clone())?;
                    let base = client
                        .post(endpoint.mcp_url(&rctx))
                        .body(endpoint.transform_mcp_body(request_body, &rctx))
                        .header("content-type", endpoint.content_type())
                        .header("Connection", "close");
                    let request = endpoint.decorate_mcp(base, &rctx).build()?;
                    Ok((client, request))
                })()
            } else {
                self.prepare_api_request(
                    &endpoint,
                    &ctx,
                    &machine,
                    self.token_manager.config(),
                    request_body,
                    proxy.clone(),
                    sink,
                    expected_sequence as u32,
                    expected_sequence as u32 + 1,
                    policy.max_calls as u32,
                )
            };
            let (client, request) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    last_error = error;
                    has_attempt_failure = true;
                    self.report_proxy_failure(ctx.id, proxy.as_ref());
                    proxy_index += 1;
                    preparation_failures += 1;
                    if preparation_failures >= proxies.len() {
                        break;
                    }
                    continue;
                }
            };
            let _proxy_permit = self.proxy_in_flight_guard(proxy.as_ref());
            let header_timeout_secs = self.stream_idle_timeout_secs();
            let header_timeout =
                (!mcp && header_timeout_secs > 0).then(|| Duration::from_secs(header_timeout_secs));
            let (sequence, send_stamp) = match self
                .token_manager
                .wait_enterprise_send(ctx.id, policy.enterprise_deadline, || {
                    let sequence = control.take_prepared_call(expected_sequence, policy.max_calls);
                    if sequence.is_some() && proxy.is_none() {
                        self.token_manager.note_direct_upstream(ctx.id);
                    }
                    sequence
                })
                .await
            {
                Ok(Some(send)) => send,
                // 同一入站 control 被并发推进：重建正确 attempt 头，不预约账号时隙。
                Ok(None) => continue,
                Err(error) => {
                    // 等待没有产生新的上游结果，不能改写本phase最近一次具体失败。
                    if has_attempt_failure
                        && error
                            .downcast_ref::<crate::kiro::token_manager::EnterpriseSendWaitTimeout>()
                            .is_some()
                    {
                        break;
                    }
                    last_error = if error
                        .downcast_ref::<crate::kiro::token_manager::EnterpriseSendWaitTimeout>()
                        .is_some_and(|wait| wait.rate_limited)
                    {
                        anyhow::Error::new(EnterpriseRateLimitError).context(error)
                    } else {
                        error
                    };
                    break;
                }
            };
            node_index += 1;
            let started = Instant::now();
            let deadline = (Deadline::now()
                + Duration::from_millis(policy.settings.first_event_timeout_ms))
            .min(policy.enterprise_deadline);
            tracing::info!(credential_id = ctx.id, upstream_call_seq = sequence + 1, max_calls = policy.max_calls, endpoint = %name, "企业号有界发送");
            let upstream_call_seq = control.note_upstream_send();
            tracing::info!(upstream_call_seq, credential_id = ctx.id, endpoint = %name, mcp, "实际发送上游请求");
            let send = await_response_headers(client.execute(request), header_timeout);
            let response = timeout_at(deadline, send).await;
            let response = match response {
                Ok(Ok(response)) => response,
                other => {
                    has_attempt_failure = true;
                    last_error = match other {
                        Ok(Err(error)) => error,
                        _ => anyhow::anyhow!("enterprise_response_header_timeout"),
                    };
                    if !mcp {
                        if let Some(sink) = sink {
                            sink.on_diagnostic(TraceDiagnosticEvent::NetworkError {
                                attempt: sequence as u32,
                                credential_id: ctx.id,
                                endpoint: endpoint.name(),
                                message: &last_error.to_string(),
                            });
                        }
                    }
                    self.report_proxy_failure(ctx.id, proxy.as_ref());
                    proxy_index += 1;
                    Self::emit_attempt(
                        sink,
                        sequence,
                        ctx.id,
                        name,
                        None,
                        outcome::NETWORK_ERROR,
                        Some(&last_error.to_string()),
                        started,
                    );
                    continue;
                }
            };
            let status = response.status();
            let mut policy_for_header = RetryPolicy::preset(RetryMode::Polite);
            policy_for_header.respect_retry_after = true;
            let retry_after = Self::retry_after_delay(response.headers(), &policy_for_header);
            let response_stamp = self.token_manager.report_enterprise_response_headers(
                ctx.id,
                status.as_u16(),
                retry_after,
            );
            let mut error_body_read = true;
            let body = if status.is_success() {
                let accepted = if mcp {
                    mcp_success(response, ctx.id, deadline).await
                } else {
                    first_effective_event(response, ctx.id, deadline, sink).await
                };
                match accepted {
                    Ok(result) => {
                        if mcp || !result.terminal_prefix {
                            self.token_manager
                                .report_enterprise_effective_response(ctx.id, send_stamp);
                        }
                        Self::emit_attempt(
                            sink,
                            sequence,
                            ctx.id,
                            name,
                            Some(status.as_u16()),
                            outcome::SUCCESS,
                            None,
                            started,
                        );
                        *control.accepted.lock() = Some((self.token_manager.clone(), ctx.id));
                        self.report_proxy_success(ctx.id, proxy.as_ref());
                        return EnterprisePhaseResult::Accepted(result);
                    }
                    Err(error) => error.to_string(),
                }
            } else {
                if status.as_u16() == 429 {
                    self.token_manager.record_rate_limit_hit(ctx.id);
                }
                match error_body(
                    response,
                    deadline.min(Deadline::now() + Duration::from_secs(2)),
                )
                .await
                {
                    Ok(body) => body,
                    Err(error) => {
                        error_body_read = false;
                        error.to_string()
                    }
                }
            };
            if let Some(sink) = sink {
                sink.on_diagnostic(TraceDiagnosticEvent::UpstreamResponse {
                    attempt: sequence as u32,
                    credential_id: ctx.id,
                    endpoint: endpoint.name(),
                    status: status.as_u16(),
                    body: &body,
                });
            }
            let quota = endpoint.is_monthly_request_limit(&body);
            let throttled = endpoint.is_account_throttled(&body);
            let invalid_token = endpoint.is_bearer_token_invalid(&body)
                || body.starts_with("AuthenticationException:")
                || body.starts_with("InvalidTokenException:");
            let auth = matches!(status.as_u16(), 401 | 403) || invalid_token;
            let terminal = is_terminal_fallback_response(endpoint.as_ref(), status, &body)
                || (status.is_client_error()
                    && !matches!(status.as_u16(), 401 | 403 | 408 | 429)
                    && !quota);
            let ordinary_rate_limit = status.as_u16() == 429
                && error_body_read
                && !quota
                && !throttled
                && !auth
                && !terminal;
            if let Some(stamp) = response_stamp {
                self.token_manager
                    .classify_enterprise_429(ctx.id, stamp, ordinary_rate_limit);
            }
            Self::emit_attempt(
                sink,
                sequence,
                ctx.id,
                name,
                Some(status.as_u16()),
                if quota {
                    outcome::QUOTA_EXHAUSTED
                } else if throttled {
                    outcome::ACCOUNT_THROTTLED
                } else if terminal {
                    request_error_outcome(&body)
                } else {
                    outcome::TRANSIENT
                },
                Some(&body),
                started,
            );
            last_error = anyhow::anyhow!("enterprise upstream {}: {}", status, body);
            has_attempt_failure = true;
            if ordinary_rate_limit {
                last_error = anyhow::Error::new(EnterpriseRateLimitError).context(last_error);
            }
            if quota {
                self.token_manager.report_quota_exhausted(ctx.id);
                break;
            }
            if throttled {
                if self.token_manager.get_account_throttle_failover() {
                    self.token_manager.report_account_throttled(
                        ctx.id,
                        Duration::from_secs(
                            self.token_manager
                                .get_account_throttle_cooldown_secs()
                                .max(1),
                        ),
                    );
                }
                break;
            }
            if auth {
                if invalid_token && !refreshed_token && !ctx.credentials.is_api_key_credential() {
                    refreshed_token = true;
                    if self
                        .token_manager
                        .force_refresh_token_for(ctx.id)
                        .await
                        .is_ok()
                    {
                        if let Ok(updated) = self.token_manager.reload_retry_context(ctx.id).await {
                            ctx = updated;
                            continue;
                        }
                    }
                }
                self.handle_auth_failure(
                    ctx.id,
                    if status.is_success() {
                        401
                    } else {
                        status.as_u16()
                    },
                    &body,
                    proxy.as_ref(),
                );
                break;
            }
            if terminal {
                return EnterprisePhaseResult::Rejected(last_error);
            }
            if should_try_next_proxy(status) {
                self.report_proxy_failure(ctx.id, proxy.as_ref());
                proxy_index += 1;
            }
        }
        control.finished.store(true, Ordering::Relaxed);
        EnterprisePhaseResult::Failover(last_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enterprise_prepared_attempt_sequence_must_match_without_consuming_extra_budget() {
        let control = EnterpriseRequestControl::default();
        assert_eq!(control.take_prepared_call(0, 2), Some(0));
        assert_eq!(control.take_prepared_call(0, 2), None);
        assert_eq!(control.calls.load(Ordering::Relaxed), 1);
        assert_eq!(control.take_prepared_call(1, 2), Some(1));
        assert_eq!(control.take_prepared_call(2, 2), None);
        assert_eq!(control.calls.load(Ordering::Relaxed), 2);
    }
}
