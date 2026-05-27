# Semantic Trace

**Status:** Usable. Semantic JSONL recording, TLM call events, VCD alignment
metadata, and post-run VCD merging are implemented.

HARC semantic trace is the transaction/event-level companion to signal
waveforms. `harc sim --record-trace <file.jsonl>` records runtime events as
JSONL, and `harc trace-merge --vcd wave.vcd --trace trace.jsonl --out
merged.vcd` overlays those events into a synthetic `harc_semantic` VCD scope.

## Implemented

- JSONL trace writer with metadata, `sim_start`, `sim_end`, `log`,
  `assertion_failure`, `randomize`, and `tlm_call` events.
- TLM method call request/response tracing for blocking initiator calls,
  RHS `fork` / `join_all`, and target responder paths.
- VCD-alignment metadata on runtime events: `vcd_time`, `clock`, and
  `clock_cycle`.
- `harc trace-merge` post-processor:
  - input: signal VCD plus semantic JSONL trace
  - output: merged VCD with synthetic `harc_semantic` event lanes
  - string-ID mappings embedded as VCD comments
  - optional `--map-out <file.json>` sidecar for the same mappings

## TODOs

1. Capture TLM argument and return values in `tlm_call` events. This needs a
   typed serializer that handles scalar, wide, enum, list/vector, and record
   payloads consistently across initiator, fork/join, and target-responder
   paths.

2. Add a direct convenience flow for users who already requested both
   `--waves` and `--record-trace`, for example an opt-in `--merge-trace`
   flag that writes `<wave>.semantic.vcd` automatically after simulation.

3. Improve viewer ergonomics with a generated GTKWave/Surfer save file that
   groups `harc_semantic` lanes, shows valid pulses first, and keeps ID fields
   near their string-map comments.

4. Document the FST workflow explicitly. `trace-merge` operates on VCD, so FST
   users should either request `--wave-format vcd` for semantic overlays or
   convert FST to VCD before merging. A future helper may automate that
   conversion when the required external tool is available.

5. Consider richer event families once the current trace surface has enough
   real fixture use:
   - coverage sample / bin-hit events
   - covergroup hook entry events
   - scheduler wait/wake events
   - scoreboard push/pop/check events
   - external reference-model call events

## Notes

- VCD has no portable transaction-record type, so semantic events are encoded
  as numeric waveform lanes plus string-ID maps.
- Multiple semantic events at the same `vcd_time` are represented as parallel
  event lanes so events are not overwritten at a single timestamp.
- The JSONL trace remains the source of truth. The merged VCD is a debug view
  for waveform inspection.
