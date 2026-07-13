use serde::{Deserialize, Serialize};

use crate::image_resize::{ResizeTarget, shrink_image_with_target};
use crate::kiro::model::requests::{conversation::Message, kiro::KiroRequest};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImageBudgetPolicy {
    pub enabled: bool,
    pub total_base64_budget_bytes: usize,
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
            history_max_dimension: 1_280,
            history_jpeg_quality: 72,
            retry_history_max_dimension: 960,
            retry_history_jpeg_quality: 60,
        }
    }
}

impl ImageBudgetPolicy {
    pub fn validate(self) -> Result<Self, ImageBudgetError> {
        if !(256 * 1024..=8 * 1024 * 1024).contains(&self.total_base64_budget_bytes) {
            return Err(ImageBudgetError::InvalidPolicy(
                "totalBase64BudgetBytes 必须在 256 KiB–8 MiB 之间".to_string(),
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
        "图片总量在历史图片压缩后仍超过预算: count={count}, total={total} bytes, budget={budget} bytes"
    )]
    Exceeded {
        count: usize,
        total: usize,
        budget: usize,
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
    apply_image_budget_inner(request, policy.validate()?, false)
}

fn apply_image_budget_inner(
    request: &mut KiroRequest,
    policy: ImageBudgetPolicy,
    force_history_reencode: bool,
) -> Result<ImageBudgetStats, ImageBudgetError> {
    let mut stats = collect_stats(request);
    if !policy.enabled
        || (!force_history_reencode && stats.after_base64_bytes <= policy.total_base64_budget_bytes)
    {
        return Ok(stats);
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
            let original_len = image.source.bytes.len();
            match shrink_image_with_target(
                &image.format,
                &image.source.bytes,
                ResizeTarget {
                    max_long_side: policy.history_max_dimension,
                    jpeg_quality: policy.history_jpeg_quality,
                },
            ) {
                Ok(processed) if processed.data_base64.len() < original_len => {
                    stats.after_base64_bytes = stats
                        .after_base64_bytes
                        .saturating_sub(original_len)
                        .saturating_add(processed.data_base64.len());
                    image.format = processed.format;
                    image.source.bytes = processed.data_base64;
                    stats.resized_history_images += 1;
                }
                Ok(_) | Err(_) => stats.unshrinkable_history_images += 1,
            }
        }
    }

    if stats.after_base64_bytes > policy.total_base64_budget_bytes {
        return Err(ImageBudgetError::Exceeded {
            count: stats.image_count,
            total: stats.after_base64_bytes,
            budget: policy.total_base64_budget_bytes,
        });
    }
    Ok(stats)
}

/// 从同一个原始请求分别生成普通预算请求体和更激进的阈值重试请求体。
///
/// 重试体只有在历史图片确实进一步缩小、且完整 JSON 请求体更小时才提供；当前轮图片在
/// 两份副本中都保持原始字节不变。
pub fn prepare_kiro_bodies(
    request: &KiroRequest,
    policy: ImageBudgetPolicy,
) -> Result<PreparedKiroBodies, ImageBudgetError> {
    let policy = policy.validate()?;

    let mut primary = request.clone();
    let primary_stats = apply_image_budget_inner(&mut primary, policy, false)?;
    let primary_body = serde_json::to_string(&primary)
        .map_err(|error| ImageBudgetError::Serialization(error.to_string()))?;

    let mut retry = request.clone();
    let retry_stats = apply_image_budget_inner(&mut retry, policy.retry_variant(), true)?;
    let retry_body = serde_json::to_string(&retry)
        .map_err(|error| ImageBudgetError::Serialization(error.to_string()))?;
    let has_useful_retry = retry_stats.history_image_count > 0
        && retry_stats.resized_history_images > 0
        && retry_body.len() < primary_body.len();

    Ok(PreparedKiroBodies {
        primary_body,
        threshold_retry_body: has_useful_retry.then_some(retry_body),
        primary_stats,
        retry_stats: has_useful_retry.then_some(retry_stats),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::requests::{
        conversation::{
            ConversationState, CurrentMessage, HistoryUserMessage, KiroImage, Message,
            UserInputMessage,
        },
        kiro::KiroRequest,
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

    #[test]
    fn compresses_only_history_and_preserves_all_images() {
        let historical = make_png(1200, 1200);
        let current = make_png(900, 900);
        let mut request = request_with_images(vec![historical.clone()], vec![current.clone()]);
        let before_count = count_images(&request);

        let policy = ImageBudgetPolicy {
            total_base64_budget_bytes: current.len() + 500_000,
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
                ..ImageBudgetPolicy::default()
            },
        )
        .unwrap();

        assert!(prepared.threshold_retry_body.is_none());
    }
}
