use serde::{Deserialize, Serialize};

use crate::image_resize::{ResizeTarget, shrink_image_with_target};
use crate::kiro::model::requests::{
    conversation::{KiroImage, Message},
    kiro::KiroRequest,
};

const EMPTY_RESPONSE_TOOL_RESULT_MAX_CHARS: usize = 512;
const EMPTY_RESPONSE_TOOL_RESULT_TRUNCATION_NOTICE: &str =
    "\n[Tool result truncated during empty-response recovery]";
const EMPTY_RESPONSE_MIN_HISTORY_IMAGES: usize = 3;
const EMPTY_RESPONSE_OMITTED_IMAGE_NOTICE: &str = "[Historical image omitted during recovery]";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImageBudgetPolicy {
    pub enabled: bool,
    pub total_base64_budget_bytes: usize,
    pub hard_base64_limit_bytes: usize,
    pub history_max_dimension: u32,
    pub history_jpeg_quality: u8,
    pub retry_history_max_dimension: u32,
    pub retry_history_jpeg_quality: u8,
}

impl Default for ImageBudgetPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            total_base64_budget_bytes: 819_200,
            hard_base64_limit_bytes: 8 * 1024 * 1024,
            history_max_dimension: 1_280,
            history_jpeg_quality: 72,
            retry_history_max_dimension: 960,
            retry_history_jpeg_quality: 60,
        }
    }
}

impl ImageBudgetPolicy {
    pub fn validate(self) -> Result<Self, ImageBudgetError> {
        if !(256 * 1024..=32 * 1024 * 1024).contains(&self.total_base64_budget_bytes) {
            return Err(ImageBudgetError::InvalidPolicy(
                "totalBase64BudgetBytes 必须在 256 KiB–32 MiB 之间".to_string(),
            ));
        }
        if !(256 * 1024..=32 * 1024 * 1024).contains(&self.hard_base64_limit_bytes) {
            return Err(ImageBudgetError::InvalidPolicy(
                "hardBase64LimitBytes 必须在 256 KiB–32 MiB 之间".to_string(),
            ));
        }
        if self.total_base64_budget_bytes > self.hard_base64_limit_bytes {
            return Err(ImageBudgetError::InvalidPolicy(
                "totalBase64BudgetBytes 不能大于 hardBase64LimitBytes".to_string(),
            ));
        }
        if !(640..=4_096).contains(&self.history_max_dimension) {
            return Err(ImageBudgetError::InvalidPolicy(
                "historyMaxDimension 必须在 640–4096 之间".to_string(),
            ));
        }
        if !(40..=95).contains(&self.history_jpeg_quality) {
            return Err(ImageBudgetError::InvalidPolicy(
                "historyJpegQuality 必须在 40–95 之间".to_string(),
            ));
        }
        if !(480..=self.history_max_dimension).contains(&self.retry_history_max_dimension) {
            return Err(ImageBudgetError::InvalidPolicy(
                "retryHistoryMaxDimension 必须在 480–historyMaxDimension 之间".to_string(),
            ));
        }
        if !(30..=self.history_jpeg_quality).contains(&self.retry_history_jpeg_quality) {
            return Err(ImageBudgetError::InvalidPolicy(
                "retryHistoryJpegQuality 必须在 30–historyJpegQuality 之间".to_string(),
            ));
        }
        Ok(self)
    }

