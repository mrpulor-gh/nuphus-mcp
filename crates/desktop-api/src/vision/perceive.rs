//! UI perception merge engine — OCR + YOLO with IoU deduplication (desktop-api shared layer).
//!
//! Logic ported from the main crate's `src/desktop/ui_perception.rs`: PaddleOCR sees text but cannot
//! see icons/empty input boxes, while YOLO sees UI elements but does not know what is written on them.
//! This module merges the two into a unified UiElement view, and provides an entrypoint that runs
//! OCR + YOLO on a single loaded image.

use crate::vision::models::{resolve_models_dir, validate_ocr_models, yolo_model_available};
use crate::vision::paddle_ocr::{OcrBlock, PaddleOcr};
use crate::vision::yolo::YoloDetector;
use crate::vision::{Element, ElementKind};
use crate::{Frame, FrameSource, Rect, Scope};

// ─── Merged output types ───────────────────────────────────────────────

/// Merged UI element
#[derive(Debug, Clone)]
pub struct UiElement {
    pub id: u32,
    pub kind: ElementKind,
    pub text: Option<String>,
    pub rect: Rect,
    pub confidence: f32,
    pub source: ElementSource,
}

/// Element source
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementSource {
    /// Detected only by OCR
    Ocr,
    /// Detected only by YOLO
    Yolo,
    /// Detected by both (IoU > threshold)
    Both,
}

// ─── Merge parameters ───────────────────────────────────────────────────

/// Merge IoU threshold
const IOU_MERGE_THRESHOLD: f32 = 0.3;

// ─── Merge entrypoint ───────────────────────────────────────────────────

/// Merge OCR text blocks and YOLO element detections.
///
/// - Each OCR block tries to match the best YOLO box (IoU > 0.3)
///   - Match found → merged as Both, kind inferred heuristically
///   - No match → kept as Ocr
/// - Unmatched YOLO boxes → kept as Yolo (icon only)
pub fn merge(ocr_blocks: &[OcrBlock], yolo_elements: &[Element]) -> Vec<UiElement> {
    let mut result: Vec<UiElement> = Vec::new();
    let mut yolo_used = vec![false; yolo_elements.len()];

    // ── Round 1: OCR → match YOLO ──
    for block in ocr_blocks {
        let ocr_rect = Rect {
            x: block.x,
            y: block.y,
            w: block.w.max(0) as u32,
            h: block.h.max(0) as u32,
        };

        // Find the best matching YOLO box
        let mut best_iou: f32 = 0.0;
        let mut best_idx: Option<usize> = None;

        for (j, yolo_elem) in yolo_elements.iter().enumerate() {
            if yolo_used[j] {
                continue;
            }
            let iou = rect_iou(&ocr_rect, &yolo_elem.rect);
            if iou > IOU_MERGE_THRESHOLD && iou > best_iou {
                best_iou = iou;
                best_idx = Some(j);
            }
        }

        if let Some(idx) = best_idx {
            // Match found → merge
            yolo_used[idx] = true;
            let yolo_elem = &yolo_elements[idx];
            let combined_kind = infer_kind(&block.text, &yolo_elem.rect);

            result.push(UiElement {
                id: 0, // numbered uniformly at the end
                kind: combined_kind,
                text: Some(block.text.clone()),
                rect: yolo_elem.rect, // YOLO box is more accurate
                confidence: (block_confidence(&block.text) + yolo_elem.confidence) / 2.0,
                source: ElementSource::Both,
            });
        } else {
            // No match → standalone OCR element
            result.push(UiElement {
                id: 0,
                kind: ElementKind::Text,
                text: Some(block.text.clone()),
                rect: ocr_rect,
                confidence: block_confidence(&block.text),
                source: ElementSource::Ocr,
            });
        }
    }

    // ── Round 2: unmatched YOLO → standalone icons ──
    for (j, yolo_elem) in yolo_elements.iter().enumerate() {
        if yolo_used[j] {
            continue;
        }
        result.push(UiElement {
            id: 0,
            kind: ElementKind::Icon,
            text: None,
            rect: yolo_elem.rect,
            confidence: yolo_elem.confidence,
            source: ElementSource::Yolo,
        });
    }

    // ── Uniform numbering ──
    for (i, elem) in result.iter_mut().enumerate() {
        elem.id = i as u32;
    }

    result
}

