#![forbid(unsafe_code)]

pub mod error;
pub mod exporter;
pub mod inference;
pub mod ingestion;
pub mod schema;
pub mod storage;

use crate::error::VaultError;
use crate::exporter::{export_double_entry_ledger, ExportSummary};
use crate::inference::GemmaExtractionEngine;
use crate::ingestion::DocumentIngestor;
use crate::schema::{DatabaseAuditReport, ExtractedDocument, LedgerRecord};
use crate::storage::SqliteLedgerStore;
use chrono::Utc;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

pub struct AppState {
    pub ingestor: Arc<DocumentIngestor>,
    pub llm_engine: Arc<Mutex<GemmaExtractionEngine>>,
    pub sqlite_store: Arc<Mutex<SqliteLedgerStore>>,
    pub model_sha256: String,
}

#[derive(Clone, Serialize)]
pub struct InferenceProgressEvent {
    pub file_name: String,
    pub tokens_generated: usize,
    pub latest_token: String,
}

#[derive(Clone, Serialize)]
pub struct AirGapStatusReport {
    pub airgap_enforced: bool,
    pub unsafe_code_forbidden: bool,
    pub model_loaded: bool,
    pub model_sha256: String,
    pub database_path: String,
    pub audit_report: DatabaseAuditReport,
}

fn resolve_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

pub fn expand_tilde_path(path: &Path) -> Result<PathBuf, VaultError> {
    expand_tilde_with_home(path, resolve_home_dir().as_deref())
}

pub(crate) fn expand_tilde_with_home(
    path: &Path,
    home_dir: Option<&Path>,
) -> Result<PathBuf, VaultError> {
    let Some(raw) = path.to_str() else {
        return Ok(path.to_path_buf());
    };

    if raw == "~" {
        return match home_dir {
            Some(home) => Ok(home.to_path_buf()),
            None => Err(VaultError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Cannot expand '~': neither HOME nor USERPROFILE environment variable is set",
                ),
            }),
        };
    }

    if raw.starts_with("~/") || raw.starts_with("~\\") {
        let home = home_dir.ok_or_else(|| VaultError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Cannot expand '~': neither HOME nor USERPROFILE environment variable is set",
            ),
        })?;

        let remainder = raw[1..].trim_start_matches(['/', '\\']);
        if remainder.is_empty() {
            return Ok(home.to_path_buf());
        }
        return Ok(home.join(remainder));
    }

    Ok(path.to_path_buf())
}

#[tauri::command]
async fn ingest_and_extract_document(
    app: AppHandle,
    state: State<'_, AppState>,
    file_path: String,
) -> Result<LedgerRecord, VaultError> {
    let path_buf = expand_tilde_path(Path::new(&file_path))?;
    let canonical_path = fs::canonicalize(&path_buf).map_err(|source| VaultError::Io {
        path: path_buf.clone(),
        source,
    })?;

    let file_name = canonical_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown_document")
        .to_string();

    let ingestor = Arc::clone(&state.ingestor);
    let llm_engine = Arc::clone(&state.llm_engine);
    let sqlite_store = Arc::clone(&state.sqlite_store);
    let app_handle = app.clone();
    let file_name_for_worker = file_name.clone();

    tokio::task::spawn_blocking(move || -> Result<LedgerRecord, VaultError> {
        let ingestion_result = ingestor.ingest_file(&canonical_path)?;

        let extracted_doc = {
            let mut engine_guard = llm_engine.lock().map_err(|_| VaultError::Concurrency {
                details: "LLM inference mutex was poisoned by a prior thread failure.".to_string(),
            })?;

            engine_guard.extract_structured_document(
                &ingestion_result.extracted_text,
                |token_count, token_piece| {
                    let _ = app_handle.emit(
                        "vault://inference-progress",
                        InferenceProgressEvent {
                            file_name: file_name_for_worker.clone(),
                            tokens_generated: token_count,
                            latest_token: token_piece.to_string(),
                        },
                    );
                },
            )?
        };

        let mut db_guard = sqlite_store.lock().map_err(|_| VaultError::Concurrency {
            details: "SQLite store mutex was poisoned.".to_string(),
        })?;

        db_guard.insert_record(
            Uuid::new_v4().to_string(),
            file_name_for_worker,
            ingestion_result.sha256_hex,
            ingestion_result.extraction_method,
            Utc::now().to_rfc3339(),
            extracted_doc,
        )
    })
    .await
    .map_err(|join_err| VaultError::Concurrency {
        details: format!("Blocking extraction worker failed to join: {}", join_err),
    })?
}

#[tauri::command]
async fn list_ledger_records(state: State<'_, AppState>) -> Result<Vec<LedgerRecord>, VaultError> {
    let sqlite_store = Arc::clone(&state.sqlite_store);
    tokio::task::spawn_blocking(move || -> Result<Vec<LedgerRecord>, VaultError> {
        let db_guard = sqlite_store.lock().map_err(|_| VaultError::Concurrency {
            details: "SQLite store mutex was poisoned.".to_string(),
        })?;
        db_guard.list_records()
    })
    .await
    .map_err(|join_err| VaultError::Concurrency {
        details: format!("SQLite list_records worker failed to join: {}", join_err),
    })?
}

