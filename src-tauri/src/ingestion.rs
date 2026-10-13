#![forbid(unsafe_code)]

use crate::error::VaultError;
use image::{DynamicImage, GrayImage, ImageFormat, Luma};
use lopdf::{Document as PdfDocument, Object};
use ocrs::{ImageSource, OcrEngine, OcrEngineParams};
use rten::Model;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

pub struct IngestionOutput {
    pub extracted_text: String,
    pub sha256_hex: String,
    pub extraction_method: String,
}

pub struct DocumentIngestor {
    ocr_engine: OcrEngine,
}

impl DocumentIngestor {
    pub fn new(detection_model_path: &Path, recognition_model_path: &Path) -> Result<Self, VaultError> {
        let det_bytes = fs::read(detection_model_path).map_err(|source| VaultError::Io {
            path: detection_model_path.to_path_buf(),
            source,
        })?;
        let rec_bytes = fs::read(recognition_model_path).map_err(|source| VaultError::Io {
            path: recognition_model_path.to_path_buf(),
            source,
        })?;

        let detection_model = Model::load(det_bytes).map_err(|err| VaultError::OcrExecution {
            details: format!("Failed to load text detection RTEN model: {}", err),
        })?;
        let recognition_model = Model::load(rec_bytes).map_err(|err| VaultError::OcrExecution {
            details: format!("Failed to load text recognition RTEN model: {}", err),
        })?;

        let ocr_engine = OcrEngine::new(OcrEngineParams {
            detection_model: Some(detection_model),
            recognition_model: Some(recognition_model),
            ..Default::default()
        })
        .map_err(|err| VaultError::OcrExecution {
            details: format!("Failed to construct OcrEngine instance: {}", err),
        })?;

        Ok(Self { ocr_engine })
    }

    pub fn ingest_file(&self, file_path: &Path) -> Result<IngestionOutput, VaultError> {
        let raw_bytes = fs::read(file_path).map_err(|source| VaultError::Io {
            path: file_path.to_path_buf(),
            source,
        })?;

        if raw_bytes.len() < 8 {
            return Err(VaultError::UnsupportedFormat {
                path: file_path.to_path_buf(),
                reason: "File is smaller than 8 bytes and contains no valid header.".to_string(),
            });
        }

        let mut hasher = Sha256::new();
        hasher.update(&raw_bytes);
        let sha256_hex = hex::encode(hasher.finalize());

        if raw_bytes.starts_with(b"%PDF-") {
            let (text, method) = self.extract_from_pdf(file_path, &raw_bytes)?;
            let cleaned = normalize_extracted_text(&text);
            if cleaned.is_empty() {
                return Err(VaultError::EmptyDocumentText {
                    path: file_path.to_path_buf(),
                });
            }
            Ok(IngestionOutput {
                extracted_text: cleaned,
                sha256_hex,
                extraction_method: method,
            })
        } else if let Some(img_format) = detect_image_magic(&raw_bytes) {
            let dyn_image = image::load_from_memory_with_format(&raw_bytes, img_format).map_err(|err| {
                VaultError::ImageDecoding {
                    path: file_path.to_path_buf(),
                    details: err.to_string(),
                }
            })?;
            let ocr_text = self.run_ocr_on_image(&dyn_image)?;
            let cleaned = normalize_extracted_text(&ocr_text);
            if cleaned.is_empty() {
                return Err(VaultError::EmptyDocumentText {
                    path: file_path.to_path_buf(),
                });
            }
            Ok(IngestionOutput {
                extracted_text: cleaned,
                sha256_hex,
                extraction_method: format!("OCR_IMAGE ({:?})", img_format),
            })
        } else {
            Err(VaultError::UnsupportedFormat {
                path: file_path.to_path_buf(),
                reason: "Header magic bytes do not match PDF, PNG, JPEG, TIFF, or WebP.".to_string(),
            })
        }
    }

    fn extract_from_pdf(&self, file_path: &Path, raw_bytes: &[u8]) -> Result<(String, String), VaultError> {
        let pdf_doc = PdfDocument::load_mem(raw_bytes).map_err(|err| VaultError::PdfExtraction {
            path: file_path.to_path_buf(),
            details: format!("Failed to parse PDF object table: {}", err),
        })?;

        let pages = pdf_doc.get_pages();
        if pages.is_empty() {
            return Err(VaultError::PdfExtraction {
                path: file_path.to_path_buf(),
                details: "PDF document contains zero page objects.".to_string(),
            });
        }

        let mut page_numbers: Vec<u32> = pages.keys().copied().collect();
        page_numbers.sort_unstable();

        let native_text = pdf_doc.extract_text(&page_numbers).unwrap_or_default();
        let alphanumeric_count = native_text.chars().filter(|c| c.is_alphanumeric()).count();

        if alphanumeric_count >= 24 {
            return Ok((native_text, "PDF_NATIVE_STREAM".to_string()));
        }

        let embedded_images = extract_embedded_images_from_pdf(&pdf_doc);
        if embedded_images.is_empty() {
            return Ok((native_text, "PDF_MINIMAL_STREAM".to_string()));
        }

        let mut combined_ocr = String::new();
        for (idx, img) in embedded_images.iter().enumerate() {
            let page_ocr = self.run_ocr_on_image(img)?;
            if !page_ocr.trim().is_empty() {
                combined_ocr.push_str(&format!("--- SCANNED PAGE IMAGE {} ---\n", idx + 1));
                combined_ocr.push_str(&page_ocr);
                combined_ocr.push('\n');
            }
        }

        Ok((combined_ocr, "PDF_SCANNED_OCR_FALLBACK".to_string()))
    }