/// Load one image and run PaddleOCR + YOLO together, then merge.
///
/// - OCR models missing → Err (clear error with the missing files, for download guidance).
/// - YOLO model missing → OCR-only result, `yolo_available = false` (honestly reported, not pretending to be available).
/// - Returns `PerceiveOutput` with merged elements, OCR/YOLO counts, and YOLO availability.
pub fn perceive_image(path: &str) -> Result<PerceiveOutput, String> {
    let models_dir = resolve_models_dir().ok_or_else(|| {
        "model directory not found; set the NUPHUS_MODELS_DIR environment variable or ensure the model files are located in data_dir/Nuphus/models".to_string()
    })?;
    validate_ocr_models(&models_dir).map_err(|e| {
        format!(
            "{e}\nDownload guidance: running desktop_perceive for the first time auto-downloads the PaddleOCR models, or download manually from \
             https://hf-mirror.com/SWHL/RapidOCR and https://gitee.com/paddlepaddle/PaddleOCR \
             and place them in {}",
            models_dir.display()
        )
    })?;

    // Decode once; OCR uses RGB, YOLO uses an RGBA Frame
    let img = image::open(path)
        .map_err(|e| format!("failed to open image {path}: {e}"))?
        .to_rgb8();
    let (w, h) = (img.width(), img.height());

    // Process-wide shared engines (P1): no 6623-line dictionary re-read and no
    // ~80MB model re-commit per call; inference is mutex-serialized inside.
    let ocr_blocks = PaddleOcr::with_shared(|ocr| ocr.ocr_image_with_boxes(&img))?;

    // Honest availability: the file existing is only a hint — availability is
    // decided by whether the session actually initializes (a corrupt
    // icon_detect.onnx must report yolo_available=false, not pretend).
    let mut yolo_available = false;
    let yolo_elements: Vec<Element> = if yolo_model_available(&models_dir) {
        let detector = YoloDetector::shared();
        match detector.init() {
            Ok(()) => {
                yolo_available = detector.is_loaded();
                let rgba = image::DynamicImage::ImageRgb8(img).to_rgba8();
                let frame = Frame {
                    id: uuid::Uuid::new_v4(),
                    pixels: rgba.into_raw(),
                    width: w,
                    height: h,
                    scope: Scope::Fullscreen,
                    timestamp: chrono::Utc::now(),
                    source: FrameSource::Screenshot,
                };
                detector
                    .detect(&frame)
                    .map_err(|e| format!("YOLO detection failed: {e}"))?
            }
            Err(e) => {
                tracing::warn!("[perceive] YOLO model present but failed to load, OCR-only: {e}");
                vec![]
            }
        }
    } else {
        vec![]
    };

    let elements = merge(&ocr_blocks, &yolo_elements);

    Ok(PerceiveOutput {
        elements,
        ocr_count: ocr_blocks.len(),
        yolo_count: yolo_elements.len(),
        yolo_available,
    })
}

/// perceive_image output
#[derive(Debug, Clone)]
pub struct PerceiveOutput {
    pub elements: Vec<UiElement>,
    pub ocr_count: usize,
    pub yolo_count: usize,
    pub yolo_available: bool,
}

// ─── Kind heuristic inference ────────────────────────────────────────────

/// Infer ElementKind for an OCR + YOLO merged element based on text content and geometric features
fn infer_kind(text: &str, rect: &Rect) -> ElementKind {
    let trimmed = text.trim();

    // Rule 1: aspect ratio > 3 → Input (input-box feature)
    if rect.h > 0 && (rect.w as f32 / rect.h as f32) > 3.0 {
        return ElementKind::Input;
    }

    // Rule 2: contains specific keywords → Input
    let lower = trimmed.to_lowercase();
    // Chinese UI keywords kept as Unicode escapes to preserve runtime values:
    //   U+8F93 U+5165 = input, U+641C U+7D22 = search,
    //   U+8BF7 U+8F93 U+5165 = please enter,
    //   U+7528 U+6237 U+540D = username, U+5BC6 U+7801 = password
    let input_keywords = [
        "\u{8F93}\u{5165}",
        "\u{641C}\u{7D22}",
        "\u{8BF7}\u{8F93}\u{5165}",
        "password",
        "email",
        "\u{7528}\u{6237}\u{540D}",
        "\u{5BC6}\u{7801}",
    ];
    for kw in &input_keywords {
        if lower.contains(kw) {
            return ElementKind::Input;
        }
    }

    // Rule 3: 1-4 chars without punctuation → Button
    let char_count = trimmed.chars().count();
    if (1..=4).contains(&char_count) && !contains_punctuation(trimmed) {
        return ElementKind::Button;
    }

    // Default → Text
    ElementKind::Text
}

/// Check whether the string contains punctuation
fn contains_punctuation(s: &str) -> bool {
    // Chinese punctuation kept as Unicode escapes to preserve runtime behavior:
    // U+3002 U+FF0C U+3001 U+FF1B U+FF1A U+FF1F U+FF01 U+2026
    // U+FF08 U+FF09 U+300A U+300B U+201C U+201D U+2018 U+2019
    s.chars().any(|c| {
        c.is_ascii_punctuation()
            || c == '\u{3002}'
            || c == '\u{FF0C}'
            || c == '\u{3001}'
            || c == '\u{FF1B}'
            || c == '\u{FF1A}'
            || c == '\u{FF1F}'
            || c == '\u{FF01}'
            || c == '\u{2026}'
            || c == '\u{FF08}'
            || c == '\u{FF09}'
            || c == '\u{300A}'
            || c == '\u{300B}'
            || c == '\u{201C}'
            || c == '\u{201D}'
            || c == '\u{2018}'
            || c == '\u{2019}'
    })
}