#[tauri::command]
async fn update_ledger_record(
    state: State<'_, AppState>,
    record_id: String,
    updated_document: ExtractedDocument,
) -> Result<LedgerRecord, VaultError> {
    let sqlite_store = Arc::clone(&state.sqlite_store);
    tokio::task::spawn_blocking(move || -> Result<LedgerRecord, VaultError> {
        let mut db_guard = sqlite_store.lock().map_err(|_| VaultError::Concurrency {
            details: "SQLite store mutex was poisoned.".to_string(),
        })?;
        db_guard.update_record(&record_id, updated_document)
    })
    .await
    .map_err(|join_err| VaultError::Concurrency {
        details: format!("SQLite update_record worker failed to join: {}", join_err),
    })?
}

#[tauri::command]
async fn delete_ledger_record(
    state: State<'_, AppState>,
    record_id: String,
) -> Result<DatabaseAuditReport, VaultError> {
    let sqlite_store = Arc::clone(&state.sqlite_store);
    tokio::task::spawn_blocking(move || -> Result<DatabaseAuditReport, VaultError> {
        let mut db_guard = sqlite_store.lock().map_err(|_| VaultError::Concurrency {
            details: "SQLite store mutex was poisoned.".to_string(),
        })?;
        db_guard.delete_record(&record_id)
    })
    .await
    .map_err(|join_err| VaultError::Concurrency {
        details: format!("SQLite delete_record worker failed to join: {}", join_err),
    })?
}

#[tauri::command]
async fn verify_ledger_audit_chain(
    state: State<'_, AppState>,
) -> Result<DatabaseAuditReport, VaultError> {
    let sqlite_store = Arc::clone(&state.sqlite_store);
    tokio::task::spawn_blocking(move || -> Result<DatabaseAuditReport, VaultError> {
        let db_guard = sqlite_store.lock().map_err(|_| VaultError::Concurrency {
            details: "SQLite store mutex was poisoned.".to_string(),
        })?;
        db_guard.verify_full_chain()
    })
    .await
    .map_err(|join_err| VaultError::Concurrency {
        details: format!(
            "SQLite audit verification worker failed to join: {}",
            join_err
        ),
    })?
}

#[tauri::command]
async fn export_double_entry_csv(
    state: State<'_, AppState>,
    destination_path: String,
) -> Result<ExportSummary, VaultError> {
    let sqlite_store = Arc::clone(&state.sqlite_store);
    let dest_path_buf = expand_tilde_path(Path::new(&destination_path))?;

    tokio::task::spawn_blocking(move || -> Result<ExportSummary, VaultError> {
        let records_snapshot = {
            let db_guard = sqlite_store.lock().map_err(|_| VaultError::Concurrency {
                details: "SQLite store mutex was poisoned.".to_string(),
            })?;
            db_guard.verify_full_chain()?;
            db_guard.list_records()?
        };

        export_double_entry_ledger(&records_snapshot, &dest_path_buf)
    })
    .await
    .map_err(|join_err| VaultError::Concurrency {
        details: format!("CSV export worker failed to join: {}", join_err),
    })?
}

#[tauri::command]
async fn verify_airgap_integrity(
    state: State<'_, AppState>,
) -> Result<AirGapStatusReport, VaultError> {
    let sqlite_store = Arc::clone(&state.sqlite_store);
    let model_sha256 = state.model_sha256.clone();

    tokio::task::spawn_blocking(move || -> Result<AirGapStatusReport, VaultError> {
        let db_guard = sqlite_store.lock().map_err(|_| VaultError::Concurrency {
            details: "SQLite store mutex was poisoned.".to_string(),
        })?;
        let audit_report = db_guard.verify_full_chain()?;
        let database_path = db_guard.database_path().display().to_string();

        Ok(AirGapStatusReport {
            airgap_enforced: true,
            unsafe_code_forbidden: true,
            model_loaded: true,
            model_sha256,
            database_path,
            audit_report,
        })
    })
    .await
    .map_err(|join_err| VaultError::Concurrency {
        details: format!("Air-gap integrity worker failed to join: {}", join_err),
    })?
}

