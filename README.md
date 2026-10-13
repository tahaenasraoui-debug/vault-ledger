# VaultLedger

**VaultLedger** is a 100% offline, air-gapped desktop app built with **Rust** and **Tauri v2**.
It ingests messy PDFs or receipt photos, extracts structured accounting records using an embedded local **Google Gemma 2B (GGUF)** model, verifies every record in a **SHA-256 chained SQLite database**, and exports balanced **Double-Entry Bookkeeping CSV** files.

## Quick Start

1. Install frontend packages:
   npm install

2. Launch in development mode:
   npm run tauri dev

3. Build production desktop binary:
   npm run tauri build
