import "./demo";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

interface ExtractedDocument {
  vendor_name: string;
  invoice_number: string | null;
  transaction_date: string;
  currency: string;
  net_amount: number;
  tax_amount: number;
  total_amount: number;
  tax_rate_percent: number | null;
  expense_category: string;
  confidence_score: number;
  raw_notes: string | null;
}

interface LedgerRecord {
  record_id: string;
  source_file_name: string;
  source_sha256: string;
  extraction_method: string;
  ingested_at_utc: string;
  document: ExtractedDocument;
  row_audit_hash: string;
  prev_chain_hash: string;
  chain_hash: string;
}

interface DatabaseAuditReport {
  verified_at_utc: string;
  active_records_verified: number;
  audit_events_verified: number;
  head_chain_hash: string;
  head_event_hash: string;
  integrity_valid: boolean;
}

interface AirGapStatusReport {
  airgap_enforced: boolean;
  unsafe_code_forbidden: boolean;
  model_loaded: boolean;
  model_sha256: string;
  database_path: string;
  audit_report: DatabaseAuditReport;
}

interface ExportSummary {
  destination_path: string;
  documents_exported: number;
  journal_rows_written: number;
  total_debits: number;
  total_credits: number;
}

interface InferenceProgressEvent {
  file_name: string;
  tokens_generated: number;
  latest_token: string;
}

interface IpcErrorEnvelope {
  code: string;
  recoverable: boolean;
  message: string;
  context: string | null;
}

const GL_LABELS: Record<string, { code: string; name: string }> = {
  SoftwareAndSaaS: { code: "6110", name: "SaaS" },
  HardwareAndEquipment: { code: "6120", name: "Hardware" },
  OfficeSupplies: { code: "6130", name: "Supplies" },
  TravelAndTransport: { code: "6210", name: "Travel" },
  MealsAndEntertainment: { code: "6220", name: "Meals" },
  ProfessionalServices: { code: "6310", name: "Professional" },
  Utilities: { code: "6410", name: "Utilities" },
  Other: { code: "6990", name: "Other" },
};

const ICON_EDIT =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z"/></svg>';
const ICON_DELETE =
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18"/><path d="M8 6V4h8v2"/><path d="M6 6l1 14h10l1-14"/></svg>';

const moneyFormat = new Intl.NumberFormat("en-US", {
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
});

function money(value: number): string {
  return moneyFormat.format(value);
}

function moneyWithCurrency(value: number, ccy: string): string {
  return `${money(value)}<span class="ccy">${escapeHtml(ccy)}</span>`;
}

let allRecords: LedgerRecord[] = [];

// DOM References
const inputFilePath = document.getElementById("input-file-path") as HTMLInputElement;
const btnIngest = document.getElementById("btn-ingest") as HTMLButtonElement;
const streamingBox = document.getElementById("streaming-box") as HTMLDivElement;
const streamStatusLabel = document.getElementById("stream-status-label") as HTMLSpanElement;
const streamTokenCount = document.getElementById("stream-token-count") as HTMLSpanElement;
const streamOutput = document.getElementById("stream-output") as HTMLPreElement;

const inputExportPath = document.getElementById("input-export-path") as HTMLInputElement;
const btnExport = document.getElementById("btn-export") as HTMLButtonElement;
const btnVerify = document.getElementById("btn-verify") as HTMLButtonElement;

const badgeChain = document.getElementById("badge-chain") as HTMLDivElement;
const badgeChainText = document.getElementById("badge-chain-text") as HTMLSpanElement;
const infoDbPath = document.getElementById("info-db-path") as HTMLElement;
const infoHeadHash = document.getElementById("info-head-hash") as HTMLElement;
const infoEventsCount = document.getElementById("info-events-count") as HTMLElement;

const kpiGross = document.getElementById("kpi-gross") as HTMLDivElement;
const kpiGrossSub = document.getElementById("kpi-gross-sub") as HTMLDivElement;
const kpiNet = document.getElementById("kpi-net") as HTMLDivElement;
const kpiTax = document.getElementById("kpi-tax") as HTMLDivElement;
const kpiConfidence = document.getElementById("kpi-confidence") as HTMLDivElement;

