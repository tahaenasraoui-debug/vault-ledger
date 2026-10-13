#![forbid(unsafe_code)]

use crate::error::VaultError;
use crate::schema::{round_currency, LedgerRecord};
use csv::WriterBuilder;
use serde::Serialize;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

#[derive(Debug, Serialize)]
struct DoubleEntryJournalRow<'a> {
    journal_entry_id: String,
    record_id: &'a str,
    transaction_date: &'a str,
    vendor_name: &'a str,
    invoice_number: &'a str,
    gl_account_code: &'a str,
    gl_account_name: &'a str,
    entry_side: &'static str,
    currency: &'a str,
    debit_amount: String,
    credit_amount: String,
    tax_rate_percent: String,
    confidence_score: String,
    source_file_sha256: &'a str,
    row_audit_hash: &'a str,
    sqlite_chain_hash: &'a str,
    memo: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportSummary {
    pub destination_path: String,
    pub documents_exported: usize,
    pub journal_rows_written: usize,
    pub total_debits: f64,
    pub total_credits: f64,
}

pub fn export_double_entry_ledger(
    records: &[LedgerRecord],
    destination_path: &Path,
) -> Result<ExportSummary, VaultError> {
    let parent_dir = destination_path.parent().unwrap_or_else(|| Path::new("."));
    let temp_file_name = format!(".vaultledger_export_{}.csv.tmp", uuid::Uuid::new_v4());
    let temp_path = parent_dir.join(temp_file_name);

    let temp_file = File::create(&temp_path).map_err(|source| VaultError::Io {
        path: temp_path.clone(),
        source,
    })?;

    let mut csv_writer = WriterBuilder::new()
        .has_headers(true)
        .delimiter(b',')
        .from_writer(temp_file);

    let mut journal_rows_written = 0usize;
    let mut total_debits = 0.0_f64;
    let mut total_credits = 0.0_f64;

    for (idx, record) in records.iter().enumerate() {
        let doc = &record.document;
        let journal_prefix = format!("JE-{:05}", idx + 1);
        let invoice_ref = doc.invoice_number.as_deref().unwrap_or("UNNUMBERED");
        let tax_rate_str = format!("{:.2}", doc.tax_rate_percent.unwrap_or(0.0));
        let confidence_str = format!("{:.2}", doc.confidence_score);
        let base_memo = doc
            .raw_notes
            .as_deref()
            .unwrap_or(record.source_file_name.as_str());

        let net_row = DoubleEntryJournalRow {
            journal_entry_id: format!("{}-1", journal_prefix),
            record_id: &record.record_id,
            transaction_date: &doc.transaction_date,
            vendor_name: &doc.vendor_name,
            invoice_number: invoice_ref,
            gl_account_code: doc.expense_category.general_ledger_code(),
            gl_account_name: doc.expense_category.account_label(),
            entry_side: "DEBIT",
            currency: &doc.currency,
            debit_amount: format!("{:.2}", doc.net_amount),
            credit_amount: "0.00".to_string(),
            tax_rate_percent: tax_rate_str.clone(),
            confidence_score: confidence_str.clone(),
            source_file_sha256: &record.source_sha256,
            row_audit_hash: &record.row_audit_hash,
            sqlite_chain_hash: &record.chain_hash,
            memo: format!("Net Expense | {}", base_memo),
        };
        csv_writer.serialize(&net_row).map_err(|err| VaultError::CsvExport {
            path: temp_path.clone(),
            details: err.to_string(),
        })?;
        journal_rows_written += 1;
        total_debits = round_currency(total_debits + doc.net_amount);

        if doc.tax_amount > 0.0 {
            let tax_row = DoubleEntryJournalRow {
                journal_entry_id: format!("{}-2", journal_prefix),
                record_id: &record.record_id,
                transaction_date: &doc.transaction_date,
                vendor_name: &doc.vendor_name,
                invoice_number: invoice_ref,
                gl_account_code: "2210-VAT-INPUT",
                gl_account_name: "Recoverable Input VAT / Sales Tax",
                entry_side: "DEBIT",
                currency: &doc.currency,
                debit_amount: format!("{:.2}", doc.tax_amount),
                credit_amount: "0.00".to_string(),
                tax_rate_percent: tax_rate_str.clone(),
                confidence_score: confidence_str.clone(),
                source_file_sha256: &record.source_sha256,
                row_audit_hash: &record.row_audit_hash,
                sqlite_chain_hash: &record.chain_hash,
                memo: format!("Input Tax ({}%) | {}", tax_rate_str, base_memo),
            };
            csv_writer.serialize(&tax_row).map_err(|err| VaultError::CsvExport {
                path: temp_path.clone(),
                details: err.to_string(),
            })?;
            journal_rows_written += 1;
            total_debits = round_currency(total_debits + doc.tax_amount);
        }

        let credit_leg_num = if doc.tax_amount > 0.0 { 3 } else { 2 };
        let credit_row = DoubleEntryJournalRow {
            journal_entry_id: format!("{}-{}", journal_prefix, credit_leg_num),
            record_id: &record.record_id,
            transaction_date: &doc.transaction_date,
            vendor_name: &doc.vendor_name,
            invoice_number: invoice_ref,
            gl_account_code: "2000-AP-CLEARING",
            gl_account_name: "Accounts Payable / Freelance Clearing",
            entry_side: "CREDIT",
            currency: &doc.currency,
            debit_amount: "0.00".to_string(),
            credit_amount: format!("{:.2}", doc.total_amount),
            tax_rate_percent: tax_rate_str,
            confidence_score: confidence_str,
            source_file_sha256: &record.source_sha256,
            row_audit_hash: &record.row_audit_hash,
            sqlite_chain_hash: &record.chain_hash,
            memo: format!("Total Payable | {}", base_memo),
        };
        csv_writer.serialize(&credit_row).map_err(|err| VaultError::CsvExport {
            path: temp_path.clone(),
            details: err.to_string(),
        })?;
        journal_rows_written += 1;
        total_credits = round_currency(total_credits + doc.total_amount);
    }

    if (total_debits - total_credits).abs() > 0.01 {
        let _ = fs::remove_file(&temp_path);
        return Err(VaultError::AccountingValidation {
            violation: format!(
                "Trial balance invariant failed during CSV export: total_debits ({:.2}) != total_credits ({:.2})",
                total_debits, total_credits
            ),
        });
    }

    csv_writer.flush().map_err(|source| VaultError::Io {
        path: temp_path.clone(),
        source,
    })?;

    let mut inner_file = csv_writer.into_inner().map_err(|err| VaultError::CsvExport {
        path: temp_path.clone(),
        details: format!("Failed to flush CSV writer buffer: {}", err),
    })?;

    inner_file.flush().map_err(|source| VaultError::Io {
        path: temp_path.clone(),
        source,
    })?;

    inner_file.sync_all().map_err(|source| VaultError::Io {
        path: temp_path.clone(),
        source,
    })?;

    fs::rename(&temp_path, destination_path).map_err(|source| VaultError::Io {
        path: destination_path.to_path_buf(),
        source,
    })?;

    Ok(ExportSummary {
        destination_path: destination_path.display().to_string(),
        documents_exported: records.len(),
        journal_rows_written,
        total_debits,
        total_credits,
    })
}
