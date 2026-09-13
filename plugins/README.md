# plugins/ — decoder plugins

One directory per decoder plugin (readsb, rtl_433, multimon-ng, ...) holding its manifest and a
thin wrapper. Plugins run as supervised subprocesses under `hk-plugins` (ADR-0003). The process
boundary keeps GPL decoders out of the core licence (ADR-0010). Add every wrapped tool to the
ADR-0010 ledger with its licence. The first plugin is readsb (T-015), built against the T-014
plugin contract.