const inputSearch = document.getElementById("input-search") as HTMLInputElement;
const selectCategoryFilter = document.getElementById("select-category-filter") as HTMLSelectElement;
const recordCount = document.getElementById("record-count") as HTMLSpanElement;
const ledgerTbody = document.getElementById("ledger-tbody") as HTMLTableSectionElement;
const toastContainer = document.getElementById("toast-container") as HTMLDivElement;

// Edit Modal DOM References
const editModal = document.getElementById("edit-modal") as HTMLDivElement;
const formEditRecord = document.getElementById("form-edit-record") as HTMLFormElement;
const btnCloseModal = document.getElementById("btn-close-modal") as HTMLButtonElement;
const btnCancelEdit = document.getElementById("btn-cancel-edit") as HTMLButtonElement;
const editRecordId = document.getElementById("edit-record-id") as HTMLInputElement;
const editVendor = document.getElementById("edit-vendor") as HTMLInputElement;
const editInvoice = document.getElementById("edit-invoice") as HTMLInputElement;
const editDate = document.getElementById("edit-date") as HTMLInputElement;
const editCurrency = document.getElementById("edit-currency") as HTMLInputElement;
const editNet = document.getElementById("edit-net") as HTMLInputElement;
const editTax = document.getElementById("edit-tax") as HTMLInputElement;
const editTotal = document.getElementById("edit-total") as HTMLInputElement;
const editCategory = document.getElementById("edit-category") as HTMLSelectElement;

function pushToast(message: string, isError = false): void {
  const toast = document.createElement("div");
  toast.className = `toast ${isError ? "error" : ""}`;
  toast.textContent = message;
  toastContainer.appendChild(toast);

  setTimeout(() => {
    toast.remove();
  }, 5500);
}

function formatError(err: unknown): string {
  if (typeof err === "object" && err !== null && "message" in err) {
    const env = err as IpcErrorEnvelope;
    return `[${env.code}] ${env.message}`;
  }
  return String(err);
}

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function updateKpiCards(records: LedgerRecord[]): void {
  let gross = 0;
  let net = 0;
  let tax = 0;
  let confSum = 0;

  for (const r of records) {
    gross += r.document.total_amount;
    net += r.document.net_amount;
    tax += r.document.tax_amount;
    confSum += r.document.confidence_score;
  }

  const ccy = records.length > 0 ? records[records.length - 1].document.currency : "MAD";
  kpiGross.innerHTML = moneyWithCurrency(gross, ccy);
  kpiGrossSub.textContent = `Across ${records.length} verified document${records.length === 1 ? "" : "s"}`;
  kpiNet.innerHTML = moneyWithCurrency(net, ccy);
  kpiTax.innerHTML = moneyWithCurrency(tax, ccy);

  kpiConfidence.textContent =
    records.length > 0 ? `${Math.round((confSum / records.length) * 100)}%` : "—";
}

