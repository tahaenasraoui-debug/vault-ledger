#![forbid(unsafe_code)]

use crate::error::VaultError;
use chrono::NaiveDate;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const GENESIS_CHAIN_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct ExtractedDocument {
    pub vendor_name: String,
    pub invoice_number: Option<String>,
    pub transaction_date: String,
    pub currency: String,
    pub net_amount: f64,
    pub tax_amount: f64,
    pub total_amount: f64,
    pub tax_rate_percent: Option<f64>,
    pub expense_category: ExpenseCategory,
    pub confidence_score: f32,
    pub raw_notes: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum ExpenseCategory {
    SoftwareAndSaaS,
    HardwareAndEquipment,
    OfficeSupplies,
    TravelAndTransport,
    MealsAndEntertainment,
    ProfessionalServices,
    Utilities,
    Other,
}

impl ExpenseCategory {
    pub fn general_ledger_code(&self) -> &'static str {
        match self {
            ExpenseCategory::SoftwareAndSaaS => "6110-SAAS",
            ExpenseCategory::HardwareAndEquipment => "6120-EQUIP",
            ExpenseCategory::OfficeSupplies => "6130-SUPPLIES",
            ExpenseCategory::TravelAndTransport => "6210-TRAVEL",
            ExpenseCategory::MealsAndEntertainment => "6220-MEALS",
            ExpenseCategory::ProfessionalServices => "6310-PROFSERV",
            ExpenseCategory::Utilities => "6410-UTIL",
            ExpenseCategory::Other => "6990-MISC",
        }
    }

    pub fn account_label(&self) -> &'static str {
        match self {
            ExpenseCategory::SoftwareAndSaaS => "Software & Cloud Subscriptions",
            ExpenseCategory::HardwareAndEquipment => "Computer Hardware & Equipment",
            ExpenseCategory::OfficeSupplies => "Office Supplies & Stationery",
            ExpenseCategory::TravelAndTransport => "Business Travel & Transport",
            ExpenseCategory::MealsAndEntertainment => "Meals & Client Entertainment",
            ExpenseCategory::ProfessionalServices => "Legal, Accounting & Consulting",
            ExpenseCategory::Utilities => "Telecommunications & Utilities",
            ExpenseCategory::Other => "Miscellaneous Operating Expenses",
        }
    }

    pub fn as_db_key(&self) -> &'static str {
        match self {
            ExpenseCategory::SoftwareAndSaaS => "SoftwareAndSaaS",
            ExpenseCategory::HardwareAndEquipment => "HardwareAndEquipment",
            ExpenseCategory::OfficeSupplies => "OfficeSupplies",
            ExpenseCategory::TravelAndTransport => "TravelAndTransport",
            ExpenseCategory::MealsAndEntertainment => "MealsAndEntertainment",
            ExpenseCategory::ProfessionalServices => "ProfessionalServices",
            ExpenseCategory::Utilities => "Utilities",
            ExpenseCategory::Other => "Other",
        }
    }

    pub fn from_db_key(key: &str) -> Result<Self, VaultError> {
        match key {
            "SoftwareAndSaaS" => Ok(ExpenseCategory::SoftwareAndSaaS),
            "HardwareAndEquipment" => Ok(ExpenseCategory::HardwareAndEquipment),
            "OfficeSupplies" => Ok(ExpenseCategory::OfficeSupplies),
            "TravelAndTransport" => Ok(ExpenseCategory::TravelAndTransport),
            "MealsAndEntertainment" => Ok(ExpenseCategory::MealsAndEntertainment),
            "ProfessionalServices" => Ok(ExpenseCategory::ProfessionalServices),
            "Utilities" => Ok(ExpenseCategory::Utilities),
            "Other" => Ok(ExpenseCategory::Other),
            unknown => Err(VaultError::DatabaseStorage {
                details: format!("Unrecognized ExpenseCategory database key: '{}'", unknown),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerRecord {
    pub record_id: String,
    pub source_file_name: String,
    pub source_sha256: String,
    pub extraction_method: String,
    pub ingested_at_utc: String,
    pub document: ExtractedDocument,
    pub row_audit_hash: String,
    pub prev_chain_hash: String,
    pub chain_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseAuditReport {
    pub verified_at_utc: String,
    pub active_records_verified: usize,
    pub audit_events_verified: usize,
    pub head_chain_hash: String,
    pub head_event_hash: String,
    pub integrity_valid: bool,
}

const VALID_ISO_4217_CURRENCIES: &[&str] = &[
    "USD", "EUR", "MAD", "GBP", "CAD", "CHF", "JPY", "AUD", "NZD", "SEK", "NOK", "DKK", "PLN",
    "CZK", "HUF", "RON", "BGN", "TRY", "AED", "SAR", "QAR", "KWD", "BHD", "OMR", "JOD", "EGP",
    "ZAR", "NGN", "KES", "GHS", "XOF", "XAF", "TND", "DZD", "CNY", "HKD", "SGD", "INR", "KRW",
    "TWD", "THB", "MYR", "IDR", "PHP", "VND", "BRL", "MXN", "ARS", "CLP", "COP", "PEN",
];

impl ExtractedDocument {
    pub fn validate_and_reconcile(&mut self) -> Result<(), VaultError> {
        self.vendor_name = self.vendor_name.trim().to_string();
        if self.vendor_name.is_empty() {
            return Err(VaultError::AccountingValidation {
                violation: "vendor_name cannot be empty.".to_string(),
            });
        }

        if let Some(ref inv) = self.invoice_number {
            let trimmed = inv.trim().to_string();
            if trimmed.is_empty()
                || trimmed.eq_ignore_ascii_case("null")
                || trimmed.eq_ignore_ascii_case("none")
            {
                self.invoice_number = None;
            } else {
                self.invoice_number = Some(trimmed);
            }
        }

        let trimmed_date = self.transaction_date.trim();
        NaiveDate::parse_from_str(trimmed_date, "%Y-%m-%d").map_err(|err| {
            VaultError::AccountingValidation {
                violation: format!(
                    "transaction_date '{}' is not a valid ISO-8601 calendar date (YYYY-MM-DD): {}",
                    trimmed_date, err
                ),
            }
        })?;
        self.transaction_date = trimmed_date.to_string();

        let normalized_currency = self.currency.trim().to_ascii_uppercase();
        if !VALID_ISO_4217_CURRENCIES.contains(&normalized_currency.as_str()) {
            return Err(VaultError::AccountingValidation {
                violation: format!(
                    "currency '{}' is not a recognized ISO-4217 3-letter currency code.",
                    normalized_currency
                ),
            });
        }
        self.currency = normalized_currency;

        if !self.net_amount.is_finite()
            || !self.tax_amount.is_finite()
            || !self.total_amount.is_finite()
        {
            return Err(VaultError::AccountingValidation {
                violation: "Monetary amounts (net, tax, total) must be finite numbers.".to_string(),
            });
        }

        self.net_amount = round_currency(self.net_amount);
        self.tax_amount = round_currency(self.tax_amount);
        self.total_amount = round_currency(self.total_amount);

        if self.net_amount < 0.0 || self.tax_amount < 0.0 || self.total_amount <= 0.0 {
            return Err(VaultError::AccountingValidation {
                violation: format!(
                    "Invalid negative or zero monetary amounts: net={:.2}, tax={:.2}, total={:.2}",
                    self.net_amount, self.tax_amount, self.total_amount
                ),
            });
        }

        if self.net_amount == 0.0 && self.total_amount > self.tax_amount {
            self.net_amount = round_currency(self.total_amount - self.tax_amount);
        }

        let computed_total = round_currency(self.net_amount + self.tax_amount);
        let discrepancy = (computed_total - self.total_amount).abs();

        if discrepancy > 0.02 {
            return Err(VaultError::AccountingValidation {
                violation: format!(
                    "Unbalanced invoice math: net ({:.2}) + tax ({:.2}) = {:.2}, which differs from total ({:.2}) by {:.2}",
                    self.net_amount, self.tax_amount, computed_total, self.total_amount, discrepancy
                ),
            });
        } else if discrepancy > 0.0 {
            self.net_amount = round_currency(self.total_amount - self.tax_amount);
        }

        if self.tax_amount == 0.0 {
            self.tax_rate_percent = Some(0.0);
        } else if self.net_amount > 0.0 {
            let implied_rate = round_currency((self.tax_amount / self.net_amount) * 100.0);
            match self.tax_rate_percent {
                Some(reported_rate) if (reported_rate - implied_rate).abs() <= 1.0 => {
                    self.tax_rate_percent = Some(round_currency(reported_rate));
                }
                Some(_) | None => {
                    self.tax_rate_percent = Some(implied_rate);
                }
            }
        }

        if !self.confidence_score.is_finite() {
            self.confidence_score = 0.5;
        } else {
            self.confidence_score = self.confidence_score.clamp(0.0, 1.0);
        }

        Ok(())
    }

    pub fn compute_audit_digest(&self, record_id: &str) -> String {
        let net_cents = amount_to_cents(self.net_amount);
        let tax_cents = amount_to_cents(self.tax_amount);
        let total_cents = amount_to_cents(self.total_amount);
        let rate_bps = self.tax_rate_percent.map(amount_to_cents).unwrap_or(0);

        let mut hasher = Sha256::new();
        hasher.update(record_id.as_bytes());
        hasher.update(b"|");
        hasher.update(self.vendor_name.as_bytes());
        hasher.update(b"|");
        hasher.update(self.invoice_number.as_deref().unwrap_or("NONE").as_bytes());
        hasher.update(b"|");
        hasher.update(self.transaction_date.as_bytes());
        hasher.update(b"|");
        hasher.update(self.currency.as_bytes());
        hasher.update(b"|");
        hasher.update(format!("{}|{}|{}|{}", net_cents, tax_cents, total_cents, rate_bps).as_bytes());
        hasher.update(b"|");
        hasher.update(self.expense_category.general_ledger_code().as_bytes());
        hex::encode(hasher.finalize())
    }
}

pub fn compute_record_chain_hash(
    prev_chain_hash: &str,
    record_id: &str,
    source_sha256: &str,
    ingested_at_utc: &str,
    row_audit_hash: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev_chain_hash.as_bytes());
    hasher.update(b"|");
    hasher.update(record_id.as_bytes());
    hasher.update(b"|");
    hasher.update(source_sha256.as_bytes());
    hasher.update(b"|");
    hasher.update(ingested_at_utc.as_bytes());
    hasher.update(b"|");
    hasher.update(row_audit_hash.as_bytes());
    hex::encode(hasher.finalize())
}

pub fn compute_audit_event_hash(
    prev_event_hash: &str,
    record_id: &str,
    event_type: &str,
    occurred_at_utc: &str,
    row_audit_hash: &str,
    snapshot_json: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev_event_hash.as_bytes());
    hasher.update(b"|");
    hasher.update(record_id.as_bytes());
    hasher.update(b"|");
    hasher.update(event_type.as_bytes());
    hasher.update(b"|");
    hasher.update(occurred_at_utc.as_bytes());
    hasher.update(b"|");
    hasher.update(row_audit_hash.as_bytes());
    hasher.update(b"|");
    hasher.update(snapshot_json.as_bytes());
    hex::encode(hasher.finalize())
}

#[inline]
pub fn round_currency(val: f64) -> f64 {
    (val * 100.0).round() / 100.0
}

#[inline]
pub fn amount_to_cents(val: f64) -> i64 {
    (val * 100.0).round() as i64
}

#[inline]
pub fn cents_to_amount(cents: i64) -> f64 {
    round_currency((cents as f64) / 100.0)
}
