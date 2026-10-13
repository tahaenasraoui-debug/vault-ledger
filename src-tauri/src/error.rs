#![forbid(unsafe_code)]

use serde::{Serialize, Serializer};
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("Filesystem access failure on path '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Unsupported or unrecognized document format at '{path}': {reason}")]
    UnsupportedFormat {
        path: PathBuf,
        reason: String,
    },

    #[error("PDF structure parsing failed for '{path}': {details}")]
    PdfExtraction {
        path: PathBuf,
        details: String,
    },

    #[error("Image decoding or rasterization failed for '{path}': {details}")]
    ImageDecoding {
        path: PathBuf,
        details: String,
    },

    #[error("Offline OCR engine failure: {details}")]
    OcrExecution {
        details: String,
    },

    #[error("Document yielded zero extractable text after both native stream and OCR passes: '{path}'")]
    EmptyDocumentText {
        path: PathBuf,
    },

    #[error("Cryptographic integrity mismatch for bundled asset '{asset}': expected SHA-256 {expected}, computed {actual}")]
    AssetIntegrityMismatch {
        asset: String,
        expected: String,
        actual: String,
    },

    #[error("Local LLM tensor/GGUF runtime error: {details}")]
    ModelInference {
        details: String,
    },

    #[error("Tokenizer initialization or encoding failure: {details}")]
    Tokenizer {
        details: String,
    },

    #[error("LLM failed to produce a valid JSON object within {tokens_generated} tokens. Raw output: {raw_output}")]
    MalformedModelJson {
        tokens_generated: usize,
        raw_output: String,
    },

    #[error("Extracted financial data failed accounting validation: {violation}")]
    AccountingValidation {
        violation: String,
    },

    #[error("Double-entry CSV export failure at '{path}': {details}")]
    CsvExport {
        path: PathBuf,
        details: String,
    },

    #[error("Offline SQLite storage error: {details}")]
    DatabaseStorage {
        details: String,
    },

    #[error("CRITICAL TAMPER ALERT on record '{record_id}': {reason} (expected {expected_hash}, computed {computed_hash})")]
    DatabaseTamperDetected {
        record_id: String,
        reason: String,
        expected_hash: String,
        computed_hash: String,
    },

    #[error("Internal concurrency or background worker join failure: {details}")]
    Concurrency {
        details: String,
    },

    #[error("Requested ledger record ID '{record_id}' was not found in the SQLite vault")]
    RecordNotFound {
        record_id: String,
    },
}

#[derive(Serialize)]
struct IpcErrorEnvelope<'a> {
    code: &'static str,
    recoverable: bool,
    message: String,
    context: Option<&'a str>,
}

impl VaultError {
    pub fn error_code(&self) -> &'static str {
        match self {
            VaultError::Io { .. } => "ERR_IO_ACCESS",
            VaultError::UnsupportedFormat { .. } => "ERR_UNSUPPORTED_FORMAT",
            VaultError::PdfExtraction { .. } => "ERR_PDF_PARSE",
            VaultError::ImageDecoding { .. } => "ERR_IMAGE_DECODE",
            VaultError::OcrExecution { .. } => "ERR_OCR_ENGINE",
            VaultError::EmptyDocumentText { .. } => "ERR_EMPTY_DOCUMENT",
            VaultError::AssetIntegrityMismatch { .. } => "ERR_ASSET_TAMPERED",
            VaultError::ModelInference { .. } => "ERR_GGUF_INFERENCE",
            VaultError::Tokenizer { .. } => "ERR_TOKENIZER",
            VaultError::MalformedModelJson { .. } => "ERR_LLM_JSON_SCHEMA",
            VaultError::AccountingValidation { .. } => "ERR_ACCOUNTING_INVARIANT",
            VaultError::CsvExport { .. } => "ERR_CSV_EXPORT",
            VaultError::DatabaseStorage { .. } => "ERR_SQLITE_STORAGE",
            VaultError::DatabaseTamperDetected { .. } => "ERR_SQLITE_TAMPER_DETECTED",
            VaultError::Concurrency { .. } => "ERR_WORKER_PANIC",
            VaultError::RecordNotFound { .. } => "ERR_RECORD_NOT_FOUND",
        }
    }

    pub fn is_recoverable(&self) -> bool {
        match self {
            VaultError::Io { .. } => true,
            VaultError::UnsupportedFormat { .. } => true,
            VaultError::PdfExtraction { .. } => true,
            VaultError::ImageDecoding { .. } => true,
            VaultError::OcrExecution { .. } => true,
            VaultError::EmptyDocumentText { .. } => true,
            VaultError::AssetIntegrityMismatch { .. } => false,
            VaultError::ModelInference { .. } => false,
            VaultError::Tokenizer { .. } => false,
            VaultError::MalformedModelJson { .. } => true,
            VaultError::AccountingValidation { .. } => true,
            VaultError::CsvExport { .. } => true,
            VaultError::DatabaseStorage { .. } => true,
            VaultError::DatabaseTamperDetected { .. } => false,
            VaultError::Concurrency { .. } => false,
            VaultError::RecordNotFound { .. } => true,
        }
    }
}

impl Serialize for VaultError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let envelope = IpcErrorEnvelope {
            code: self.error_code(),
            recoverable: self.is_recoverable(),
            message: self.to_string(),
            context: match self {
                VaultError::MalformedModelJson { raw_output, .. } => Some(raw_output.as_str()),
                VaultError::AccountingValidation { violation } => Some(violation.as_str()),
                VaultError::DatabaseTamperDetected { reason, .. } => Some(reason.as_str()),
                VaultError::Io { .. }
                | VaultError::UnsupportedFormat { .. }
                | VaultError::PdfExtraction { .. }
                | VaultError::ImageDecoding { .. }
                | VaultError::OcrExecution { .. }
                | VaultError::EmptyDocumentText { .. }
                | VaultError::AssetIntegrityMismatch { .. }
                | VaultError::ModelInference { .. }
                | VaultError::Tokenizer { .. }
                | VaultError::CsvExport { .. }
                | VaultError::DatabaseStorage { .. }
                | VaultError::Concurrency { .. }
                | VaultError::RecordNotFound { .. } => None,
            },
        };
        envelope.serialize(serializer)
    }
}
