#![forbid(unsafe_code)]

use crate::error::VaultError;
use crate::schema::{
    amount_to_cents, cents_to_amount, compute_audit_event_hash, compute_record_chain_hash,
    DatabaseAuditReport, ExpenseCategory, ExtractedDocument, LedgerRecord, GENESIS_CHAIN_HASH,
};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::fs;
use std::path::{Path, PathBuf};

pub struct SqliteLedgerStore {
    conn: Connection,
    db_path: PathBuf,
}

impl SqliteLedgerStore {
    pub fn open_and_verify(db_path: &Path) -> Result<(Self, DatabaseAuditReport), VaultError> {
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent).map_err(|source| VaultError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let conn = Connection::open(db_path).map_err(|err| VaultError::DatabaseStorage {
            details: format!(
                "Failed to open SQLite database at '{}': {}",
                db_path.display(),
                err
            ),
        })?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;
             PRAGMA temp_store = MEMORY;
             PRAGMA trusted_schema = OFF;",
        )
        .map_err(|err| VaultError::DatabaseStorage {
            details: format!("Failed to configure hardened SQLite PRAGMAs: {}", err),
        })?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ledger_records (
                seq_no INTEGER PRIMARY KEY AUTOINCREMENT,
                record_id TEXT NOT NULL UNIQUE,
                source_file_name TEXT NOT NULL,
                source_sha256 TEXT NOT NULL,
                extraction_method TEXT NOT NULL,
                ingested_at_utc TEXT NOT NULL,
                vendor_name TEXT NOT NULL,
                invoice_number TEXT,
                transaction_date TEXT NOT NULL,
                currency TEXT NOT NULL,
                net_amount_cents INTEGER NOT NULL,
                tax_amount_cents INTEGER NOT NULL,
                total_amount_cents INTEGER NOT NULL,
                tax_rate_bps INTEGER,
                expense_category TEXT NOT NULL,
                confidence_score REAL NOT NULL,
                raw_notes TEXT,
                row_audit_hash TEXT NOT NULL,
                prev_chain_hash TEXT NOT NULL,
                chain_hash TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_ledger_records_date
                ON ledger_records(transaction_date ASC, seq_no ASC);

            CREATE TABLE IF NOT EXISTS ledger_audit_events (
                event_id INTEGER PRIMARY KEY AUTOINCREMENT,
                record_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                occurred_at_utc TEXT NOT NULL,
                snapshot_json TEXT NOT NULL,
                row_audit_hash TEXT NOT NULL,
                prev_event_hash TEXT NOT NULL,
                event_chain_hash TEXT NOT NULL
            );",
        )
        .map_err(|err| VaultError::DatabaseStorage {
            details: format!("Failed to initialize SQLite schema tables: {}", err),
        })?;

        let store = Self {
            conn,
            db_path: db_path.to_path_buf(),
        };