    fn run_ocr_on_image(&self, img: &DynamicImage) -> Result<String, VaultError> {
        let preprocessed = preprocess_for_ocr(img);
        let rgb_img = DynamicImage::ImageLuma8(preprocessed).into_rgb8();
        let (width, height) = rgb_img.dimensions();

        let img_source = ImageSource::from_bytes(rgb_img.as_raw(), (width, height)).map_err(|err| {
            VaultError::OcrExecution {
                details: format!("Failed to construct OCR ImageSource buffer: {}", err),
            }
        })?;

        let ocr_input = self.ocr_engine.prepare_input(img_source).map_err(|err| {
            VaultError::OcrExecution {
                details: format!("OCR input tensor preparation failed: {}", err),
            }
        })?;

        let word_rects = self.ocr_engine.detect_words(&ocr_input).map_err(|err| {
            VaultError::OcrExecution {
                details: format!("OCR word bounding-box detection failed: {}", err),
            }
        })?;

        let line_rects = self.ocr_engine.find_text_lines(&ocr_input, &word_rects);
        let line_texts = self.ocr_engine.recognize_text(&ocr_input, &line_rects).map_err(|err| {
            VaultError::OcrExecution {
                details: format!("OCR line recognition failed: {}", err),
            }
        })?;

        let mut output = String::new();
        for line in line_texts.into_iter().flatten() {
            let line_str = line.to_string();
            if !line_str.trim().is_empty() {
                output.push_str(line_str.trim());
                output.push('\n');
            }
        }

        Ok(output)
    }
}

fn preprocess_for_ocr(img: &DynamicImage) -> GrayImage {
    let gray = img.to_luma8();
    let (width, height) = gray.dimensions();
    if width == 0 || height == 0 {
        return gray;
    }

    let mut min_luma: u8 = 255;
    let mut max_luma: u8 = 0;
    for pixel in gray.pixels() {
        let val = pixel[0];
        if val < min_luma {
            min_luma = val;
        }
        if val > max_luma {
            max_luma = val;
        }
    }

    let dynamic_range = max_luma.saturating_sub(min_luma);
    if dynamic_range < 16 {
        return gray;
    }

    let mut enhanced = GrayImage::new(width, height);
    for (x, y, pixel) in gray.enumerate_pixels() {
        let normalized =
            ((u16::from(pixel[0].saturating_sub(min_luma)) * 255) / u16::from(dynamic_range)) as u8;
        enhanced.put_pixel(x, y, Luma([normalized]));
    }
    enhanced
}

fn extract_embedded_images_from_pdf(pdf_doc: &PdfDocument) -> Vec<DynamicImage> {
    let mut images = Vec::new();
    for (_object_id, object) in &pdf_doc.objects {
        if let Object::Stream(ref stream) = object {
            let is_image_subtype = stream
                .dict
                .get(b"Subtype")
                .ok()
                .and_then(|obj| obj.as_name_str().ok())
                .map(|name| name == "Image")
                .unwrap_or(false);

            if !is_image_subtype {
                continue;
            }

            if let Ok(decoded_img) = image::load_from_memory(&stream.content) {
                images.push(decoded_img);
                continue;
            }

            if let Ok(decompressed) = stream.decompressed_content() {
                if let Ok(decoded_img) = image::load_from_memory(&decompressed) {
                    images.push(decoded_img);
                }
            }
        }
    }
    images
}

fn detect_image_magic(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"II*\x00") || bytes.starts_with(b"MM\x00*") {
        Some(ImageFormat::Tiff)
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageFormat::WebP)
    } else {
        None
    }
}

fn normalize_extracted_text(raw: &str) -> String {
    let mut normalized = String::with_capacity(raw.len());
    let mut blank_line_streak = 0usize;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            blank_line_streak += 1;
            if blank_line_streak <= 1 && !normalized.is_empty() {
                normalized.push('\n');
            }
        } else {
            blank_line_streak = 0;
            normalized.push_str(trimmed);
            normalized.push('\n');
        }
    }

    normalized.trim().to_string()
}