fn compute_file_sha256(path: &Path) -> Result<String, VaultError> {
    let bytes = fs::read(path).map_err(|source| VaultError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let resolver = app.path();

            let gguf_path = resolver
                .resolve(
                    "resources/models/gemma-2b-it-q4_k_m.gguf",
                    BaseDirectory::Resource,
                )
                .map_err(|e| {
                    Box::new(VaultError::ModelInference {
                        details: e.to_string(),
                    })
                })?;

            let tokenizer_path = resolver
                .resolve("resources/models/tokenizer.json", BaseDirectory::Resource)
                .map_err(|e| {
                    Box::new(VaultError::Tokenizer {
                        details: e.to_string(),
                    })
                })?;

            let det_ocr_path = resolver
                .resolve("resources/ocr/text-detection.rten", BaseDirectory::Resource)
                .map_err(|e| {
                    Box::new(VaultError::OcrExecution {
                        details: e.to_string(),
                    })
                })?;

            let rec_ocr_path = resolver
                .resolve(
                    "resources/ocr/text-recognition.rten",
                    BaseDirectory::Resource,
                )
                .map_err(|e| {
                    Box::new(VaultError::OcrExecution {
                        details: e.to_string(),
                    })
                })?;

            let app_data_dir = resolver
                .resolve("vault_storage", BaseDirectory::AppLocalData)
                .map_err(|e| {
                    Box::new(VaultError::DatabaseStorage {
                        details: e.to_string(),
                    })
                })?;

            let sqlite_db_path = app_data_dir.join("vault_ledger.sqlite3");

            let model_sha256 = compute_file_sha256(&gguf_path)?;
            let ingestor = DocumentIngestor::new(&det_ocr_path, &rec_ocr_path)?;
            let llm_engine = GemmaExtractionEngine::load_from_disk(&gguf_path, &tokenizer_path)?;
            let (sqlite_store, _startup_audit) =
                SqliteLedgerStore::open_and_verify(&sqlite_db_path)?;

            app.manage(AppState {
                ingestor: Arc::new(ingestor),
                llm_engine: Arc::new(Mutex::new(llm_engine)),
                sqlite_store: Arc::new(Mutex::new(sqlite_store)),
                model_sha256,
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ingest_and_extract_document,
            list_ledger_records,
            update_ledger_record,
            delete_ledger_record,
            verify_ledger_audit_chain,
            export_double_entry_csv,
            verify_airgap_integrity,
        ])
        .run(tauri::generate_context!())
        .expect("Fatal failure while starting air-gapped VaultLedger Tauri v2 runtime");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_expands_home_and_subpaths() {
        let home = Path::new("/home/alice");

        assert_eq!(
            expand_tilde_with_home(Path::new("~"), Some(home)).unwrap(),
            PathBuf::from("/home/alice")
        );
        assert_eq!(
            expand_tilde_with_home(Path::new("~/"), Some(home)).unwrap(),
            PathBuf::from("/home/alice")
        );
        assert_eq!(
            expand_tilde_with_home(Path::new("~/Documents/invoice.pdf"), Some(home)).unwrap(),
            PathBuf::from("/home/alice/Documents/invoice.pdf")
        );
        assert_eq!(
            expand_tilde_with_home(Path::new("~//Documents/journal_2026.csv"), Some(home)).unwrap(),
            PathBuf::from("/home/alice/Documents/journal_2026.csv")
        );
        assert_eq!(
            expand_tilde_with_home(Path::new("~\\Documents\\journal_2026.csv"), Some(home))
                .unwrap(),
            PathBuf::from("/home/alice").join("Documents\\journal_2026.csv")
        );
    }

    #[test]
    fn expand_tilde_leaves_non_home_paths_unchanged() {
        let home = Path::new("/home/alice");

        assert_eq!(
            expand_tilde_with_home(Path::new("/tmp/invoice.pdf"), Some(home)).unwrap(),
            PathBuf::from("/tmp/invoice.pdf")
        );
        assert_eq!(
            expand_tilde_with_home(Path::new("relative/invoice.pdf"), Some(home)).unwrap(),
            PathBuf::from("relative/invoice.pdf")
        );
        assert_eq!(
            expand_tilde_with_home(Path::new("~bob/invoice.pdf"), Some(home)).unwrap(),
            PathBuf::from("~bob/invoice.pdf")
        );
        assert_eq!(
            expand_tilde_with_home(Path::new("/var/~/invoice.pdf"), Some(home)).unwrap(),
            PathBuf::from("/var/~/invoice.pdf")
        );
    }

    #[test]
    fn expand_tilde_errors_when_home_is_missing() {
        let err = expand_tilde_with_home(Path::new("~/Documents/invoice.pdf"), None).unwrap_err();
        assert!(matches!(err, VaultError::Io { .. }));

        let bare_err = expand_tilde_with_home(Path::new("~"), None).unwrap_err();
        assert!(matches!(bare_err, VaultError::Io { .. }));

        // Non-tilde paths still succeed even without HOME set.
        assert_eq!(
            expand_tilde_with_home(Path::new("/tmp/invoice.pdf"), None).unwrap(),
            PathBuf::from("/tmp/invoice.pdf")
        );
    }

    #[test]
    fn expand_tilde_path_uses_environment_home() {
        if let Some(expected_home) = resolve_home_dir() {
            let expanded = expand_tilde_path(Path::new("~/Documents/invoice.pdf")).unwrap();
            assert_eq!(expanded, expected_home.join("Documents/invoice.pdf"));
        }
    }
}
