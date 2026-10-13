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

const GL_LABELS: Record<string, string> = {
  SoftwareAndSaaS: "6110 · SaaS",
  HardwareAndEquipment: "6120 · Hardware",
  OfficeSupplies: "6130 · Supplies",
  TravelAndTransport: "6210 · Travel",
  MealsAndEntertainment: "6220 · Meals",
  ProfessionalServices: "6310 · Prof Serv",
  Utilities: "6410 · Utilities",
  Other: "6990 · Misc",
};

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
  kpiGross.textContent = `${gross.toFixed(2)} ${ccy}`;
  kpiGrossSub.textContent = `Across ${records.length} verified document${records.length === 1 ? "" : "s"}`;
  kpiNet.textContent = `${net.toFixed(2)} ${ccy}`;
  kpiTax.textContent = `${tax.toFixed(2)} ${ccy}`;

  if (records.length > 0) {
    const avgConf = Math.round((confSum / records.length) * 100);
    kpiConfidence.textContent = `${avgConf}% AI CONF`;
  } else {
    kpiConfidence.textContent = "100% INTACT";
  }
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
    emptyTr.innerHTML = `<td colspan="11" class="empty-state">No matching ledger records found. Ingest a PDF or receipt image above to populate the vault.</td>`;
    ledgerTbody.appendChild(emptyTr);
    return;
  }

  for (const rec of filtered) {
    const doc = rec.document;
    const tr = document.createElement("tr");
    const shortHash = rec.chain_hash.slice(0, 10) + "…";
    const confPct = Math.round(doc.confidence_score * 100);
    const glLabel = GL_LABELS[doc.expense_category] ?? doc.expense_category;

    tr.innerHTML = `
      <td><strong>${escapeHtml(doc.transaction_date)}</strong></td>
      <td>
        <div class="vendor-cell">
          <span class="vendor-name">${escapeHtml(doc.vendor_name)}</span>
          <span class="vendor-source">${escapeHtml(rec.source_file_name)} · ${escapeHtml(rec.extraction_method)}</span>
        </div>
      </td>
      <td>${escapeHtml(doc.invoice_number ?? "—")}</td>
      <td><span class="gl-badge">${escapeHtml(glLabel)}</span></td>
      <td><span class="ccy-pill">${escapeHtml(doc.currency)}</span></td>
      <td class="num">${doc.net_amount.toFixed(2)}</td>
      <td class="num">${doc.tax_amount.toFixed(2)}</td>
      <td class="num"><strong>${doc.total_amount.toFixed(2)}</strong></td>
      <td>
        <div class="conf-wrap">
          <div class="conf-bar"><div class="conf-fill" style="width: ${confPct}%"></div></div>
          <span>${confPct}%</span>
        </div>
      </td>
      <td><span class="hash-badge" title="Chain SHA-256: ${rec.chain_hash}">${shortHash}</span></td>
      <td class="actions-col">
        <button class="row-btn" data-edit-id="${rec.record_id}">Edit</button>
        <button class="row-btn delete" data-delete-id="${rec.record_id}">Delete</button>
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
    infoHeadHash.textContent = status.audit_report.head_chain_hash.slice(0, 16) + "…";
    infoEventsCount.textContent = `${status.audit_report.audit_events_verified} signed events`;

    if (status.audit_report.integrity_valid) {
      badgeChain.className = "status-pill status-emerald";
      badgeChainText.textContent = `CHAIN VERIFIED (${status.audit_report.active_records_verified})`;
    }

    allRecords = await invoke<LedgerRecord[]>("list_ledger_records");
    updateKpiCards(allRecords);
    renderFilteredTable();
  } catch (err) {
    badgeChain.className = "status-pill status-rose";
    badgeChainText.textContent = "TAMPER ALERT";
    pushToast(formatError(err), true);
  }
}

async function handleIngestDocument(): Promise<void> {
  const filePath = inputFilePath.value.trim();
  if (!filePath) {
    pushToast("Enter a local PDF or image path first.", true);
    return;
  }

  btnIngest.disabled = true;
  streamingBox.classList.remove("hidden");
  streamOutput.textContent = "{";
  streamTokenCount.textContent = "0 tokens";
  streamStatusLabel.textContent = "RUNNING LOCAL GEMMA-2B GGUF INFERENCE...";

  try {
    const record = await invoke<LedgerRecord>("ingest_and_extract_document", {
      filePath,
    });
    pushToast(
      `Extracted "${record.document.vendor_name}" (${record.document.total_amount.toFixed(2)} ${record.document.currency}) and anchored to SQLite.`
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
    pushToast(`Record removed and SHA-256 chain re-anchored.`);
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
    pushToast(`Updated "${updatedDocument.vendor_name}" and re-verified SHA-256 chain.`);
    await refreshSystemState();
  } catch (err) {
    pushToast(formatError(err), true);
  }
}

async function handleExportCsv(): Promise<void> {
  const destinationPath = inputExportPath.value.trim();
  if (!destinationPath) {
    pushToast("Enter a destination file path for the CSV export.", true);
    return;
  }

  btnExport.disabled = true;
  try {
    const summary = await invoke<ExportSummary>("export_double_entry_csv", {
      destinationPath,
    });
    pushToast(
      `Exported ${summary.documents_exported} invoices (${summary.journal_rows_written} balanced journal legs) to ${summary.destination_path}.`
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
      `Audit Passed: ${report.active_records_verified} active records & ${report.audit_events_verified} historical events verified intact.`
    );
    await refreshSystemState();
  } catch (err) {
    badgeChain.className = "status-pill status-rose";
    badgeChainText.textContent = "TAMPER ALERT";
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
    streamStatusLabel.textContent = `STREAMING JSON · ${event.payload.file_name}`;
    streamTokenCount.textContent = `${event.payload.tokens_generated} tokens`;
    streamOutput.textContent += event.payload.latest_token;
    streamOutput.scrollTop = streamOutput.scrollHeight;
  });

  await refreshSystemState();
});
