// Browser demo: when the page runs outside Tauri (for example on GitHub Pages),
// answer the app's IPC calls with sample data so the interface is usable.
// Inside the desktop app window.__TAURI_INTERNALS__ already exists and this file does nothing.

interface Doc {
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

interface Rec {
  record_id: string;
  source_file_name: string;
  source_sha256: string;
  extraction_method: string;
  ingested_at_utc: string;
  document: Doc;
  row_audit_hash: string;
  prev_chain_hash: string;
  chain_hash: string;
}

const isTauri = "__TAURI_INTERNALS__" in window;

if (!isTauri) {
  let nextId = 1;
  const hex = () =>
    Array.from({ length: 64 }, () => Math.floor(Math.random() * 16).toString(16)).join("");

  const make = (
    vendor: string,
    invoice: string | null,
    date: string,
    category: string,
    net: number,
    tax: number,
    confidence: number,
    file: string,
    method = "pdf_text",
  ): Rec => ({
    record_id: `demo-${nextId++}`,
    source_file_name: file,
    source_sha256: hex(),
    extraction_method: method,
    ingested_at_utc: new Date().toISOString(),
    document: {
      vendor_name: vendor,
      invoice_number: invoice,
      transaction_date: date,
      currency: "MAD",
      net_amount: net,
      tax_amount: tax,
      total_amount: Math.round((net + tax) * 100) / 100,
      tax_rate_percent: 20,
      expense_category: category,
      confidence_score: confidence,
      raw_notes: null,
    },
    row_audit_hash: hex(),
    prev_chain_hash: hex(),
    chain_hash: hex(),
  });

  let records: Rec[] = [
    make("Maroc Telecom", "FT-2026-0412", "2026-10-03", "Utilities", 250, 50, 0.97, "maroc_telecom_oct.pdf"),
    make("Hetzner Online GmbH", "R0048213", "2026-10-07", "SoftwareAndSaaS", 412.5, 82.5, 0.93, "hetzner_invoice.pdf"),
    make("Librairie Al Madariss", null, "2026-10-11", "OfficeSupplies", 186, 37.2, 0.61, "receipt_photo.jpg", "ocr"),
    make("ONCF", "TK-88231", "2026-10-14", "TravelAndTransport", 140, 14, 0.88, "oncf_ticket.pdf"),
    make("JetBrains s.r.o.", "JB-5539100", "2026-10-19", "SoftwareAndSaaS", 1890, 378, 0.99, "jetbrains_renewal.pdf"),
  ];
  let auditEvents = records.length;

  const callbacks = new Map<number, (e: unknown) => void>();
  const listeners = new Map<string, number[]>();
  let callbackId = 1;

  const emit = (event: string, payload: unknown) => {
    for (const id of listeners.get(event) ?? []) {
      callbacks.get(id)?.({ event, id, payload });
    }
  };

  const audit = () => ({
    verified_at_utc: new Date().toISOString(),
    active_records_verified: records.length,
    audit_events_verified: auditEvents,
    head_chain_hash: records.length ? records[records.length - 1].chain_hash : "0".repeat(64),
    head_event_hash: hex(),
    integrity_valid: true,
  });

  const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

  const ingest = async (filePath: string): Promise<Rec> => {
    const file = filePath.split(/[\\/]/).pop() || "document.pdf";
    const vendor = file
      .replace(/\.[^.]+$/, "")
      .replace(/[_-]+/g, " ")
      .replace(/\b\w/g, (c) => c.toUpperCase());
    const rec = make(vendor, `DEMO-${1000 + nextId}`, "2026-10-28", "ProfessionalServices", 320, 64, 0.9, file);
    const json = JSON.stringify(
      {
        vendor_name: rec.document.vendor_name,
        invoice_number: rec.document.invoice_number,
        transaction_date: rec.document.transaction_date,
        currency: "MAD",
        net_amount: rec.document.net_amount,
        tax_amount: rec.document.tax_amount,
        total_amount: rec.document.total_amount,
      },
      null,
      2,
    );
    const pieces = json.match(/.{1,6}/gs) ?? [];
    let count = 0;
    for (const piece of pieces) {
      count += 1;
      emit("vault://inference-progress", { file_name: file, tokens_generated: count, latest_token: piece });
      await sleep(35);
    }
    records = [...records, rec];
    auditEvents += 1;
    return rec;
  };

  const handlers: Record<string, (args: any) => unknown> = {
    verify_airgap_integrity: () => ({
      airgap_enforced: true,
      unsafe_code_forbidden: true,
      model_loaded: true,
      model_sha256: hex(),
      database_path: "~/.local/share/local.vaultledger.airgap/vault.sqlite3",
      audit_report: audit(),
    }),
    verify_ledger_audit_chain: () => audit(),
    list_ledger_records: () => records,
    ingest_and_extract_document: (a) => ingest(String(a.filePath ?? "")),
    delete_ledger_record: (a) => {
      records = records.filter((r) => r.record_id !== a.recordId);
      auditEvents += 1;
      return audit();
    },
    update_ledger_record: (a) => {
      const rec = records.find((r) => r.record_id === a.recordId);
      if (!rec) throw { code: "NOT_FOUND", recoverable: true, message: "Record not found", context: null };
      rec.document = a.updatedDocument;
      rec.chain_hash = hex();
      auditEvents += 1;
      return rec;
    },
    export_double_entry_csv: (a) => ({
      destination_path: `${a.destinationPath} (demo: no file written)`,
      documents_exported: records.length,
      journal_rows_written: records.length * 3,
      total_debits: 0,
      total_credits: 0,
    }),
  };

  (window as any).__TAURI_INTERNALS__ = {
    transformCallback: (cb: (e: unknown) => void) => {
      const id = callbackId++;
      callbacks.set(id, cb);
      return id;
    },
    invoke: async (cmd: string, args: any) => {
      if (cmd === "plugin:event|listen") {
        const list = listeners.get(args.event) ?? [];
        list.push(args.handler);
        listeners.set(args.event, list);
        return args.handler;
      }
      if (cmd === "plugin:event|unlisten") return null;
      const fn = handlers[cmd];
      if (!fn) throw { code: "DEMO", recoverable: true, message: `${cmd} is not available in the browser demo`, context: null };
      return fn(args ?? {});
    },
  };

  window.addEventListener("DOMContentLoaded", () => {
    const pill = document.querySelector<HTMLElement>(".status-pill");
    if (pill) {
      pill.textContent = "Browser demo with sample data";
      pill.title = "Download the desktop app to process your own documents";
    }
  });
}