function renderFilteredTable(): void {
  const query = inputSearch.value.trim().toLowerCase();
  const catFilter = selectCategoryFilter.value;

  const filtered = allRecords.filter((rec) => {
    const doc = rec.document;
    const matchesCategory = catFilter === "ALL" || doc.expense_category === catFilter;
    const matchesSearch =
      query.length === 0 ||
      doc.vendor_name.toLowerCase().includes(query) ||
      (doc.invoice_number ?? "").toLowerCase().includes(query) ||
      doc.currency.toLowerCase().includes(query) ||
      doc.transaction_date.includes(query);
    return matchesCategory && matchesSearch;
  });

  recordCount.textContent = String(filtered.length);
  ledgerTbody.innerHTML = "";

  if (filtered.length === 0) {
    const emptyTr = document.createElement("tr");
    emptyTr.innerHTML = `<td colspan="8" class="empty-state">No records to show. Extract an invoice to add the first one.</td>`;
    ledgerTbody.appendChild(emptyTr);
    return;
  }

  for (const rec of filtered) {
    const doc = rec.document;
    const tr = document.createElement("tr");
    const shortHash = rec.chain_hash.slice(0, 8) + "…";
    const confPct = Math.round(doc.confidence_score * 100);
    const gl = GL_LABELS[doc.expense_category] ?? { code: "", name: doc.expense_category };
    const sourceLine = doc.invoice_number
      ? `Invoice ${doc.invoice_number} from ${rec.source_file_name}`
      : `From ${rec.source_file_name}`;

    tr.innerHTML = `
      <td>${escapeHtml(doc.transaction_date)}</td>
      <td>
        <span class="vendor-name">${escapeHtml(doc.vendor_name)}</span>
        <span class="vendor-source" title="${escapeHtml(rec.source_file_name)} (${escapeHtml(rec.extraction_method)})">${escapeHtml(sourceLine)}</span>
      </td>
      <td><span class="gl-code">${escapeHtml(gl.code)}</span> ${escapeHtml(gl.name)}</td>
      <td class="num">${money(doc.net_amount)}</td>
      <td class="num">${money(doc.tax_amount)}</td>
      <td class="num total">${moneyWithCurrency(doc.total_amount, doc.currency)}</td>
      <td>
        <div class="conf">
          <div class="conf-bar"><div class="conf-fill${confPct < 70 ? " low" : ""}" style="width: ${confPct}%"></div></div>
          <span>${confPct}%</span>
        </div>
        <span class="hash-badge" title="Chain SHA-256: ${rec.chain_hash}">${shortHash}</span>
      </td>
      <td class="actions-col">
        <button class="row-btn" data-edit-id="${rec.record_id}" aria-label="Edit ${escapeHtml(doc.vendor_name)}" title="Edit">${ICON_EDIT}</button>
        <button class="row-btn delete" data-delete-id="${rec.record_id}" aria-label="Delete ${escapeHtml(doc.vendor_name)}" title="Delete">${ICON_DELETE}</button>
      </td>
    `;

    ledgerTbody.appendChild(tr);
  }

  ledgerTbody.querySelectorAll<HTMLButtonElement>("[data-edit-id]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const id = btn.getAttribute("data-edit-id");
      const found = allRecords.find((r) => r.record_id === id);
      if (found) {
        openEditModal(found);
      }
    });
  });

  ledgerTbody.querySelectorAll<HTMLButtonElement>("[data-delete-id]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const id = btn.getAttribute("data-delete-id");
      if (id) {
        await handleDeleteRecord(id);
      }
    });
  });
}

function openEditModal(record: LedgerRecord): void {
  editRecordId.value = record.record_id;
  editVendor.value = record.document.vendor_name;
  editInvoice.value = record.document.invoice_number ?? "";
  editDate.value = record.document.transaction_date;
  editCurrency.value = record.document.currency;
  editNet.value = record.document.net_amount.toFixed(2);
  editTax.value = record.document.tax_amount.toFixed(2);
  editTotal.value = record.document.total_amount.toFixed(2);
  editCategory.value = record.document.expense_category;
  editModal.classList.remove("hidden");
}

function closeEditModal(): void {
  editModal.classList.add("hidden");
}

async function refreshSystemState(): Promise<void> {
  try {
    const status = await invoke<AirGapStatusReport>("verify_airgap_integrity");
    infoDbPath.textContent = status.database_path;
    infoDbPath.title = status.database_path;
    infoHeadHash.textContent = status.audit_report.head_chain_hash.slice(0, 16) + "…";
    infoEventsCount.textContent = `${status.audit_report.audit_events_verified} signed event${status.audit_report.audit_events_verified === 1 ? "" : "s"}`;

    if (status.audit_report.integrity_valid) {
      badgeChain.className = "status-pill status-emerald";
      badgeChainText.textContent = `Chain verified, ${status.audit_report.active_records_verified} record${status.audit_report.active_records_verified === 1 ? "" : "s"}`;
    }

    allRecords = await invoke<LedgerRecord[]>("list_ledger_records");
    updateKpiCards(allRecords);
    renderFilteredTable();
  } catch (err) {
    badgeChain.className = "status-pill status-rose";
    badgeChainText.textContent = "Tampering detected";
    pushToast(formatError(err), true);
  }
}

