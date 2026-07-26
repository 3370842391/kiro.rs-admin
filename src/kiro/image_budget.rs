use serde::{Deserialize, Serialize};

use crate::image_resize::{ResizeTarget, shrink_image_with_target};
use crate::kiro::model::requests::{
    conversation::{KiroImage, Message},
    kiro::KiroRequest,
    tool::ToolResult,
};

const EMPTY_RESPONSE_TOOL_RESULT_MAX_CHARS: usize = 512;
const EMPTY_RESPONSE_HISTORY_TOOL_RESULT_MAX_CHARS: usize = 1_024;
const EMPTY_RESPONSE_RECENT_HISTORY_TOOL_RESULTS_TO_KEEP: usize = 8;
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
    /// **尺寸硬上限（长边像素）**，与字节预算完全解耦：历史图和当前轮图都封顶到这里。
    ///
    /// 上游对多图请求另有像素约束（`image dimensions exceed max allowed size for
    /// many-image requests: 2000 pixels`），跟字节数无关。此前只有 `history_max_dimension`
    /// 且被字节预算 gate 住，当前轮的图从不压缩，单张超大图必然 400。默认 2000 对齐上游。
    #[serde(default = "default_hard_max_dimension")]
    pub hard_max_dimension: u32,
}

fn default_hard_max_dimension() -> u32 {
    2_000
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
            hard_max_dimension: default_hard_max_dimension(),
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
        if !(512..=4_096).contains(&self.hard_max_dimension) {
            return Err(ImageBudgetError::InvalidPolicy(
                "hardMaxDimension 必须在 512–4096 之间".to_string(),
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
    if !policy.enabled {
        return stats;
    }

    // 尺寸硬封顶：**先于**字节预算判定，且不受其 gate。上游的像素约束与字节数无关，
    // 总量没超时单张超大图同样会被 400；当前轮的图也必须覆盖（历史图走下面的字节预算路径）。
    cap_image_dimensions(request, policy, &mut stats);

    if !force_history_reencode && stats.after_base64_bytes <= policy.total_base64_budget_bytes {
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

/// 把历史图与当前轮图的长边一律封顶到 `policy.hard_max_dimension`。
///
/// 先用只读图片头的 `peek_dimensions` 判定，未超限不重编码（省 CPU）；头解析失败时保守跳过，
/// 不猜、不丢图。与字节预算无关，故不看 `total_base64_budget_bytes`。
fn cap_image_dimensions(
    request: &mut KiroRequest,
    policy: ImageBudgetPolicy,
    stats: &mut ImageBudgetStats,
) {
    let target = ResizeTarget {
        max_long_side: policy.hard_max_dimension,
        jpeg_quality: policy.history_jpeg_quality,
    };

    let mut cap_one = |image: &mut KiroImage, is_history: bool| {
        let Some((width, height)) =
            crate::image_resize::peek_dimensions(&image.format, &image.source.bytes)
        else {
            return;
        };
        if width.max(height) <= policy.hard_max_dimension {
            return;
        }
        let original_len = image.source.bytes.len();
        let Ok(processed) =
            shrink_image_with_target(&image.format, &image.source.bytes, target)
        else {
            if is_history {
                stats.unshrinkable_history_images += 1;
            } else {
                stats.unshrinkable_current_images += 1;
            }
            return;
        };
        stats.after_base64_bytes = stats
            .after_base64_bytes
            .saturating_sub(original_len)
            .saturating_add(processed.data_base64.len());
        image.format = processed.format;
        image.source.bytes = processed.data_base64;
        if is_history {
            stats.resized_history_images += 1;
        } else {
            stats.resized_current_images += 1;
        }
    };

    for message in &mut request.conversation_state.history {
        if let Message::User(user) = message {
            for image in &mut user.user_input_message.images {
                cap_one(image, true);
            }
        }
    }
    for image in &mut request
        .conversation_state
        .current_message
        .user_input_message
        .images
    {
        cap_one(image, false);
    }
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

fn truncate_tool_result_text(result: &mut ToolResult, max_chars: usize) -> bool {
    let mut truncated_result = false;
    for content in &mut result.content {
        let Some(serde_json::Value::String(text)) = content.get_mut("text") else {
            continue;
        };
        if text.chars().count() <= max_chars {
            continue;
        }

        let mut truncated = text.chars().take(max_chars).collect::<String>();
        truncated.push_str(EMPTY_RESPONSE_TOOL_RESULT_TRUNCATION_NOTICE);
        *text = truncated;
        truncated_result = true;
    }
    truncated_result
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

    let historical_result_count = request
        .conversation_state
        .history
        .iter()
        .map(|message| match message {
            Message::User(user) => user
                .user_input_message
                .user_input_message_context
                .tool_results
                .len(),
            Message::Assistant(_) => 0,
        })
        .sum::<usize>();
    let mut old_results_left =
        historical_result_count.saturating_sub(EMPTY_RESPONSE_RECENT_HISTORY_TOOL_RESULTS_TO_KEEP);
    for message in &mut request.conversation_state.history {
        if old_results_left == 0 {
            break;
        }
        let Message::User(user) = message else {
            continue;
        };
        for result in &mut user
            .user_input_message
            .user_input_message_context
            .tool_results
        {
            if old_results_left == 0 {
                break;
            }
            if truncate_tool_result_text(result, EMPTY_RESPONSE_HISTORY_TOOL_RESULT_MAX_CHARS) {
                stats.truncated_tool_results += 1;
            }
            old_results_left -= 1;
        }
    }

    for result in &mut request
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .tool_results
    {
        if truncate_tool_result_text(result, EMPTY_RESPONSE_TOOL_RESULT_MAX_CHARS) {
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
    fn empty_response_retry_compacts_old_history_tool_results_and_keeps_recent_eight() {
        let mut request = request_with_current_tool_results(Vec::new());
        for index in 0..10 {
            let mut historical = HistoryUserMessage::new(format!("history-{index}"), "model");
            historical.user_input_message.user_input_message_context =
                UserInputMessageContext::new().with_tool_results(vec![ToolResult {
                    tool_use_id: format!("tool-{index}"),
                    content: vec![serde_json::Map::from_iter([
                        (
                            "text".to_string(),
                            serde_json::Value::String(format!(
                                "result-{index}:{}",
                                "界".repeat(1_500)
                            )),
                        ),
                        ("kind".to_string(), serde_json::json!("stdout")),
                    ])],
                    status: Some("success".to_string()),
                    is_error: false,
                }]);
            request
                .conversation_state
                .history
                .push(Message::User(historical));
        }
        let primary_body = serde_json::to_string(&request).unwrap();

        let (recovery_body, stats) = prepare_empty_response_retry_body(
            &primary_body,
            ImageBudgetPolicy {
                enabled: false,
                ..ImageBudgetPolicy::default()
            },
        )
        .unwrap()
        .expect("older long tool results should produce a smaller recovery body");
        let recovery: KiroRequest = serde_json::from_str(&recovery_body).unwrap();

        assert!(recovery_body.len() < primary_body.len());
        assert_eq!(stats.truncated_tool_results, 2);
        for (index, message) in recovery.conversation_state.history.iter().enumerate() {
            let Message::User(message) = message else {
                panic!("fixture must contain historical user messages");
            };
            let result = &message
                .user_input_message
                .user_input_message_context
                .tool_results[0];
            assert_eq!(result.tool_use_id, format!("tool-{index}"));
            assert_eq!(result.status.as_deref(), Some("success"));
            assert!(!result.is_error);
            assert_eq!(result.content[0]["kind"], "stdout");
            let text = result.content[0]["text"].as_str().unwrap();
            if index < 2 {
                assert!(text.ends_with(EMPTY_RESPONSE_TOOL_RESULT_TRUNCATION_NOTICE));
                assert_eq!(
                    text.trim_end_matches(EMPTY_RESPONSE_TOOL_RESULT_TRUNCATION_NOTICE)
                        .chars()
                        .count(),
                    1_024
                );
            } else {
                assert_eq!(text, format!("result-{index}:{}", "界".repeat(1_500)));
            }
        }
    }

    #[test]
    fn empty_response_retry_keeps_all_history_tool_results_when_only_eight_exist() {
        let mut request = request_with_current_tool_results(Vec::new());
        for index in 0..8 {
            let mut historical = HistoryUserMessage::new(format!("history-{index}"), "model");
            historical.user_input_message.user_input_message_context =
                UserInputMessageContext::new().with_tool_results(vec![ToolResult::success(
                    format!("tool-{index}"),
                    "x".repeat(2_000),
                )]);
            request
                .conversation_state
                .history
                .push(Message::User(historical));
        }
        let primary_body = serde_json::to_string(&request).unwrap();

        assert!(
            prepare_empty_response_retry_body(
                &primary_body,
                ImageBudgetPolicy {
                    enabled: false,
                    ..ImageBudgetPolicy::default()
                },
            )
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
    /// 量化尺寸封顶给「全部未超限」请求带来的额外开销。
    ///
    /// 这是纯度量、不断言性能（CI 机器负载不可控），只把数字打出来供人判断。
    /// 关注点：封顶被放在字节预算 early-return **之前**，所以原本完全跳过图片处理的
    /// 请求现在也要为每张图做一次 `peek_dimensions`（含整张图 base64 全量解码）。
    #[test]
    fn measure_dimension_cap_overhead_on_within_limit_images() {
        // 噪点 PNG 不可压缩，尺寸要小到总量不触发字节预算路径，才能量到「纯封顶」开销。
        let images: Vec<String> = (0..8).map(|_| make_png(700, 500)).collect();
        let total_b64: usize = images.iter().map(String::len).sum();
        let request = request_with_images(images.clone(), images);
        let policy = ImageBudgetPolicy {
            total_base64_budget_bytes: 32 * 1024 * 1024,
            hard_base64_limit_bytes: 32 * 1024 * 1024,
            hard_max_dimension: 2_000,
            ..ImageBudgetPolicy::default()
        };

        let started = std::time::Instant::now();
        let mut cloned = request.clone();
        let stats = apply_image_budget(&mut cloned, policy).unwrap();
        let elapsed = started.elapsed();

        println!(
            "16 张 1200x900 全部未超限：base64 总量 {} KiB，耗时 {:?}，重编码 {} 张",
            total_b64 / 1024,
            elapsed,
            stats.resized_history_images + stats.resized_current_images
        );
        assert_eq!(
            stats.resized_history_images + stats.resized_current_images,
            0,
            "未超限不应重编码"
        );
    }

    /// 当前轮的超尺寸图必须被封顶。
    ///
    /// 回归线上 400：`image dimensions exceed max allowed size for many-image requests:
    /// 2000 pixels`。修复前 `apply_image_budget_inner` 只遍历 history、且被字节预算 gate 住，
    /// 当前轮的图从不压缩，单张超大图必然被上游拒。
    #[test]
    fn caps_oversized_current_turn_image_dimensions() {
        let oversized = make_png(2_600, 900);
        let mut request = request_with_images(Vec::new(), vec![oversized.clone()]);
        // 故意把字节预算放到远超实际总量：证明封顶不依赖字节预算触发。
        let policy = ImageBudgetPolicy {
            total_base64_budget_bytes: 32 * 1024 * 1024,
            hard_base64_limit_bytes: 32 * 1024 * 1024,
            hard_max_dimension: 2_000,
            ..ImageBudgetPolicy::default()
        };

        let stats = apply_image_budget(&mut request, policy).unwrap();

        assert_eq!(stats.resized_current_images, 1, "当前轮图应被封顶");
        let capped = &request
            .conversation_state
            .current_message
            .user_input_message
            .images[0];
        let (width, height) =
            crate::image_resize::peek_dimensions(&capped.format, &capped.source.bytes)
                .expect("封顶后应仍是可解析图片");
        assert!(
            width.max(height) <= 2_000,
            "长边应 <= 2000，实际 {width}x{height}"
        );
    }

    /// 未超尺寸的图不重编码：避免为所有请求白烧 CPU。
    #[test]
    fn leaves_within_limit_images_untouched() {
        let small = make_png(800, 600);
        let mut request = request_with_images(Vec::new(), vec![small.clone()]);
        let policy = ImageBudgetPolicy {
            total_base64_budget_bytes: 32 * 1024 * 1024,
            hard_base64_limit_bytes: 32 * 1024 * 1024,
            hard_max_dimension: 2_000,
            ..ImageBudgetPolicy::default()
        };

        let stats = apply_image_budget(&mut request, policy).unwrap();

        assert_eq!(stats.resized_current_images, 0);
        assert_eq!(
            request
                .conversation_state
                .current_message
                .user_input_message
                .images[0]
                .source
                .bytes,
            small,
            "未超限的图必须字节级不变"
        );
    }

    /// 历史里的超尺寸图同样封顶（此前只在字节预算超标时才压，且目标是 1280 而非硬上限）。
    #[test]
    fn caps_oversized_history_image_even_when_bytes_are_within_budget() {
        let oversized = make_png(2_400, 1_000);
        let mut request = request_with_images(vec![oversized], Vec::new());
        let policy = ImageBudgetPolicy {
            total_base64_budget_bytes: 32 * 1024 * 1024,
            hard_base64_limit_bytes: 32 * 1024 * 1024,
            hard_max_dimension: 2_000,
            ..ImageBudgetPolicy::default()
        };

        let stats = apply_image_budget(&mut request, policy).unwrap();

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
