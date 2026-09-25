//! YOLO icon detection — ONNX Runtime based UI element detection (desktop-api shared layer).
//!
//! Logic ported from the main crate's `src/desktop/yolo_detect.rs`, sharing the model path with paddle_ocr.
//! Detects icons, buttons and other UI elements, complementing OCR text detection.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::vision::models::{resolve_models_dir, YOLO_MODEL};
use crate::vision::{Element, ElementKind};
use crate::{DesktopError, Frame, Rect};

/// YOLO detection parameters
const YOLO_INPUT_SIZE: u32 = 640;
const YOLO_CONF_THRESHOLD: f32 = 0.25;
const NMS_IOU_THRESHOLD: f32 = 0.45;

/// Process-wide shared detector (P1: per-call `YoloDetector::new` + `detect`
/// re-committed the ~80 MB ONNX model on every single perceive).
static SHARED_YOLO: OnceLock<YoloDetector> = OnceLock::new();

/// YOLO icon detector
pub struct YoloDetector {
    session: Mutex<Option<ort::session::Session>>,
    /// Model path the loaded session was built from — a path change (e.g.
    /// NUPHUS_MODELS_DIR switch) triggers a reload instead of serving stale state.
    loaded_from: Mutex<Option<PathBuf>>,
}

impl Default for YoloDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl YoloDetector {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
            loaded_from: Mutex::new(None),
        }
    }

    /// Process-wide shared detector instance.
    pub fn shared() -> &'static YoloDetector {
        SHARED_YOLO.get_or_init(YoloDetector::new)
    }

    /// Whether a usable model session is currently loaded (honest availability
    /// signal for callers reporting `yolo_available`).
    pub fn is_loaded(&self) -> bool {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Initialize the YOLO session (lazy-loaded; re-initializes when the resolved
    /// model path changed).
    pub fn init(&self) -> Result<(), DesktopError> {
        super::paddle_ocr::ensure_ort_dylib();
        let model_path = resolve_models_dir()
            .map(|dir| dir.join(YOLO_MODEL))
            .filter(|p| p.exists())
            .ok_or_else(|| {
                DesktopError::Other(anyhow::anyhow!(
                    "[yolo] icon_detect.onnx model file not found"
                ))
            })?;

        {
            let session_guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
            let path_guard = self.loaded_from.lock().unwrap_or_else(|e| e.into_inner());
            if session_guard.is_some() && path_guard.as_ref() == Some(&model_path) {
                return Ok(());
            }
        }

        tracing::info!("[yolo] loading ONNX model: {}", model_path.display());

        let session = ort::session::Session::builder()
            .map_err(|e| {
                DesktopError::Other(anyhow::anyhow!(
                    "[yolo] failed to create session builder: {e}"
                ))
            })?
            .commit_from_file(&model_path)
            .map_err(|e| {
                DesktopError::Other(anyhow::anyhow!("[yolo] failed to load model: {e}"))
            })?;

        // Diagnostic logs
        for input in session.inputs() {
            tracing::debug!(
                "[yolo] input: name={}, type={:?}",
                input.name(),
                input.dtype()
            );
        }
        for output in session.outputs() {
            tracing::debug!(
                "[yolo] output: name={}, type={:?}",
                output.name(),
                output.dtype()
            );
        }

        let mut guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(session);
        *self.loaded_from.lock().unwrap_or_else(|e| e.into_inner()) = Some(model_path);
        tracing::info!("[yolo] model loaded");
        Ok(())
    }

    /// Detect UI elements in a frame
    pub fn detect(&self, frame: &Frame) -> Result<Vec<Element>, DesktopError> {
        // Lazy initialization (returns empty when the model is missing, does not panic — callers
        // pre-check availability via models::yolo_model_available)
        {
            let guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_none() {
                drop(guard);
                if let Err(e) = self.init() {
                    tracing::warn!("[yolo] detect skipped (model not loaded): {e}");
                    return Ok(vec![]);
                }
            }
        }

        // 1. Preprocessing
        let input_array = Self::preprocess_frame(frame)?;

        // 2. ORT inference
        let mut guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let session = match guard.as_mut() {
            Some(s) => s,
            None => return Ok(vec![]),
        };

        let input_name = session.inputs()[0].name().to_string();
        let output_name = session.outputs()[0].name().to_string();

        let input_value =
            ort::value::TensorRef::from_array_view(input_array.view()).map_err(|e| {
                DesktopError::Other(anyhow::anyhow!("[yolo] failed to create input tensor: {e}"))
            })?;

        // Inference + postprocessing
        let detections = {
            let outputs = session
                .run(ort::inputs![input_name.as_str() => input_value])
                .map_err(|e| {
                    DesktopError::Other(anyhow::anyhow!("[yolo] inference failed: {e}"))
                })?;

            let output_value = outputs.get(output_name.as_str()).ok_or_else(|| {
                DesktopError::Other(anyhow::anyhow!("[yolo] output '{output_name}' not found"))
            })?;

            let output_view = output_value.try_extract_array::<f32>().map_err(|e| {
                DesktopError::Other(anyhow::anyhow!(
                    "[yolo] failed to extract output array: {e}"
                ))
            })?;

            // Diagnostics
            let out_shape = output_view.shape();
            if out_shape.len() >= 3 && out_shape[1] >= 5 {
                let num_dets = out_shape[2];
                let max_conf = (0..num_dets)
                    .map(|i| output_view[[0, 4, i]])
                    .fold(0.0f32, f32::max);
                let num_above = (0..num_dets)
                    .filter(|&i| output_view[[0, 4, i]] >= YOLO_CONF_THRESHOLD)
                    .count();
                tracing::info!(
                    "[yolo] inference complete: shape={:?}, max_conf={:.4}, >{:.2}={}",
                    out_shape,
                    max_conf,
                    YOLO_CONF_THRESHOLD,
                    num_above
                );
            }

            let shape = output_view.shape();
            if shape.len() < 3 || shape[1] < 5 {
                tracing::warn!("[yolo] unexpected output shape: {:?}", shape);
                return Ok(vec![]);
            }
            let num_detections = shape[2];

            let scale_x = frame.width as f32 / YOLO_INPUT_SIZE as f32;
            let scale_y = frame.height as f32 / YOLO_INPUT_SIZE as f32;

            let mut dets: Vec<(Rect, f32)> = Vec::new();
            for i in 0..num_detections {
                let cx = output_view[[0, 0, i]];
                let cy = output_view[[0, 1, i]];
                let w = output_view[[0, 2, i]];
                let h = output_view[[0, 3, i]];
                let conf = output_view[[0, 4, i]];

                if conf < YOLO_CONF_THRESHOLD {
                    continue;
                }

                let abs_cx = cx * scale_x;
                let abs_cy = cy * scale_y;
                let abs_w = w * scale_x;
                let abs_h = h * scale_y;

                let x = (abs_cx - abs_w / 2.0).max(0.0) as i32;
                let y = (abs_cy - abs_h / 2.0).max(0.0) as i32;
                let rw = (abs_w.min(frame.width as f32 - x as f32)).max(1.0) as u32;
                let rh = (abs_h.min(frame.height as f32 - y as f32)).max(1.0) as u32;

                dets.push((Rect { x, y, w: rw, h: rh }, conf));
            }
            dets
        }; // outputs dropped here

        drop(guard);

        // 3. NMS
        let filtered = Self::nms(&detections, NMS_IOU_THRESHOLD);

        // 4. Map to Elements
        let elements: Vec<Element> = filtered
            .into_iter()
            .map(|(rect, conf)| Element {
                kind: ElementKind::Icon,
                rect,
                confidence: conf,
            })
            .collect();

        tracing::debug!(
            "[yolo] detection complete: {} candidates → {} elements (NMS)",
            detections.len(),
            elements.len()
        );

        Ok(elements)
    }

    // ─── Internal methods ───────────────────────────────────────────

    /// Preprocess an RGBA frame into a BCHW ndarray (1,3,640,640) float32 normalized [0,1]
    fn preprocess_frame(frame: &Frame) -> Result<ndarray::Array4<f32>, DesktopError> {
        use image::imageops::FilterType;

        let rgba = image::RgbaImage::from_raw(frame.width, frame.height, frame.pixels.clone())
            .ok_or_else(|| {
                DesktopError::Other(anyhow::anyhow!("[yolo] invalid frame pixel data"))
            })?;

        let rgb = image::DynamicImage::ImageRgba8(rgba).to_rgb8();
        let resized =
            image::imageops::resize(&rgb, YOLO_INPUT_SIZE, YOLO_INPUT_SIZE, FilterType::Lanczos3);

        let (w, h) = (resized.width() as usize, resized.height() as usize);
        let raw = resized.into_raw();

        let mut data = vec![0.0f32; 3 * h * w];
        for y in 0..h {
            for x in 0..w {
                let src_idx = (y * w + x) * 3;
                data[y * w + x] = raw[src_idx] as f32 / 255.0;
                data[h * w + y * w + x] = raw[src_idx + 1] as f32 / 255.0;
                data[2 * h * w + y * w + x] = raw[src_idx + 2] as f32 / 255.0;
            }
        }

        let arr = ndarray::Array4::from_shape_vec((1, 3, h, w), data).map_err(|e| {
            DesktopError::Other(anyhow::anyhow!("[yolo] failed to build ndarray: {e}"))
        })?;

        Ok(arr)
    }

    /// NMS (Non-Maximum Suppression) — descending by confidence
    fn nms(detections: &[(Rect, f32)], iou_threshold: f32) -> Vec<(Rect, f32)> {
        if detections.is_empty() {
            return vec![];
        }

        let mut idx_sort: Vec<usize> = (0..detections.len()).collect();
        idx_sort.sort_by(|&a, &b| {
            detections[b]
                .1
                .partial_cmp(&detections[a].1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut keep = vec![true; detections.len()];

        for i in 0..idx_sort.len() {
            let a = idx_sort[i];
            if !keep[a] {
                continue;
            }
            for &b in &idx_sort[i + 1..] {
                if !keep[b] {
                    continue;
                }
                if Self::compute_iou(&detections[a].0, &detections[b].0) > iou_threshold {
                    keep[b] = false;
                }
            }
        }

        idx_sort
            .into_iter()
            .filter(|&i| keep[i])
            .map(|i| detections[i])
            .collect()
    }

    /// Compute the IoU of two boxes
    fn compute_iou(a: &Rect, b: &Rect) -> f32 {
        let ax1 = a.x as f32;
        let ay1 = a.y as f32;
        let ax2 = ax1 + a.w as f32;
        let ay2 = ay1 + a.h as f32;

        let bx1 = b.x as f32;
        let by1 = b.y as f32;
        let bx2 = bx1 + b.w as f32;
        let by2 = by1 + b.h as f32;

        let inter_x1 = ax1.max(bx1);
        let inter_y1 = ay1.max(by1);
        let inter_x2 = ax2.min(bx2);
        let inter_y2 = ay2.min(by2);

        let inter_w = (inter_x2 - inter_x1).max(0.0);
        let inter_h = (inter_y2 - inter_y1).max(0.0);
        let inter_area = inter_w * inter_h;

        let area_a = a.w as f32 * a.h as f32;
        let area_b = b.w as f32 * b.h as f32;
        let union_area = area_a + area_b - inter_area;

        if union_area <= 0.0 {
            0.0
        } else {
            inter_area / union_area
        }
    }
}
