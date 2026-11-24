#![forbid(unsafe_code)]

use crate::error::VaultError;
use crate::expand_tilde_path;
use crate::schema::{amount_to_cents, cents_to_amount, LedgerRecord};
use csv::WriterBuilder;
use serde::Serialize;
use std::collections::BTreeMap;
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

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CurrencyTrialBalance {
    pub currency: String,
    pub documents_exported: usize,
    pub total_debits: f64,
    pub total_credits: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportSummary {
    pub destination_path: String,
    pub documents_exported: usize,
    pub journal_rows_written: usize,
    pub totals_by_currency: Vec<CurrencyTrialBalance>,
    pub total_debits: Option<f64>,
    pub total_credits: Option<f64>,
}

pub fn export_double_entry_ledger(
    records: &[LedgerRecord],
    destination_path: &Path,
) -> Result<ExportSummary, VaultError> {
    let expanded_destination = expand_tilde_path(destination_path)?;
    let parent_dir = expanded_destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
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
    // Keyed by normalized ISO-4217 currency code: (documents_exported, debit_cents, credit_cents).
    let mut currency_totals: BTreeMap<String, (usize, i64, i64)> = BTreeMap::new();

    for (idx, record) in records.iter().enumerate() {
        let doc = &record.document;
        let currency_key = doc.currency.trim().to_ascii_uppercase();
        let totals_entry = currency_totals.entry(currency_key).or_insert((0, 0, 0));
        totals_entry.0 += 1;

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
        csv_writer
            .serialize(&net_row)
            .map_err(|err| VaultError::CsvExport {
                path: temp_path.clone(),
                details: err.to_string(),
            })?;
        journal_rows_written += 1;
        totals_entry.1 += amount_to_cents(doc.net_amount);

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
            csv_writer
                .serialize(&tax_row)
                .map_err(|err| VaultError::CsvExport {
                    path: temp_path.clone(),
                    details: err.to_string(),
                })?;
            journal_rows_written += 1;
            totals_entry.1 += amount_to_cents(doc.tax_amount);
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
        csv_writer
            .serialize(&credit_row)
            .map_err(|err| VaultError::CsvExport {
                path: temp_path.clone(),
                details: err.to_string(),
            })?;
        journal_rows_written += 1;
        totals_entry.2 += amount_to_cents(doc.total_amount);
    }

    let mut totals_by_currency = Vec::with_capacity(currency_totals.len());
    for (currency, (doc_count, debit_cents, credit_cents)) in currency_totals {
        if (debit_cents - credit_cents).abs() > 1 {
            let _ = fs::remove_file(&temp_path);
            return Err(VaultError::AccountingValidation {
                violation: format!(
                    "Trial balance invariant failed for currency '{}' during CSV export: total_debits ({:.2}) != total_credits ({:.2})",
                    currency,
                    cents_to_amount(debit_cents),
                    cents_to_amount(credit_cents)
                ),
            });
        }

        totals_by_currency.push(CurrencyTrialBalance {
            currency,
            documents_exported: doc_count,
            total_debits: cents_to_amount(debit_cents),
            total_credits: cents_to_amount(credit_cents),
        });
    }

    let (total_debits, total_credits) = match totals_by_currency.as_slice() {
        [] => (Some(0.0), Some(0.0)),
        [single] => (Some(single.total_debits), Some(single.total_credits)),
        _ => (None, None),
    };

    csv_writer.flush().map_err(|source| VaultError::Io {
        path: temp_path.clone(),
        source,
    })?;

    let mut inner_file = csv_writer
        .into_inner()
        .map_err(|err| VaultError::CsvExport {
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

    fs::rename(&temp_path, &expanded_destination).map_err(|source| VaultError::Io {
        path: expanded_destination.clone(),
        source,
    })?;

    Ok(ExportSummary {
        destination_path: expanded_destination.display().to_string(),
        documents_exported: records.len(),
        journal_rows_written,
        totals_by_currency,
        total_debits,
        total_credits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ExpenseCategory, ExtractedDocument, GENESIS_CHAIN_HASH};
    use std::path::PathBuf;

    struct TempTestDir {
        path: PathBuf,
    }

    impl TempTestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "vault_ledger_{}_{}",
                label,
                uuid::Uuid::new_v4()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempTestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn make_record(
        id: &str,
        vendor: &str,
        currency: &str,
        net: f64,
        tax: f64,
        total: f64,
        category: ExpenseCategory,
    ) -> LedgerRecord {
        let doc = ExtractedDocument {
            vendor_name: vendor.to_string(),
            invoice_number: Some(format!("INV-{}", id)),
            transaction_date: "2026-10-15".to_string(),
            currency: currency.to_string(),
            net_amount: net,
            tax_amount: tax,
            total_amount: total,
            tax_rate_percent: if tax > 0.0 { Some(20.0) } else { Some(0.0) },
            expense_category: category,
            confidence_score: 0.95,
            raw_notes: None,
        };
        let row_audit_hash = doc.compute_audit_digest(id);
        LedgerRecord {
            record_id: id.to_string(),
            source_file_name: format!("{}.pdf", id),
            source_sha256: "a".repeat(64),
            extraction_method: "PDF_NATIVE_STREAM".to_string(),
            ingested_at_utc: "2026-10-15T12:00:00Z".to_string(),
            document: doc,
            row_audit_hash,
            prev_chain_hash: GENESIS_CHAIN_HASH.to_string(),
            chain_hash: "b".repeat(64),
        }
    }

    #[test]
    fn single_currency_export_writes_balanced_double_entry_csv() {
        let dir = TempTestDir::new("export_single");
        let dest = dir.path.join("journal.csv");

        let records = vec![
            make_record(
                "rec-1",
                "Maroc Telecom",
                "MAD",
                250.00,
                50.00,
                300.00,
                ExpenseCategory::Utilities,
            ),
            make_record(
                "rec-2",
                "Zero Tax Supplier",
                "MAD",
                120.00,
                0.00,
                120.00,
                ExpenseCategory::OfficeSupplies,
            ),
        ];

        let summary = export_double_entry_ledger(&records, &dest).unwrap();
        assert_eq!(summary.documents_exported, 2);
        // 3 legs for rec-1 (net + VAT + AP) + 2 legs for rec-2 (net + AP)
        assert_eq!(summary.journal_rows_written, 5);
        assert_eq!(summary.total_debits, Some(420.00));
        assert_eq!(summary.total_credits, Some(420.00));
        assert_eq!(
            summary.totals_by_currency,
            vec![CurrencyTrialBalance {
                currency: "MAD".to_string(),
                documents_exported: 2,
                total_debits: 420.00,
                total_credits: 420.00,
            }]
        );

        let csv_content = fs::read_to_string(&dest).unwrap();
        let lines: Vec<&str> = csv_content.lines().collect();
        assert_eq!(lines.len(), 6); // header + 5 rows
        assert!(lines[0].starts_with("journal_entry_id,record_id,transaction_date,"));
        assert!(csv_content.contains("6410-UTIL"));
        assert!(csv_content.contains("2210-VAT-INPUT"));
        assert!(csv_content.contains("2000-AP-CLEARING"));

        // Verify no stray temp files remain in destination directory.
        let entries: Vec<_> = fs::read_dir(&dir.path).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn mixed_currency_export_groups_totals_without_blind_summing() {
        let dir = TempTestDir::new("export_mixed");
        let dest = dir.path.join("mixed_journal.csv");

        let records = vec![
            make_record(
                "rec-usd-1",
                "Cloud Host",
                "USD",
                100.00,
                0.00,
                100.00,
                ExpenseCategory::SoftwareAndSaaS,
            ),
            make_record(
                "rec-mad-1",
                "Maroc Telecom",
                "MAD",
                250.00,
                50.00,
                300.00,
                ExpenseCategory::Utilities,
            ),
            make_record(
                "rec-eur-1",
                "Hetzner",
                "EUR",
                40.00,
                8.00,
                48.00,
                ExpenseCategory::SoftwareAndSaaS,
            ),
            make_record(
                "rec-usd-2",
                "Domain Registrar",
                "USD",
                25.50,
                0.00,
                25.50,
                ExpenseCategory::SoftwareAndSaaS,
            ),
        ];

        let summary = export_double_entry_ledger(&records, &dest).unwrap();
        assert_eq!(summary.documents_exported, 4);
        assert_eq!(summary.journal_rows_written, 10);
        // Mixed-currency exports must never report a single cross-currency combined sum.
        assert_eq!(summary.total_debits, None);
        assert_eq!(summary.total_credits, None);
        assert_eq!(
            summary.totals_by_currency,
            vec![
                CurrencyTrialBalance {
                    currency: "EUR".to_string(),
                    documents_exported: 1,
                    total_debits: 48.00,
                    total_credits: 48.00,
                },
                CurrencyTrialBalance {
                    currency: "MAD".to_string(),
                    documents_exported: 1,
                    total_debits: 300.00,
                    total_credits: 300.00,
                },
                CurrencyTrialBalance {
                    currency: "USD".to_string(),
                    documents_exported: 2,
                    total_debits: 125.50,
                    total_credits: 125.50,
                },
            ]
        );
    }

    #[test]
    fn mixed_currency_export_rejects_offsetting_cross_currency_imbalances() {
        let dir = TempTestDir::new("export_imbalanced");
        let dest = dir.path.join("imbalanced.csv");

        // USD is off by -1.00 and EUR is off by +1.00; naive cross-currency totals would cancel out!
        let records = vec![
            make_record(
                "rec-usd",
                "Broken USD",
                "USD",
                100.00,
                0.00,
                101.00,
                ExpenseCategory::Other,
            ),
            make_record(
                "rec-eur",
                "Broken EUR",
                "EUR",
                101.00,
                0.00,
                100.00,
                ExpenseCategory::Other,
            ),
        ];

        let err = export_double_entry_ledger(&records, &dest).unwrap_err();
        match err {
            VaultError::AccountingValidation { violation } => {
                assert!(
                    violation.contains("EUR") || violation.contains("USD"),
                    "expected currency name in violation: {}",
                    violation
                );
            }
            other => panic!("expected AccountingValidation error, got {:?}", other),
        }

        // Destination must not be written and temp file must be cleaned up.
        assert!(!dest.exists());
        let entries: Vec<_> = fs::read_dir(&dir.path).unwrap().collect();
        assert!(entries.is_empty());
    }

    #[test]
    fn export_expands_leading_tilde_destination_path() {
        let Some(home) = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
            .map(PathBuf::from)
        else {
            return;
        };

        let missing_sub = format!("vault_ledger_nonexistent_{}", uuid::Uuid::new_v4());
        let tilde_dest = PathBuf::from(format!("~/{}/journal.csv", missing_sub));
        let err = export_double_entry_ledger(&[], &tilde_dest).unwrap_err();
        match err {
            VaultError::Io { path, .. } => {
                assert!(
                    path.starts_with(&home),
                    "expected expanded path under '{}', got '{}'",
                    home.display(),
                    path.display()
                );
            }
            other => panic!("expected Io error for missing parent dir, got {:?}", other),
        }
    }
}
