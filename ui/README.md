# ui/ — web client

TypeScript + WASM web client (ADR-0002): WebGL2 waterfall, inventory, region-over-time and the
attack-map views, served by `hackriffd` through `hk-api`. Keep the TypeScript thin; put logic in
Rust/WASM. Build output goes to `ui/dist/` (gitignored). The approach is gated on spike S3; the
work starts in T-022.