// ─── Helper functions ───────────────────────────────────────────────────

/// Estimate the confidence of an OCR text block (based on text length)
fn block_confidence(text: &str) -> f32 {
    let len = text.trim().len();
    if len == 0 {
        0.0
    } else if len <= 2 {
        0.95
    } else {
        0.85
    }
}

/// Compute the IoU (Intersection over Union) of two rectangles
fn rect_iou(a: &Rect, b: &Rect) -> f32 {
    let ax1 = a.x;
    let ay1 = a.y;
    let ax2 = a.x + a.w as i32;
    let ay2 = a.y + a.h as i32;

    let bx1 = b.x;
    let by1 = b.y;
    let bx2 = b.x + b.w as i32;
    let by2 = b.y + b.h as i32;

    let inter_x1 = ax1.max(bx1);
    let inter_y1 = ay1.max(by1);
    let inter_x2 = ax2.min(bx2);
    let inter_y2 = ay2.min(by2);

    if inter_x2 <= inter_x1 || inter_y2 <= inter_y1 {
        return 0.0;
    }

    let inter_area = (inter_x2 - inter_x1) as f32 * (inter_y2 - inter_y1) as f32;
    let area_a = (a.w * a.h) as f32;
    let area_b = (b.w * b.h) as f32;
    let union_area = area_a + area_b - inter_area;

    if union_area <= 0.0 {
        0.0
    } else {
        inter_area / union_area
    }
}

// ─── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn block(text: &str, x: i32, y: i32, w: i32, h: i32) -> OcrBlock {
        OcrBlock {
            text: text.into(),
            x,
            y,
            w,
            h,
        }
    }

    fn elem(x: i32, y: i32, w: u32, h: u32) -> Element {
        Element {
            kind: ElementKind::Icon,
            rect: Rect { x, y, w, h },
            confidence: 0.8,
        }
    }

    #[test]
    fn test_ocr_only() {
        let blocks = vec![block("hello", 10, 10, 100, 30)];
        let result = merge(&blocks, &[]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].source, ElementSource::Ocr);
        assert_eq!(result[0].kind, ElementKind::Text);
        assert_eq!(result[0].text.as_deref(), Some("hello"));
    }

    #[test]
    fn test_yolo_only() {
        let elements = vec![elem(50, 50, 30, 30)];
        let result = merge(&[], &elements);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].source, ElementSource::Yolo);
        assert_eq!(result[0].kind, ElementKind::Icon);
        assert_eq!(result[0].text, None);
    }

    #[test]
    fn test_merge_ocr_yolo() {
        let blocks = vec![block("\u{786E}\u{8BA4}", 45, 45, 40, 25)];
        let elements = vec![elem(40, 40, 50, 30)];
        let result = merge(&blocks, &elements);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].source, ElementSource::Both);
        assert_eq!(result[0].kind, ElementKind::Button); // 2 chars, no punct
        assert_eq!(result[0].text.as_deref(), Some("\u{786E}\u{8BA4}"));
    }

    #[test]
    fn test_merge_input_keyword() {
        let blocks = vec![block(
            "\u{8BF7}\u{8F93}\u{5165}\u{5185}\u{5BB9}",
            45,
            45,
            50,
            30,
        )];
        let elements = vec![elem(40, 40, 60, 35)];
        let result = merge(&blocks, &elements);
        assert_eq!(result[0].kind, ElementKind::Input);
    }

    #[test]
    fn test_merge_input_wide_aspect() {
        let blocks = vec![block("test", 50, 5, 300, 25)];
        let elements = vec![elem(0, 0, 400, 30)];
        let result = merge(&blocks, &elements);
        assert_eq!(result[0].kind, ElementKind::Input);
    }

    #[test]
    fn test_merge_no_overlap_keeps_both() {
        let blocks = vec![block("a", 0, 0, 10, 10), block("b", 100, 100, 10, 10)];
        let elements = vec![elem(300, 300, 20, 20)];
        let result = merge(&blocks, &elements);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].id, 0);
        assert_eq!(result[1].id, 1);
        assert_eq!(result[2].id, 2);
    }

    #[test]
    fn test_rect_iou_non_overlapping_is_zero() {
        assert_eq!(
            rect_iou(
                &Rect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 10
                },
                &Rect {
                    x: 100,
                    y: 100,
                    w: 10,
                    h: 10
                }
            ),
            0.0
        );
    }
}