        let audit_report = store.verify_full_chain()?;
        Ok((store, audit_report))
    }

    pub fn database_path(&self) -> &Path {
        &self.db_path
    }

    pub fn insert_record(
        &mut self,
        record_id: String,
        source_file_name: String,
        source_sha256: String,
        extraction_method: String,
        ingested_at_utc: String,
        mut document: ExtractedDocument,
    ) -> Result<LedgerRecord, VaultError> {
        document.validate_and_reconcile()?;

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|err| VaultError::DatabaseStorage {
                details: format!("Failed to begin IMMEDIATE SQLite transaction: {}", err),
            })?;

        let prev_chain_hash: String = tx
            .query_row(
                "SELECT chain_hash FROM ledger_records ORDER BY seq_no DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| VaultError::DatabaseStorage {
                details: format!("Failed to query previous chain_hash head: {}", err),
            })?
            .unwrap_or_else(|| GENESIS_CHAIN_HASH.to_string());

        let row_audit_hash = document.compute_audit_digest(&record_id);
        let chain_hash = compute_record_chain_hash(
            &prev_chain_hash,
            &record_id,
            &source_sha256,
            &ingested_at_utc,
            &row_audit_hash,
        );

        let net_cents = amount_to_cents(document.net_amount);
        let tax_cents = amount_to_cents(document.tax_amount);
        let total_cents = amount_to_cents(document.total_amount);
        let tax_rate_bps = document.tax_rate_percent.map(amount_to_cents);

        tx.execute(
            "INSERT INTO ledger_records (
                record_id, source_file_name, source_sha256, extraction_method, ingested_at_utc,
                vendor_name, invoice_number, transaction_date, currency,
                net_amount_cents, tax_amount_cents, total_amount_cents, tax_rate_bps,
                expense_category, confidence_score, raw_notes,
                row_audit_hash, prev_chain_hash, chain_hash
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            params![
                record_id,
                source_file_name,
                source_sha256,
                extraction_method,
                ingested_at_utc,
                document.vendor_name,
                document.invoice_number,
                document.transaction_date,
                document.currency,
                net_cents,
                tax_cents,
                total_cents,
                tax_rate_bps,
                document.expense_category.as_db_key(),
                f64::from(document.confidence_score),
                document.raw_notes,
                row_audit_hash,
                prev_chain_hash,
                chain_hash,
            ],
        )
        .map_err(|err| VaultError::DatabaseStorage {
            details: format!("Failed to insert LedgerRecord into SQLite: {}", err),
        })?;

        let record = LedgerRecord {
            record_id,
            source_file_name,
            source_sha256,
            extraction_method,
            ingested_at_utc: ingested_at_utc.clone(),
            document,
            row_audit_hash: row_audit_hash.clone(),
            prev_chain_hash,
            chain_hash,
        };

        Self::append_audit_event_in_tx(
            &tx,
            &record.record_id,
            "INSERT",
            &ingested_at_utc,
            &row_audit_hash,
            &record,
        )?;

        tx.commit().map_err(|err| VaultError::DatabaseStorage {
            details: format!("Failed to commit SQLite insert transaction: {}", err),
        })?;

        Ok(record)
    }

    pub fn update_record(
        &mut self,
        record_id: &str,
        mut updated_document: ExtractedDocument,
    ) -> Result<LedgerRecord, VaultError> {
        updated_document.validate_and_reconcile()?;

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|err| VaultError::DatabaseStorage {
                details: format!("Failed to begin IMMEDIATE SQLite transaction: {}", err),
            })?;

        let existing_exists: Option<i64> = tx
            .query_row(
                "SELECT seq_no FROM ledger_records WHERE record_id = ?1",
                params![record_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| VaultError::DatabaseStorage {
                details: format!(
                    "Failed to look up record '{}' for update: {}",
                    record_id, err
                ),
            })?;

        if existing_exists.is_none() {
            return Err(VaultError::RecordNotFound {
                record_id: record_id.to_string(),
            });
        }

        let new_row_audit_hash = updated_document.compute_audit_digest(record_id);
        let net_cents = amount_to_cents(updated_document.net_amount);
        let tax_cents = amount_to_cents(updated_document.tax_amount);
        let total_cents = amount_to_cents(updated_document.total_amount);
        let tax_rate_bps = updated_document.tax_rate_percent.map(amount_to_cents);

        tx.execute(
            "UPDATE ledger_records SET
                vendor_name = ?1,
                invoice_number = ?2,
                transaction_date = ?3,
                currency = ?4,
                net_amount_cents = ?5,
                tax_amount_cents = ?6,
                total_amount_cents = ?7,
                tax_rate_bps = ?8,
                expense_category = ?9,
                confidence_score = ?10,
                raw_notes = ?11,
                row_audit_hash = ?12
             WHERE record_id = ?13",
            params![
                updated_document.vendor_name,
                updated_document.invoice_number,
                updated_document.transaction_date,
                updated_document.currency,
                net_cents,
                tax_cents,
                total_cents,
                tax_rate_bps,
                updated_document.expense_category.as_db_key(),
                f64::from(updated_document.confidence_score),
                updated_document.raw_notes,
                new_row_audit_hash,
                record_id,
            ],
        )
        .map_err(|err| VaultError::DatabaseStorage {
            details: format!("Failed to update record '{}' in SQLite: {}", record_id, err),
        })?;

        Self::recompute_active_chain_in_tx(&tx)?;

        let updated_record = Self::fetch_single_record_in_tx(&tx, record_id)?;
        let occurred_at = Utc::now().to_rfc3339();
        Self::append_audit_event_in_tx(
            &tx,
            record_id,
            "UPDATE",
            &occurred_at,
            &updated_record.row_audit_hash,
            &updated_record,
        )?;

        tx.commit().map_err(|err| VaultError::DatabaseStorage {
            details: format!("Failed to commit SQLite update transaction: {}", err),
        })?;

        Ok(updated_record)
    }

    pub fn delete_record(&mut self, record_id: &str) -> Result<DatabaseAuditReport, VaultError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|err| VaultError::DatabaseStorage {
                details: format!("Failed to begin IMMEDIATE SQLite transaction: {}", err),
            })?;

        let record_before_delete = Self::fetch_single_record_in_tx(&tx, record_id)?;

        tx.execute(
            "DELETE FROM ledger_records WHERE record_id = ?1",
            params![record_id],
        )
        .map_err(|err| VaultError::DatabaseStorage {
            details: format!(
                "Failed to delete record '{}' from SQLite: {}",
                record_id, err
            ),
        })?;

        Self::recompute_active_chain_in_tx(&tx)?;

        let occurred_at = Utc::now().to_rfc3339();
        Self::append_audit_event_in_tx(
            &tx,
            record_id,
            "DELETE",
            &occurred_at,
            &record_before_delete.row_audit_hash,
            &record_before_delete,
        )?;

        tx.commit().map_err(|err| VaultError::DatabaseStorage {
            details: format!("Failed to commit SQLite delete transaction: {}", err),
        })?;

        self.verify_full_chain()
    }

    pub fn list_records(&self) -> Result<Vec<LedgerRecord>, VaultError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT
                    record_id, source_file_name, source_sha256, extraction_method, ingested_at_utc,
                    vendor_name, invoice_number, transaction_date, currency,
                    net_amount_cents, tax_amount_cents, total_amount_cents, tax_rate_bps,
                    expense_category, confidence_score, raw_notes,
                    row_audit_hash, prev_chain_hash, chain_hash
                 FROM ledger_records
                 ORDER BY seq_no ASC",
            )
            .map_err(|err| VaultError::DatabaseStorage {
                details: format!("Failed to prepare list_records SQL statement: {}", err),
            })?;

        let mut rows = stmt.query([]).map_err(|err| VaultError::DatabaseStorage {
            details: format!("Failed to execute list_records query: {}", err),
        })?;

        let mut results = Vec::new();
        while let Some(row) = rows.next().map_err(|err| VaultError::DatabaseStorage {
            details: format!("Failed to advance SQLite row cursor: {}", err),
        })? {
            results.push(Self::decode_ledger_row(row)?);
        }

        Ok(results)
    }

    pub fn verify_full_chain(&self) -> Result<DatabaseAuditReport, VaultError> {
        let records = self.list_records()?;
        let mut expected_prev_hash = GENESIS_CHAIN_HASH.to_string();

        for mut record in records.iter().cloned() {
            record.document.validate_and_reconcile()?;

            let computed_row_digest = record.document.compute_audit_digest(&record.record_id);
            if computed_row_digest != record.row_audit_hash {
                return Err(VaultError::DatabaseTamperDetected {
                    record_id: record.record_id,
                    reason: "Row field payload does not match stored row_audit_hash".to_string(),
                    expected_hash: record.row_audit_hash,
                    computed_hash: computed_row_digest,
                });
            }

            if record.prev_chain_hash != expected_prev_hash {
                return Err(VaultError::DatabaseTamperDetected {
                    record_id: record.record_id,
                    reason: "Broken prev_chain_hash link in ledger_records sequence".to_string(),
                    expected_hash: expected_prev_hash,
                    computed_hash: record.prev_chain_hash,
                });
            }

            let computed_chain_hash = compute_record_chain_hash(
                &record.prev_chain_hash,
                &record.record_id,
                &record.source_sha256,
                &record.ingested_at_utc,
                &record.row_audit_hash,
            );

            if computed_chain_hash != record.chain_hash {
                return Err(VaultError::DatabaseTamperDetected {
                    record_id: record.record_id,
                    reason: "Stored chain_hash does not match recomputed SHA-256 chain link"
                        .to_string(),
                    expected_hash: record.chain_hash,
                    computed_hash: computed_chain_hash,
                });
            }

            expected_prev_hash = record.chain_hash;
        }

        let mut event_stmt = self
            .conn
            .prepare(
                "SELECT record_id, event_type, occurred_at_utc, snapshot_json, row_audit_hash, prev_event_hash, event_chain_hash
                 FROM ledger_audit_events
                 ORDER BY event_id ASC",
            )
            .map_err(|err| VaultError::DatabaseStorage {
                details: format!("Failed to prepare audit event verification query: {}", err),
            })?;

        let mut event_rows = event_stmt
            .query([])
            .map_err(|err| VaultError::DatabaseStorage {
                details: format!("Failed to query ledger_audit_events: {}", err),
            })?;

        let mut expected_prev_event_hash = GENESIS_CHAIN_HASH.to_string();
        let mut events_count = 0usize;

        while let Some(row) = event_rows
            .next()
            .map_err(|err| VaultError::DatabaseStorage {
                details: format!("Failed to read audit event row: {}", err),
            })?
        {
            let record_id: String = row.get(0).map_err(|e| VaultError::DatabaseStorage {
                details: e.to_string(),
            })?;
            let event_type: String = row.get(1).map_err(|e| VaultError::DatabaseStorage {
                details: e.to_string(),
            })?;
            let occurred_at_utc: String = row.get(2).map_err(|e| VaultError::DatabaseStorage {
                details: e.to_string(),
            })?;
            let snapshot_json: String = row.get(3).map_err(|e| VaultError::DatabaseStorage {
                details: e.to_string(),
            })?;
            let row_audit_hash: String = row.get(4).map_err(|e| VaultError::DatabaseStorage {
                details: e.to_string(),
            })?;
            let prev_event_hash: String = row.get(5).map_err(|e| VaultError::DatabaseStorage {
                details: e.to_string(),
            })?;
            let event_chain_hash: String = row.get(6).map_err(|e| VaultError::DatabaseStorage {
                details: e.to_string(),
            })?;

            if prev_event_hash != expected_prev_event_hash {
                return Err(VaultError::DatabaseTamperDetected {
                    record_id,
                    reason: "Audit event log prev_event_hash link broken".to_string(),
                    expected_hash: expected_prev_event_hash,
                    computed_hash: prev_event_hash,
                });
            }

            let computed_event_hash = compute_audit_event_hash(
                &prev_event_hash,
                &record_id,
                &event_type,
                &occurred_at_utc,
                &row_audit_hash,
                &snapshot_json,
            );

            if computed_event_hash != event_chain_hash {
                return Err(VaultError::DatabaseTamperDetected {
                    record_id,
                    reason: "Audit event log event_chain_hash mismatch".to_string(),
                    expected_hash: event_chain_hash,
                    computed_hash: computed_event_hash,
                });
            }

            expected_prev_event_hash = event_chain_hash;
            events_count += 1;
        }

        Ok(DatabaseAuditReport {
            verified_at_utc: Utc::now().to_rfc3339(),
            active_records_verified: records.len(),
            audit_events_verified: events_count,
            head_chain_hash: expected_prev_hash,
            head_event_hash: expected_prev_event_hash,
            integrity_valid: true,
        })
    }

    fn recompute_active_chain_in_tx(tx: &Transaction<'_>) -> Result<(), VaultError> {
        let mut stmt = tx
            .prepare(
                "SELECT seq_no, record_id, source_sha256, ingested_at_utc, row_audit_hash
                 FROM ledger_records
                 ORDER BY seq_no ASC",
            )
            .map_err(|err| VaultError::DatabaseStorage {
                details: format!(
                    "Failed to prepare active chain recomputation query: {}",
                    err
                ),
            })?;

        let mut rows = stmt.query([]).map_err(|err| VaultError::DatabaseStorage {
            details: format!("Failed to query rows for chain recomputation: {}", err),
        })?;

        let mut updates: Vec<(i64, String, String)> = Vec::new();
        let mut running_prev_hash = GENESIS_CHAIN_HASH.to_string();

        while let Some(row) = rows.next().map_err(|err| VaultError::DatabaseStorage {
            details: format!("Failed to iterate rows during chain recomputation: {}", err),
        })? {
            let seq_no: i64 = row.get(0).map_err(|e| VaultError::DatabaseStorage {
                details: e.to_string(),
            })?;
            let record_id: String = row.get(1).map_err(|e| VaultError::DatabaseStorage {
                details: e.to_string(),
            })?;
            let source_sha256: String = row.get(2).map_err(|e| VaultError::DatabaseStorage {
                details: e.to_string(),
            })?;
            let ingested_at_utc: String = row.get(3).map_err(|e| VaultError::DatabaseStorage {
                details: e.to_string(),
            })?;
            let row_audit_hash: String = row.get(4).map_err(|e| VaultError::DatabaseStorage {
                details: e.to_string(),
            })?;

            let new_chain_hash = compute_record_chain_hash(
                &running_prev_hash,
                &record_id,
                &source_sha256,
                &ingested_at_utc,
                &row_audit_hash,
            );

            updates.push((seq_no, running_prev_hash, new_chain_hash.clone()));
            running_prev_hash = new_chain_hash;
        }
        drop(rows);
        drop(stmt);

        for (seq_no, prev_hash, chain_hash) in updates {
            tx.execute(
                "UPDATE ledger_records SET prev_chain_hash = ?1, chain_hash = ?2 WHERE seq_no = ?3",
                params![prev_hash, chain_hash, seq_no],
            )
            .map_err(|err| VaultError::DatabaseStorage {
                details: format!(
                    "Failed to update chain hashes at seq_no {}: {}",
                    seq_no, err
                ),
            })?;
        }

        Ok(())
    }

    fn append_audit_event_in_tx(
        tx: &Transaction<'_>,
        record_id: &str,
        event_type: &str,
        occurred_at_utc: &str,
        row_audit_hash: &str,
        record_snapshot: &LedgerRecord,
    ) -> Result<(), VaultError> {
        let prev_event_hash: String = tx
            .query_row(
                "SELECT event_chain_hash FROM ledger_audit_events ORDER BY event_id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| VaultError::DatabaseStorage {
                details: format!("Failed to read previous audit event hash: {}", err),
            })?
            .unwrap_or_else(|| GENESIS_CHAIN_HASH.to_string());

        let snapshot_json =
            serde_json::to_string(record_snapshot).map_err(|err| VaultError::DatabaseStorage {
                details: format!("Failed to serialize LedgerRecord audit snapshot: {}", err),
            })?;

        let event_chain_hash = compute_audit_event_hash(
            &prev_event_hash,
            record_id,
            event_type,
            occurred_at_utc,
            row_audit_hash,
            &snapshot_json,
        );

        tx.execute(
            "INSERT INTO ledger_audit_events (
                record_id, event_type, occurred_at_utc, snapshot_json,
                row_audit_hash, prev_event_hash, event_chain_hash
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record_id,
                event_type,
                occurred_at_utc,
                snapshot_json,
                row_audit_hash,
                prev_event_hash,
                event_chain_hash,
            ],
        )
        .map_err(|err| VaultError::DatabaseStorage {
            details: format!(
                "Failed to append audit event into ledger_audit_events: {}",
                err
            ),
        })?;

        Ok(())
    }

    fn fetch_single_record_in_tx(
        tx: &Transaction<'_>,
        record_id: &str,
    ) -> Result<LedgerRecord, VaultError> {
        let mut stmt = tx
            .prepare(
                "SELECT
                    record_id, source_file_name, source_sha256, extraction_method, ingested_at_utc,
                    vendor_name, invoice_number, transaction_date, currency,
                    net_amount_cents, tax_amount_cents, total_amount_cents, tax_rate_bps,
                    expense_category, confidence_score, raw_notes,
                    row_audit_hash, prev_chain_hash, chain_hash
                 FROM ledger_records
                 WHERE record_id = ?1",
            )
            .map_err(|err| VaultError::DatabaseStorage {
                details: format!("Failed to prepare single record query: {}", err),
            })?;

        let mut rows =
            stmt.query(params![record_id])
                .map_err(|err| VaultError::DatabaseStorage {
                    details: format!("Failed to execute single record query: {}", err),
                })?;

        if let Some(row) = rows.next().map_err(|err| VaultError::DatabaseStorage {
            details: format!("Failed to read single record row: {}", err),
        })? {
            Self::decode_ledger_row(row)
        } else {
            Err(VaultError::RecordNotFound {
                record_id: record_id.to_string(),
            })
        }
    }

    fn decode_ledger_row(row: &rusqlite::Row<'_>) -> Result<LedgerRecord, VaultError> {
        let record_id: String = row.get(0).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;
        let source_file_name: String = row.get(1).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;
        let source_sha256: String = row.get(2).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;
        let extraction_method: String = row.get(3).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;
        let ingested_at_utc: String = row.get(4).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;

        let vendor_name: String = row.get(5).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;
        let invoice_number: Option<String> =
            row.get(6).map_err(|e| VaultError::DatabaseStorage {
                details: e.to_string(),
            })?;
        let transaction_date: String = row.get(7).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;
        let currency: String = row.get(8).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;

        let net_cents: i64 = row.get(9).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;
        let tax_cents: i64 = row.get(10).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;
        let total_cents: i64 = row.get(11).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;
        let tax_rate_bps: Option<i64> = row.get(12).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;

        let category_str: String = row.get(13).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;
        let confidence_f64: f64 = row.get(14).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;
        let raw_notes: Option<String> = row.get(15).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;

        let row_audit_hash: String = row.get(16).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;
        let prev_chain_hash: String = row.get(17).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;
        let chain_hash: String = row.get(18).map_err(|e| VaultError::DatabaseStorage {
            details: e.to_string(),
        })?;

        let expense_category = ExpenseCategory::from_db_key(&category_str)?;

        let document = ExtractedDocument {
            vendor_name,
            invoice_number,
            transaction_date,
            currency,
            net_amount: cents_to_amount(net_cents),
            tax_amount: cents_to_amount(tax_cents),
            total_amount: cents_to_amount(total_cents),
            tax_rate_percent: tax_rate_bps.map(cents_to_amount),
            expense_category,
            confidence_score: confidence_f64 as f32,
            raw_notes,
        };

        Ok(LedgerRecord {
            record_id,
            source_file_name,
            source_sha256,
            extraction_method,
            ingested_at_utc,
            document,
            row_audit_hash,
            prev_chain_hash,
            chain_hash,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDbDir {
        path: PathBuf,
    }

    impl TempDbDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "vault_ledger_{}_{}",
                label,
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn db_path(&self) -> PathBuf {
            self.path.join("vault.sqlite3")
        }
    }

    impl Drop for TempDbDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn sample_doc(vendor: &str, currency: &str, net: f64, tax: f64) -> ExtractedDocument {
        ExtractedDocument {
            vendor_name: vendor.to_string(),
            invoice_number: Some("INV-001".to_string()),
            transaction_date: "2026-10-20".to_string(),
            currency: currency.to_string(),
            net_amount: net,
            tax_amount: tax,
            total_amount: net + tax,
            tax_rate_percent: if tax > 0.0 {
                Some(((tax / net) * 100.0).round())
            } else {
                Some(0.0)
            },
            expense_category: ExpenseCategory::SoftwareAndSaaS,
            confidence_score: 0.94,
            raw_notes: None,
        }
    }

    #[test]
    fn sha256_chain_verifies_across_insert_update_delete_and_reopen() {
        let tmp = TempDbDir::new("chain_lifecycle");
        let db_path = tmp.db_path();

        let (mut store, initial_audit) = SqliteLedgerStore::open_and_verify(&db_path).unwrap();
        assert!(initial_audit.integrity_valid);
        assert_eq!(initial_audit.active_records_verified, 0);
        assert_eq!(initial_audit.audit_events_verified, 0);
        assert_eq!(initial_audit.head_chain_hash, GENESIS_CHAIN_HASH);

        let rec1 = store
            .insert_record(
                "rec-1".to_string(),
                "inv1.pdf".to_string(),
                "1".repeat(64),
                "PDF_NATIVE_STREAM".to_string(),
                "2026-10-20T10:00:00Z".to_string(),
                sample_doc("Vendor One", "MAD", 100.0, 20.0),
            )
            .unwrap();
        assert_eq!(rec1.prev_chain_hash, GENESIS_CHAIN_HASH);

        let rec2 = store
            .insert_record(
                "rec-2".to_string(),
                "inv2.pdf".to_string(),
                "2".repeat(64),
                "PDF_NATIVE_STREAM".to_string(),
                "2026-10-21T10:00:00Z".to_string(),
                sample_doc("Vendor Two", "USD", 50.0, 0.0),
            )
            .unwrap();
        assert_eq!(rec2.prev_chain_hash, rec1.chain_hash);

        let audit_after_inserts = store.verify_full_chain().unwrap();
        assert!(audit_after_inserts.integrity_valid);
        assert_eq!(audit_after_inserts.active_records_verified, 2);
        assert_eq!(audit_after_inserts.audit_events_verified, 2);
        assert_eq!(audit_after_inserts.head_chain_hash, rec2.chain_hash);

        // Update first record: active chain must recompute cleanly and append an UPDATE audit event.
        let updated_rec1 = store
            .update_record("rec-1", sample_doc("Vendor One Edited", "MAD", 200.0, 40.0))
            .unwrap();
        assert_ne!(updated_rec1.chain_hash, rec1.chain_hash);

        let audit_after_update = store.verify_full_chain().unwrap();
        assert!(audit_after_update.integrity_valid);
        assert_eq!(audit_after_update.active_records_verified, 2);
        assert_eq!(audit_after_update.audit_events_verified, 3);

        // Delete first record: remaining record should now chain directly from GENESIS_CHAIN_HASH.
        let audit_after_delete = store.delete_record("rec-1").unwrap();
        assert!(audit_after_delete.integrity_valid);
        assert_eq!(audit_after_delete.active_records_verified, 1);
        assert_eq!(audit_after_delete.audit_events_verified, 4);

        let remaining = store.list_records().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].record_id, "rec-2");
        assert_eq!(remaining[0].prev_chain_hash, GENESIS_CHAIN_HASH);

        drop(store);

        // Re-opening from disk must pass full cryptographic verification.
        let (_reopened, reopened_audit) = SqliteLedgerStore::open_and_verify(&db_path).unwrap();
        assert!(reopened_audit.integrity_valid);
        assert_eq!(reopened_audit.active_records_verified, 1);
        assert_eq!(reopened_audit.audit_events_verified, 4);
    }

    #[test]
    fn sha256_chain_detects_out_of_band_row_tampering() {
        let tmp = TempDbDir::new("tamper_row");
        let db_path = tmp.db_path();

        let (mut store, _) = SqliteLedgerStore::open_and_verify(&db_path).unwrap();
        store
            .insert_record(
                "rec-1".to_string(),
                "inv1.pdf".to_string(),
                "a".repeat(64),
                "PDF_NATIVE_STREAM".to_string(),
                "2026-10-20T10:00:00Z".to_string(),
                sample_doc("Legit Vendor", "EUR", 100.0, 20.0),
            )
            .unwrap();
        drop(store);

        // Tamper with vendor_name out-of-band directly in SQLite.
        let raw_conn = Connection::open(&db_path).unwrap();
        raw_conn
            .execute(
                "UPDATE ledger_records SET vendor_name = 'Tampered Vendor' WHERE record_id = 'rec-1'",
                [],
            )
            .unwrap();
        drop(raw_conn);

        let err = SqliteLedgerStore::open_and_verify(&db_path).err().unwrap();
        assert!(matches!(
            err,
            VaultError::DatabaseTamperDetected { ref record_id, .. } if record_id == "rec-1"
        ));
    }

    #[test]
    fn sha256_chain_detects_broken_chain_hash_link() {
        let tmp = TempDbDir::new("tamper_chain");
        let db_path = tmp.db_path();

        let (mut store, _) = SqliteLedgerStore::open_and_verify(&db_path).unwrap();
        store
            .insert_record(
                "rec-1".to_string(),
                "inv1.pdf".to_string(),
                "a".repeat(64),
                "PDF_NATIVE_STREAM".to_string(),
                "2026-10-20T10:00:00Z".to_string(),
                sample_doc("Vendor A", "USD", 100.0, 0.0),
            )
            .unwrap();
        store
            .insert_record(
                "rec-2".to_string(),
                "inv2.pdf".to_string(),
                "b".repeat(64),
                "PDF_NATIVE_STREAM".to_string(),
                "2026-10-21T10:00:00Z".to_string(),
                sample_doc("Vendor B", "USD", 200.0, 0.0),
            )
            .unwrap();
        drop(store);

        // Corrupt prev_chain_hash on second record.
        let raw_conn = Connection::open(&db_path).unwrap();
        raw_conn
            .execute(
                "UPDATE ledger_records SET prev_chain_hash = ?1 WHERE record_id = 'rec-2'",
                params!["f".repeat(64)],
            )
            .unwrap();
        drop(raw_conn);

        let err = SqliteLedgerStore::open_and_verify(&db_path).err().unwrap();
        assert!(matches!(
            err,
            VaultError::DatabaseTamperDetected { ref record_id, .. } if record_id == "rec-2"
        ));
    }

    #[test]
    fn sha256_chain_detects_audit_event_log_tampering() {
        let tmp = TempDbDir::new("tamper_audit");
        let db_path = tmp.db_path();

        let (mut store, _) = SqliteLedgerStore::open_and_verify(&db_path).unwrap();
        store
            .insert_record(
                "rec-1".to_string(),
                "inv1.pdf".to_string(),
                "c".repeat(64),
                "PDF_NATIVE_STREAM".to_string(),
                "2026-10-20T10:00:00Z".to_string(),
                sample_doc("Vendor C", "MAD", 300.0, 60.0),
            )
            .unwrap();
        drop(store);

        // Tamper with snapshot_json in the append-only ledger_audit_events table.
        let raw_conn = Connection::open(&db_path).unwrap();
        raw_conn
            .execute(
                "UPDATE ledger_audit_events SET snapshot_json = '{\"forged\":true}' WHERE record_id = 'rec-1'",
                [],
            )
            .unwrap();
        drop(raw_conn);

        let err = SqliteLedgerStore::open_and_verify(&db_path).err().unwrap();
        assert!(matches!(
            err,
            VaultError::DatabaseTamperDetected { ref record_id, .. } if record_id == "rec-1"
        ));
    }
}