    pub fn retry_variant(self) -> Self {
        Self {
            history_max_dimension: self.retry_history_max_dimension,
            history_jpeg_quality: self.retry_history_jpeg_quality,
            ..self
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImageBudgetStats {
    pub image_count: usize,
    pub history_image_count: usize,
    pub current_image_count: usize,
    pub before_base64_bytes: usize,
    pub after_base64_bytes: usize,
    pub resized_history_images: usize,
    pub unshrinkable_history_images: usize,
    pub resized_current_images: usize,
    pub unshrinkable_current_images: usize,
    pub omitted_history_images: usize,
    pub truncated_tool_results: usize,
}

#[derive(Debug, Clone)]
pub struct PreparedKiroBodies {
    pub primary_body: String,
    pub threshold_retry_body: Option<String>,
    pub primary_stats: ImageBudgetStats,
    pub retry_stats: Option<ImageBudgetStats>,
}

#[derive(Debug, thiserror::Error)]
pub enum ImageBudgetError {
    #[error("图片预算配置无效: {0}")]
    InvalidPolicy(String),
    #[error(
        "图片总量在历史图片压缩后仍超过硬上限: count={count}, history={history_count}, current={current_count}, before={before} bytes, after={after} bytes, soft={soft_limit} bytes, hard={hard_limit} bytes"
    )]
    Exceeded {
        count: usize,
        history_count: usize,
        current_count: usize,
        before: usize,
        after: usize,
        soft_limit: usize,
        hard_limit: usize,
    },
    #[error("Kiro 请求序列化失败: {0}")]
    Serialization(String),
}

pub fn count_images(request: &KiroRequest) -> usize {
    let current = request
        .conversation_state
        .current_message
        .user_input_message
        .images
        .len();
    current
        + request
            .conversation_state
            .history
            .iter()
            .map(|message| match message {
                Message::User(user) => user.user_input_message.images.len(),
                Message::Assistant(_) => 0,
            })
            .sum::<usize>()
}

fn collect_stats(request: &KiroRequest) -> ImageBudgetStats {
    let current_images = &request
        .conversation_state
        .current_message
        .user_input_message
        .images;
    let current_image_count = current_images.len();
    let current_bytes = current_images
        .iter()
        .map(|image| image.source.bytes.len())
        .sum::<usize>();
    let mut history_image_count = 0;
    let mut history_bytes = 0;
    for message in &request.conversation_state.history {
        if let Message::User(user) = message {
            history_image_count += user.user_input_message.images.len();
            history_bytes += user
                .user_input_message
                .images
                .iter()
                .map(|image| image.source.bytes.len())
                .sum::<usize>();
        }
    }
    ImageBudgetStats {
        image_count: current_image_count + history_image_count,
        history_image_count,
        current_image_count,
        before_base64_bytes: current_bytes + history_bytes,
        after_base64_bytes: current_bytes + history_bytes,
        ..ImageBudgetStats::default()
    }
}

pub fn apply_image_budget(
    request: &mut KiroRequest,
    policy: ImageBudgetPolicy,
) -> Result<ImageBudgetStats, ImageBudgetError> {
    let policy = policy.validate()?;
    let stats = apply_image_budget_inner(request, policy, false);
    if policy.enabled && stats.after_base64_bytes > policy.hard_base64_limit_bytes {
        return Err(ImageBudgetError::Exceeded {
            count: stats.image_count,
            history_count: stats.history_image_count,
            current_count: stats.current_image_count,
            before: stats.before_base64_bytes,
            after: stats.after_base64_bytes,
            soft_limit: policy.total_base64_budget_bytes,
            hard_limit: policy.hard_base64_limit_bytes,
        });
    }
    Ok(stats)
}

fn apply_image_budget_inner(
    request: &mut KiroRequest,
    policy: ImageBudgetPolicy,
    force_history_reencode: bool,
) -> ImageBudgetStats {
    let mut stats = collect_stats(request);
    if !policy.enabled
        || (!force_history_reencode && stats.after_base64_bytes <= policy.total_base64_budget_bytes)
    {
        return stats;
    }

    for message in &mut request.conversation_state.history {
        let Message::User(user) = message else {
            continue;
        };
        for image in &mut user.user_input_message.images {
            if !force_history_reencode
                && stats.after_base64_bytes <= policy.total_base64_budget_bytes
            {
                break;
            }
            if shrink_image_if_smaller(image, policy, &mut stats) {
                stats.resized_history_images += 1;
            } else {
                stats.unshrinkable_history_images += 1;
            }
        }
    }

    stats
}

fn shrink_image_if_smaller(
    image: &mut KiroImage,
    policy: ImageBudgetPolicy,
    stats: &mut ImageBudgetStats,
) -> bool {
    let original_len = image.source.bytes.len();
    let Ok(processed) = shrink_image_with_target(
        &image.format,
        &image.source.bytes,
        ResizeTarget {
            max_long_side: policy.history_max_dimension,
            jpeg_quality: policy.history_jpeg_quality,
        },
    ) else {
        return false;
    };
    if processed.data_base64.len() >= original_len {
        return false;
    }

    stats.after_base64_bytes = stats
        .after_base64_bytes
        .saturating_sub(original_len)
        .saturating_add(processed.data_base64.len());
    image.format = processed.format;
    image.source.bytes = processed.data_base64;
    true
}

fn apply_empty_response_recovery(
    request: &mut KiroRequest,
    policy: ImageBudgetPolicy,
) -> ImageBudgetStats {
    let mut stats = collect_stats(request);
    if policy.enabled {
        let retry_policy = policy.retry_variant();
        for message in &mut request.conversation_state.history {
            let Message::User(user) = message else {
                continue;
            };
            for image in &mut user.user_input_message.images {
                if shrink_image_if_smaller(image, retry_policy, &mut stats) {
                    stats.resized_history_images += 1;
                } else {
                    stats.unshrinkable_history_images += 1;
                }
            }
        }
        for image in &mut request
            .conversation_state
            .current_message
            .user_input_message
            .images
        {
            if shrink_image_if_smaller(image, retry_policy, &mut stats) {
                stats.resized_current_images += 1;
            } else {
                stats.unshrinkable_current_images += 1;
            }
        }

        let mut images_left_to_omit = stats
            .history_image_count
            .saturating_sub(EMPTY_RESPONSE_MIN_HISTORY_IMAGES);
        for message in &mut request.conversation_state.history {
            if stats.after_base64_bytes <= policy.total_base64_budget_bytes
                || images_left_to_omit == 0
            {
                break;
            }
            let Message::User(user) = message else {
                continue;
            };
            let mut omitted_from_message = false;
            while stats.after_base64_bytes > policy.total_base64_budget_bytes
                && images_left_to_omit > 0
                && !user.user_input_message.images.is_empty()
            {
                let omitted = user.user_input_message.images.remove(0);
                stats.after_base64_bytes = stats
                    .after_base64_bytes
                    .saturating_sub(omitted.source.bytes.len());
                stats.image_count = stats.image_count.saturating_sub(1);
                stats.history_image_count = stats.history_image_count.saturating_sub(1);
                stats.omitted_history_images += 1;
                images_left_to_omit -= 1;
                omitted_from_message = true;
            }
            if omitted_from_message
                && user.user_input_message.images.is_empty()
                && user.user_input_message.content.trim().is_empty()
                && user
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                    .is_empty()
            {
                user.user_input_message.content = EMPTY_RESPONSE_OMITTED_IMAGE_NOTICE.to_string();
            }
        }
    }

    for result in &mut request
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .tool_results
    {
        let mut truncated_result = false;
        for content in &mut result.content {
            let Some(serde_json::Value::String(text)) = content.get_mut("text") else {
                continue;
            };
            if text.chars().count() <= EMPTY_RESPONSE_TOOL_RESULT_MAX_CHARS {
                continue;
            }

            let mut truncated = text
                .chars()
                .take(EMPTY_RESPONSE_TOOL_RESULT_MAX_CHARS)
                .collect::<String>();
            truncated.push_str(EMPTY_RESPONSE_TOOL_RESULT_TRUNCATION_NOTICE);
            *text = truncated;
            truncated_result = true;
        }
        if truncated_result {
            stats.truncated_tool_results += 1;
        }
    }

    stats
}

pub fn prepare_empty_response_retry_body(
    primary_body: &str,
    policy: ImageBudgetPolicy,
) -> Result<Option<(String, ImageBudgetStats)>, ImageBudgetError> {
    let policy = policy.validate()?;
    let mut recovery: KiroRequest = serde_json::from_str(primary_body)
        .map_err(|error| ImageBudgetError::Serialization(error.to_string()))?;
    let recovery_stats = apply_empty_response_recovery(&mut recovery, policy);
    let recovery_body = serde_json::to_string(&recovery)
        .map_err(|error| ImageBudgetError::Serialization(error.to_string()))?;
    let changed = recovery_body != primary_body;
    Ok(changed.then_some((recovery_body, recovery_stats)))
}

/// 从同一个原始请求生成普通请求体、阈值重试体和空响应恢复体。
///
/// 阈值重试体只进一步压缩历史图片；空响应恢复体会压缩历史及当前图片，并限制当前轮
/// 工具结果文本。两种恢复体都只在完整 JSON 请求体实际发生变化时提供。
pub fn prepare_kiro_bodies(
    request: &KiroRequest,
    policy: ImageBudgetPolicy,
) -> Result<PreparedKiroBodies, ImageBudgetError> {
    let policy = policy.validate()?;

    if !policy.enabled {
        let stats = collect_stats(request);
        let body = serde_json::to_string(request)
            .map_err(|error| ImageBudgetError::Serialization(error.to_string()))?;
        return Ok(PreparedKiroBodies {
            primary_body: body,
            threshold_retry_body: None,
            primary_stats: stats,
            retry_stats: None,
        });
    }

    let mut primary = request.clone();
    let primary_stats = apply_image_budget_inner(&mut primary, policy, false);
    let primary_body = serde_json::to_string(&primary)
        .map_err(|error| ImageBudgetError::Serialization(error.to_string()))?;

    let mut retry = request.clone();
    let retry_stats = apply_image_budget_inner(&mut retry, policy.retry_variant(), true);
    let retry_body = serde_json::to_string(&retry)
        .map_err(|error| ImageBudgetError::Serialization(error.to_string()))?;
    let has_useful_retry = retry_stats.history_image_count > 0
        && retry_stats.resized_history_images > 0
        && retry_body.len() < primary_body.len();

    if primary_stats.after_base64_bytes <= policy.hard_base64_limit_bytes {
        let retry_fits = retry_stats.after_base64_bytes <= policy.hard_base64_limit_bytes;
        return Ok(PreparedKiroBodies {
            primary_body,
            threshold_retry_body: (has_useful_retry && retry_fits).then_some(retry_body),
            primary_stats,
            retry_stats: (has_useful_retry && retry_fits).then_some(retry_stats),
        });
    }
    if has_useful_retry && retry_stats.after_base64_bytes <= policy.hard_base64_limit_bytes {
        return Ok(PreparedKiroBodies {
            primary_body: retry_body,
            threshold_retry_body: None,
            primary_stats: retry_stats,
            retry_stats: None,
        });
    }

    let smallest = primary_stats
        .after_base64_bytes
        .min(retry_stats.after_base64_bytes);
    Err(ImageBudgetError::Exceeded {
        count: primary_stats.image_count,
        history_count: primary_stats.history_image_count,
        current_count: primary_stats.current_image_count,
        before: primary_stats.before_base64_bytes,
        after: smallest,
        soft_limit: policy.total_base64_budget_bytes,
        hard_limit: policy.hard_base64_limit_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::requests::{
        conversation::{
            ConversationState, CurrentMessage, HistoryUserMessage, KiroImage, Message,
            UserInputMessage, UserInputMessageContext,
        },
        kiro::KiroRequest,
        tool::ToolResult,
    };
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
    use image::{ImageFormat, Rgb, RgbImage};
    use std::io::Cursor;

    fn make_png(width: u32, height: u32) -> String {
        let mut image = RgbImage::new(width, height);
        let mut state = 0x1234_5678_u32;
        for y in 0..height {
            for x in 0..width {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                image.put_pixel(
                    x,
                    y,
                    Rgb([state as u8, (state >> 8) as u8, (state >> 16) as u8]),
                );
            }
        }
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        BASE64.encode(bytes)
    }

    fn request_with_images(history: Vec<String>, current: Vec<String>) -> KiroRequest {
        let history = history
            .into_iter()
            .enumerate()
            .map(|(index, data)| {
                let mut message = HistoryUserMessage::new(format!("history-{index}"), "model");
                message.user_input_message.images = vec![KiroImage::from_base64("png", data)];
                Message::User(message)
            })
            .collect();
        let current = current
            .into_iter()
            .map(|data| KiroImage::from_base64("png", data))
            .collect();
        KiroRequest {
            conversation_state: ConversationState::new("conv")
                .with_history(history)
                .with_current_message(CurrentMessage::new(
                    UserInputMessage::new("current", "model").with_images(current),
                )),
            profile_arn: None,
            additional_model_request_fields: None,
        }
    }

    fn request_with_current_tool_results(results: Vec<ToolResult>) -> KiroRequest {
        KiroRequest {
            conversation_state: ConversationState::new("conv").with_current_message(
                CurrentMessage::new(
                    UserInputMessage::new("current", "model")
                        .with_context(UserInputMessageContext::new().with_tool_results(results)),
                ),
            ),
            profile_arn: None,
            additional_model_request_fields: None,
        }
    }

    #[test]
    fn empty_response_retry_reencodes_current_images_without_removing_any() {
        let historical = make_png(640, 640);
        let current = make_png(640, 640);
        let request = request_with_images(vec![historical.clone()], vec![current.clone()]);
        let policy = ImageBudgetPolicy {
            total_base64_budget_bytes: 8 * 1024 * 1024,
            hard_base64_limit_bytes: 8 * 1024 * 1024,
            retry_history_max_dimension: 480,
            retry_history_jpeg_quality: 45,
            ..ImageBudgetPolicy::default()
        };
        let prepared = prepare_kiro_bodies(&request, policy).unwrap();

        let primary: KiroRequest = serde_json::from_str(&prepared.primary_body).unwrap();
        let (recovery_body, recovery_stats) =
            prepare_empty_response_retry_body(&prepared.primary_body, policy)
                .unwrap()
                .expect("current images should produce an empty-response recovery body");
        let recovery: KiroRequest = serde_json::from_str(&recovery_body).unwrap();

        assert_eq!(count_images(&recovery), count_images(&primary));
        assert_eq!(
            primary
                .conversation_state
                .current_message
                .user_input_message
                .images[0]
                .source
                .bytes,
            current
        );
        assert_ne!(
            recovery
                .conversation_state
                .current_message
                .user_input_message
                .images[0]
                .source
                .bytes,
            current
        );
        assert_ne!(
            match &recovery.conversation_state.history[0] {
                Message::User(message) => &message.user_input_message.images[0].source.bytes,
                Message::Assistant(_) => panic!("fixture must contain a historical user image"),
            },
            &historical
        );
        assert_eq!(recovery_stats.image_count, count_images(&recovery));
        assert!(recovery_stats.after_base64_bytes < recovery_stats.before_base64_bytes);
    }

    #[test]
    fn empty_response_retry_omits_oldest_history_images_but_keeps_three() {
        let history = (0..6)
            .map(|index| make_png(640 + index * 8, 640 + index * 8))
            .collect::<Vec<_>>();
        let current = make_png(704, 704);
        let mut request = request_with_images(history, vec![current]);
        for message in &mut request.conversation_state.history {
            let Message::User(message) = message else {
                continue;
            };
            message.user_input_message.content.clear();
        }
        let policy = ImageBudgetPolicy {
            total_base64_budget_bytes: 256 * 1024,
            hard_base64_limit_bytes: 8 * 1024 * 1024,
            retry_history_max_dimension: 480,
            retry_history_jpeg_quality: 30,
            ..ImageBudgetPolicy::default()
        };
        let prepared = prepare_kiro_bodies(&request, policy).unwrap();
        let (recovery_body, stats) =
            prepare_empty_response_retry_body(&prepared.primary_body, policy)
                .unwrap()
                .expect("oversized history should produce a recovery body");
        let recovery: KiroRequest = serde_json::from_str(&recovery_body).unwrap();

        assert_eq!(
            recovery
                .conversation_state
                .current_message
                .user_input_message
                .images
                .len(),
            1
        );
        let history_messages = &recovery.conversation_state.history;
        for message in &history_messages[..3] {
            let Message::User(message) = message else {
                panic!("fixture must contain historical user messages");
            };
            assert!(message.user_input_message.images.is_empty());
            assert_eq!(
                message.user_input_message.content,
                "[Historical image omitted during recovery]"
            );
        }
        for message in &history_messages[3..] {
            let Message::User(message) = message else {
                panic!("fixture must contain historical user messages");
            };
            assert_eq!(message.user_input_message.images.len(), 1);
        }
        assert_eq!(stats.history_image_count, 3);
        assert_eq!(stats.current_image_count, 1);
        assert_eq!(stats.omitted_history_images, 3);
    }

    #[test]
    fn empty_response_retry_truncates_long_unicode_tool_text_and_preserves_metadata() {
        let current_long_text = "界".repeat(520);
        let historical_long_text = "旧".repeat(520);
        let mut first_content = serde_json::Map::new();
        first_content.insert(
            "text".to_string(),
            serde_json::Value::String(current_long_text.clone()),
        );
        first_content.insert("kind".to_string(), serde_json::json!("stdout"));
        let request_results = vec![
            ToolResult {
                tool_use_id: "tool-long".to_string(),
                content: vec![first_content],
                status: Some("error".to_string()),
                is_error: true,
            },
            ToolResult::success("tool-short", "ok"),
        ];
        let mut request = request_with_current_tool_results(request_results);
        let mut historical = HistoryUserMessage::new("history", "model");
        historical.user_input_message.user_input_message_context = UserInputMessageContext::new()
            .with_tool_results(vec![ToolResult::success(
                "tool-history",
                historical_long_text.clone(),
            )]);
        request
            .conversation_state
            .history
            .push(Message::User(historical));
        let original_body = serde_json::to_string(&request).unwrap();

        let policy = ImageBudgetPolicy {
            enabled: false,
            ..ImageBudgetPolicy::default()
        };
        let prepared = prepare_kiro_bodies(&request, policy).unwrap();
        assert_eq!(prepared.primary_body, original_body);

        let (recovery_body, recovery_stats) =
            prepare_empty_response_retry_body(&prepared.primary_body, policy)
                .unwrap()
                .expect("long tool text must create a recovery body even when images are disabled");
        let recovery: KiroRequest = serde_json::from_str(&recovery_body).unwrap();
        assert_eq!(recovery_stats.truncated_tool_results, 1);
        let results = &recovery
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context
            .tool_results;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool_use_id, "tool-long");
        assert_eq!(results[0].status.as_deref(), Some("error"));
        assert!(results[0].is_error);
        assert_eq!(results[0].content.len(), 1);
        assert_eq!(results[0].content[0]["kind"], "stdout");
        assert_eq!(
            results[0].content[0]["text"].as_str().unwrap(),
            format!(
                "{}\n[Tool result truncated during empty-response recovery]",
                "界".repeat(512)
            )
        );
        assert_eq!(results[1].tool_use_id, "tool-short");
        assert_eq!(results[1].content[0]["text"], "ok");
        let historical_results = match &recovery.conversation_state.history[0] {
            Message::User(message) => {
                &message
                    .user_input_message
                    .user_input_message_context
                    .tool_results
            }
            Message::Assistant(_) => panic!("fixture must contain a historical user message"),
        };
        assert_eq!(
            historical_results[0].content[0]["text"],
            historical_long_text
        );
    }

    #[test]
    fn empty_response_retry_is_absent_when_short_tool_results_need_no_change() {
        let request = request_with_current_tool_results(vec![ToolResult::success(
            "tool-short",
            "short result",
        )]);

        let prepared = prepare_kiro_bodies(&request, ImageBudgetPolicy::default()).unwrap();

        assert!(
            prepare_empty_response_retry_body(&prepared.primary_body, ImageBudgetPolicy::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn threshold_retry_body_never_truncates_tool_results() {
        let historical = make_png(1_200, 1_200);
        let long_text = "x".repeat(600);
        let mut request = request_with_images(vec![historical], vec![]);
        request
            .conversation_state
            .current_message
            .user_input_message
            .user_input_message_context = UserInputMessageContext::new()
            .with_tool_results(vec![ToolResult::success("tool-long", long_text.clone())]);
        let prepared = prepare_kiro_bodies(
            &request,
            ImageBudgetPolicy {
                total_base64_budget_bytes: 8 * 1024 * 1024,
                hard_base64_limit_bytes: 8 * 1024 * 1024,
                retry_history_max_dimension: 480,
                retry_history_jpeg_quality: 45,
                ..ImageBudgetPolicy::default()
            },
        )
        .unwrap();

        let threshold: KiroRequest = serde_json::from_str(
            prepared
                .threshold_retry_body
                .as_deref()
                .expect("historical image should produce a threshold retry body"),
        )
        .unwrap();
        assert_eq!(
            threshold
                .conversation_state
                .current_message
                .user_input_message
                .user_input_message_context
                .tool_results[0]
                .content[0]["text"],
            long_text
        );
    }

    #[test]
    fn compresses_only_history_and_preserves_all_images() {
        let historical = make_png(1200, 1200);
        let current = make_png(900, 900);
        let mut request = request_with_images(vec![historical.clone()], vec![current.clone()]);
        let before_count = count_images(&request);

        let policy = ImageBudgetPolicy {
            total_base64_budget_bytes: current.len() + 500_000,
            hard_base64_limit_bytes: 8 * 1024 * 1024,
            history_max_dimension: 640,
            retry_history_max_dimension: 480,
            ..ImageBudgetPolicy::default()
        };
        let stats = apply_image_budget(&mut request, policy).unwrap();

        assert_eq!(count_images(&request), before_count);
        assert_eq!(
            request
                .conversation_state
                .current_message
                .user_input_message
                .images[0]
                .source
                .bytes,
            current
        );
        assert!(stats.after_base64_bytes <= policy.total_base64_budget_bytes);
        assert_eq!(stats.resized_history_images, 1);
    }

    #[test]
    fn impossible_current_only_budget_returns_typed_error_without_deleting() {
        let current = make_png(900, 900);
        let mut request = request_with_images(vec![], vec![current]);
        let before_count = count_images(&request);
        let policy = ImageBudgetPolicy {
            total_base64_budget_bytes: 256 * 1024,
            hard_base64_limit_bytes: 256 * 1024,
            ..ImageBudgetPolicy::default()
        };

        let error = apply_image_budget(&mut request, policy).unwrap_err();
        assert!(matches!(error, ImageBudgetError::Exceeded { .. }));
        assert_eq!(count_images(&request), before_count);
    }

    #[test]
    fn prepared_bodies_keep_current_images_and_offer_smaller_history_retry() {
        let historical = make_png(1200, 1200);
        let current = make_png(900, 900);
        let request = request_with_images(vec![historical], vec![current.clone()]);
        let policy = ImageBudgetPolicy {
            total_base64_budget_bytes: 8 * 1024 * 1024,
            hard_base64_limit_bytes: 8 * 1024 * 1024,
            retry_history_max_dimension: 480,
            retry_history_jpeg_quality: 55,
            ..ImageBudgetPolicy::default()
        };

        let prepared = prepare_kiro_bodies(&request, policy).unwrap();
        let retry_body = prepared
            .threshold_retry_body
            .as_ref()
            .expect("历史图片可进一步压缩时应生成阈值重试请求体");

        assert!(retry_body.len() < prepared.primary_body.len());
        let primary: KiroRequest = serde_json::from_str(&prepared.primary_body).unwrap();
        let retry: KiroRequest = serde_json::from_str(retry_body).unwrap();
        assert_eq!(
            primary
                .conversation_state
                .current_message
                .user_input_message
                .images[0]
                .source
                .bytes,
            current
        );
        assert_eq!(
            retry
                .conversation_state
                .current_message
                .user_input_message
                .images[0]
                .source
                .bytes,
            current
        );
        assert_eq!(count_images(&primary), count_images(&retry));
    }

    #[test]
    fn prepared_bodies_do_not_offer_retry_without_history_images() {
        let current = make_png(900, 900);
        let request = request_with_images(vec![], vec![current]);
        let prepared = prepare_kiro_bodies(
            &request,
            ImageBudgetPolicy {
                total_base64_budget_bytes: 8 * 1024 * 1024,
                hard_base64_limit_bytes: 8 * 1024 * 1024,
                ..ImageBudgetPolicy::default()
            },
        )
        .unwrap();

        assert!(prepared.threshold_retry_body.is_none());
    }

    #[test]
    fn normal_body_above_soft_target_but_below_hard_limit_is_allowed() {
        let historical = make_png(1_200, 1_200);
        let current = make_png(320, 320);
        let request = request_with_images(vec![historical], vec![current.clone()]);
        let policy = ImageBudgetPolicy {
            total_base64_budget_bytes: 800 * 1024,
            hard_base64_limit_bytes: 8 * 1024 * 1024,
            retry_history_max_dimension: 480,
            retry_history_jpeg_quality: 50,
            ..ImageBudgetPolicy::default()
        };

        let prepared = prepare_kiro_bodies(&request, policy).unwrap();
        assert!(
            prepared.primary_stats.after_base64_bytes > policy.total_base64_budget_bytes,
            "fixture must stay above the 800 KiB soft target"
        );
        assert!(prepared.primary_stats.after_base64_bytes <= policy.hard_base64_limit_bytes);
        let retry = prepared
            .threshold_retry_body
            .as_ref()
            .expect("history compression should provide a smaller threshold retry body");
        assert!(retry.len() < prepared.primary_body.len());

        let primary: KiroRequest = serde_json::from_str(&prepared.primary_body).unwrap();
        assert_eq!(
            primary
                .conversation_state
                .current_message
                .user_input_message
                .images[0]
                .source
                .bytes,
            current,
            "current-turn bytes must remain unchanged"
        );
    }

    #[test]
    fn aggressive_body_becomes_primary_when_normal_body_exceeds_hard_limit() {
        let historical = make_png(1_200, 1_200);
        let request = request_with_images(vec![historical], vec![]);
        let policy = ImageBudgetPolicy {
            total_base64_budget_bytes: 256 * 1024,
            hard_base64_limit_bytes: 512 * 1024,
            history_max_dimension: 1_280,
            history_jpeg_quality: 90,
            retry_history_max_dimension: 480,
            retry_history_jpeg_quality: 40,
            ..ImageBudgetPolicy::default()
        };

        let prepared = prepare_kiro_bodies(&request, policy).unwrap();
        assert!(prepared.primary_stats.after_base64_bytes <= policy.hard_base64_limit_bytes);
        assert_eq!(prepared.primary_stats.resized_history_images, 1);
        assert!(prepared.threshold_retry_body.is_none());
    }

    #[test]
    fn both_bodies_above_hard_limit_return_typed_error() {
        let current = make_png(900, 900);
        let request = request_with_images(vec![], vec![current]);
        let policy = ImageBudgetPolicy {
            total_base64_budget_bytes: 256 * 1024,
            hard_base64_limit_bytes: 256 * 1024,
            ..ImageBudgetPolicy::default()
        };

        let error = prepare_kiro_bodies(&request, policy).unwrap_err();
        assert!(matches!(
            error,
            ImageBudgetError::Exceeded {
                count: 1,
                history_count: 0,
                current_count: 1,
                before,
                after,
                soft_limit,
                hard_limit,
            } if soft_limit == 256 * 1024
                && hard_limit == 256 * 1024
                && before >= after
                && after > 256 * 1024
        ));
    }

    #[test]
    fn policy_requires_soft_not_above_hard_and_caps_hard_at_32_mib() {
        assert!(
            ImageBudgetPolicy {
                total_base64_budget_bytes: 2 * 1024 * 1024,
                hard_base64_limit_bytes: 1024 * 1024,
                ..ImageBudgetPolicy::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            ImageBudgetPolicy {
                hard_base64_limit_bytes: 32 * 1024 * 1024 + 1,
                ..ImageBudgetPolicy::default()
            }
            .validate()
            .is_err()
        );
    }
}
