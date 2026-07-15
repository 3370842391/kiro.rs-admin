//! Admin API 错误类型定义

use std::fmt;

use axum::http::StatusCode;

use super::types::AdminErrorResponse;

/// Admin 服务错误类型
#[derive(Debug)]
pub enum AdminServiceError {
    /// 认证流程中的结构化错误
    Auth {
        status: StatusCode,
        error_type: &'static str,
        code: &'static str,
        stage: &'static str,
        retryable: bool,
        message: String,
    },

    /// 凭据不存在
    NotFound { id: u64 },

    /// 上游服务调用失败（网络、API 错误等）
    UpstreamError(String),

    /// 内部状态错误
    InternalError(String),

    /// 凭据无效（验证失败）
    InvalidCredential(String),

    /// revision 或锁定字段冲突
    Conflict(String),

    /// 一次性预览不存在、过期或已消费
    Gone(String),
}

impl fmt::Display for AdminServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdminServiceError::Auth { message, .. } => write!(f, "{}", message),
            AdminServiceError::NotFound { id } => {
                write!(f, "凭据不存在: {}", id)
            }
            AdminServiceError::UpstreamError(msg) => write!(f, "上游服务错误: {}", msg),
            AdminServiceError::InternalError(msg) => write!(f, "内部错误: {}", msg),
            AdminServiceError::InvalidCredential(msg) => write!(f, "凭据无效: {}", msg),
            AdminServiceError::Conflict(msg) => write!(f, "状态冲突: {}", msg),
            AdminServiceError::Gone(msg) => write!(f, "资源已过期: {}", msg),
        }
    }
}

impl std::error::Error for AdminServiceError {}

impl AdminServiceError {
    pub fn auth(
        status: StatusCode,
        error_type: &'static str,
        code: &'static str,
        stage: &'static str,
        retryable: bool,
        message: impl Into<String>,
    ) -> Self {
        Self::Auth {
            status,
            error_type,
            code,
            stage,
            retryable,
            message: message.into(),
        }
    }

    /// 获取对应的 HTTP 状态码
    pub fn status_code(&self) -> StatusCode {
        match self {
            AdminServiceError::Auth { status, .. } => *status,
            AdminServiceError::NotFound { .. } => StatusCode::NOT_FOUND,
            AdminServiceError::UpstreamError(_) => StatusCode::BAD_GATEWAY,
            AdminServiceError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AdminServiceError::InvalidCredential(_) => StatusCode::BAD_REQUEST,
            AdminServiceError::Conflict(_) => StatusCode::CONFLICT,
            AdminServiceError::Gone(_) => StatusCode::GONE,
        }
    }

    /// 转换为 API 错误响应
    pub fn into_response(self) -> AdminErrorResponse {
        match &self {
            AdminServiceError::Auth {
                error_type,
                code,
                stage,
                retryable,
                message,
                ..
            } => AdminErrorResponse::structured(*error_type, message, *code, *stage, *retryable),
            AdminServiceError::NotFound { .. } => AdminErrorResponse::not_found(self.to_string()),
            AdminServiceError::UpstreamError(_) => AdminErrorResponse::api_error(self.to_string()),
            AdminServiceError::InternalError(_) => {
                AdminErrorResponse::internal_error(self.to_string())
            }
            AdminServiceError::InvalidCredential(_) => {
                AdminErrorResponse::invalid_request(self.to_string())
            }
            AdminServiceError::Conflict(_) | AdminServiceError::Gone(_) => {
                AdminErrorResponse::invalid_request(self.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_error_preserves_status_code_and_structured_fields() {
        let error = AdminServiceError::auth(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "state_mismatch",
            "social_callback",
            false,
            "OAuth state 不匹配",
        );
        assert_eq!(error.status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(error.to_string(), "OAuth state 不匹配");

        let value = serde_json::to_value(error.into_response()).unwrap();
        assert_eq!(value["error"]["type"], "invalid_request");
        assert_eq!(value["error"]["message"], "OAuth state 不匹配");
        assert_eq!(value["error"]["code"], "state_mismatch");
        assert_eq!(value["error"]["stage"], "social_callback");
        assert_eq!(value["error"]["retryable"], false);
    }
}