async function handleIngestDocument(): Promise<void> {
  const filePath = inputFilePath.value.trim();
  if (!filePath) {
    pushToast("Enter the path of a PDF or image first.", true);
    return;
  }

  btnIngest.disabled = true;
  streamingBox.classList.remove("hidden");
  streamOutput.textContent = "{";
  streamTokenCount.textContent = "0 tokens";
  streamStatusLabel.textContent = "Reading document…";

  try {
    const record = await invoke<LedgerRecord>("ingest_and_extract_document", {
      filePath,
    });
    pushToast(
      `Extracted "${record.document.vendor_name}" (${money(record.document.total_amount)} ${record.document.currency}) and saved it to the vault.`
    );
    inputFilePath.value = "";
    await refreshSystemState();
  } catch (err) {
    pushToast(formatError(err), true);
  } finally {
    btnIngest.disabled = false;
  }
}

async function handleDeleteRecord(recordId: string): Promise<void> {
  try {
    await invoke<DatabaseAuditReport>("delete_ledger_record", { recordId });
    pushToast("Record deleted. The SHA-256 chain was recalculated.");
    await refreshSystemState();
  } catch (err) {
    pushToast(formatError(err), true);
  }
}

async function handleSaveEdit(e: Event): Promise<void> {
  e.preventDefault();
  const recordId = editRecordId.value;
  const existing = allRecords.find((r) => r.record_id === recordId);
  if (!existing) return;

  const updatedDocument: ExtractedDocument = {
    vendor_name: editVendor.value.trim(),
    invoice_number: editInvoice.value.trim() || null,
    transaction_date: editDate.value.trim(),
    currency: editCurrency.value.trim().toUpperCase(),
    net_amount: parseFloat(editNet.value),
    tax_amount: parseFloat(editTax.value),
    total_amount: parseFloat(editTotal.value),
    tax_rate_percent: existing.document.tax_rate_percent,
    expense_category: editCategory.value,
    confidence_score: 1.0,
    raw_notes: existing.document.raw_notes,
  };

  try {
    await invoke<LedgerRecord>("update_ledger_record", {
      recordId,
      updatedDocument,
    });
    closeEditModal();
    pushToast(`Saved "${updatedDocument.vendor_name}". The SHA-256 chain was recalculated.`);
    await refreshSystemState();
  } catch (err) {
    pushToast(formatError(err), true);
  }
}

async function handleExportCsv(): Promise<void> {
  const destinationPath = inputExportPath.value.trim();
  if (!destinationPath) {
    pushToast("Enter a path to save the CSV to.", true);
    return;
  }

  btnExport.disabled = true;
  try {
    const summary = await invoke<ExportSummary>("export_double_entry_csv", {
      destinationPath,
    });
    pushToast(
      `Exported ${summary.documents_exported} invoices (${summary.journal_rows_written} journal lines) to ${summary.destination_path}.`
    );
  } catch (err) {
    pushToast(formatError(err), true);
  } finally {
    btnExport.disabled = false;
  }
}

async function handleVerifyIntegrity(): Promise<void> {
  try {
    const report = await invoke<DatabaseAuditReport>("verify_ledger_audit_chain");
    pushToast(
      `Audit passed: ${report.active_records_verified} records and ${report.audit_events_verified} audit events are intact.`
    );
    await refreshSystemState();
  } catch (err) {
    badgeChain.className = "status-pill status-rose";
    badgeChainText.textContent = "Tampering detected";
    pushToast(formatError(err), true);
  }
}

window.addEventListener("DOMContentLoaded", async () => {
  btnIngest.addEventListener("click", handleIngestDocument);
  btnExport.addEventListener("click", handleExportCsv);
  btnVerify.addEventListener("click", handleVerifyIntegrity);
  inputSearch.addEventListener("input", renderFilteredTable);
  selectCategoryFilter.addEventListener("change", renderFilteredTable);

  btnCloseModal.addEventListener("click", closeEditModal);
  btnCancelEdit.addEventListener("click", closeEditModal);
  formEditRecord.addEventListener("submit", handleSaveEdit);

  await listen<InferenceProgressEvent>("vault://inference-progress", (event) => {
    streamStatusLabel.textContent = `Extracting ${event.payload.file_name}`;
    streamTokenCount.textContent = `${event.payload.tokens_generated} tokens`;
    streamOutput.textContent += event.payload.latest_token;
    streamOutput.scrollTop = streamOutput.scrollHeight;
  });

  await refreshSystemState();
});
