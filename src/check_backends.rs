//! Dual-backend trace divergence detector for `harc sim --check-backends`.
//!
//! Both backends (`arch sim` and Verilator) write the same semantic JSONL
//! trace format defined in `runtime/harc_trace_rt.h`. The fields a real
//! semantic event carries (cycle, tlm_call payload, log severity/message,
//! sim_end errors, randomize payload) are deterministic across backends
//! when the same seed is used; backend-implementation noise lives in
//! `seq`, `vcd_time`, and `meta.dut_backend`. We strip those, compare the
//! remaining lines in order, and surface the first N mismatches.
//!
//! Companion to arch-com's SFG (signal flow graph) work — see
//! `docs/2026-05-28-backend-equivalence-gap.md`.

use std::fs;
use std::path::Path;

/// One pair of trace lines that disagree between the ARCH and Verilator backends.
#[derive(Debug, Clone)]
pub struct Divergence {
    /// 1-based line number in each (normalized) trace.
    pub line: usize,
    /// Cycle parsed from one of the events, if present. Used to make the
    /// report easy to read alongside a waveform.
    pub cycle: Option<u64>,
    /// Semantic event type (`tlm_call`, `log`, `sim_end`, ...), or
    /// `"<missing>"` when one backend ended early.
    pub event_type: String,
    /// Normalized ARCH-backend line, or "<missing>" when one trace is shorter.
    pub arch_line: String,
    /// Normalized Verilator-backend line, or "<missing>".
    pub sv_line: String,
}

impl Divergence {
    pub fn fmt(&self) -> String {
        let cycle = self
            .cycle
            .map(|c| format!("cycle {c}"))
            .unwrap_or_else(|| "cycle ?".to_string());
        format!(
            "line {} ({cycle}, type `{}`):\n      arch: {}\n      sv:   {}",
            self.line, self.event_type, self.arch_line, self.sv_line
        )
    }
}

/// Maximum number of divergences to collect before stopping. Keeps output
/// readable when an early mismatch cascades into many follow-on differences.
const MAX_DIVERGENCES: usize = 10;

/// Compare two semantic trace files and return the divergent line pairs.
///
/// Returns `Ok(vec![])` when the traces are byte-identical after
/// normalization. Returns a non-empty vec (up to `MAX_DIVERGENCES`)
/// otherwise. I/O errors propagate as `Err`.
pub fn diff_traces(arch_trace: &Path, sv_trace: &Path) -> Result<Vec<Divergence>, String> {
    let arch_text = fs::read_to_string(arch_trace)
        .map_err(|e| format!("reading {}: {}", arch_trace.display(), e))?;
    let sv_text = fs::read_to_string(sv_trace)
        .map_err(|e| format!("reading {}: {}", sv_trace.display(), e))?;
    diff_trace_strings(&arch_text, &sv_text)
}

/// Same as [`diff_traces`] but operates on in-memory trace text — used by
/// unit tests so they don't need to materialize files on disk.
///
/// REQUIRES: deterministic, stable cross-backend event order.
///
/// Both inputs are walked by line index after normalization (see
/// [`normalize_lines`]); a backend that emits two semantically
/// equivalent events on the same cycle in a different order than the
/// other backend WILL produce a false-positive divergence. Today this
/// holds — the only two backends are the ARCH native sim (single-
/// threaded C++ event loop) and Verilator (single-threaded VPI tick),
/// both of which serialize trace emission. If a future backend (e.g.
/// `arch sim --thread-sim parallel` once it grows native trace
/// support, or a multi-threaded SV simulator) breaks this assumption,
/// either:
///   1. Force the trace writer back into a deterministic order
///      (preferred — keeps the diff dumb and fast), or
///   2. Replace the per-line comparison with cycle-bucketed,
///      stable-sorted compare (see Limitations in
///      `docs/2026-05-28-backend-equivalence-gap.md`).
pub fn diff_trace_strings(arch_text: &str, sv_text: &str) -> Result<Vec<Divergence>, String> {
    let arch_lines = normalize_lines(arch_text);
    let sv_lines = normalize_lines(sv_text);
    let mut out = Vec::new();
    let max = arch_lines.len().max(sv_lines.len());
    for i in 0..max {
        let a = arch_lines.get(i);
        let s = sv_lines.get(i);
        match (a, s) {
            (Some(al), Some(sl)) if al.normalized == sl.normalized => continue,
            (Some(al), Some(sl)) => out.push(Divergence {
                line: i + 1,
                cycle: al.cycle.or(sl.cycle),
                event_type: pick_event_type(&al.event_type, &sl.event_type),
                arch_line: al.normalized.clone(),
                sv_line: sl.normalized.clone(),
            }),
            (Some(al), None) => out.push(Divergence {
                line: i + 1,
                cycle: al.cycle,
                event_type: al.event_type.clone(),
                arch_line: al.normalized.clone(),
                sv_line: "<missing>".to_string(),
            }),
            (None, Some(sl)) => out.push(Divergence {
                line: i + 1,
                cycle: sl.cycle,
                event_type: sl.event_type.clone(),
                arch_line: "<missing>".to_string(),
                sv_line: sl.normalized.clone(),
            }),
            (None, None) => break,
        }
        if out.len() >= MAX_DIVERGENCES {
            break;
        }
    }
    Ok(out)
}

fn pick_event_type(a: &str, s: &str) -> String {
    if a == s {
        a.to_string()
    } else if a.is_empty() {
        s.to_string()
    } else if s.is_empty() {
        a.to_string()
    } else {
        format!("{a} vs {s}")
    }
}

struct NormalizedLine {
    normalized: String,
    cycle: Option<u64>,
    event_type: String,
}

fn normalize_lines(text: &str) -> Vec<NormalizedLine> {
    text.lines()
        .filter_map(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return None;
            }
            Some(normalize_one(trimmed))
        })
        .collect()
}

/// Strip backend-implementation noise from a single trace line.
///
/// Removes `seq`, `vcd_time`, and (in the meta record) `dut_backend` and
/// `tool` — these legitimately differ between backends without indicating
/// a real divergence. Returns the cleaned line plus extracted `cycle` and
/// `type` for diagnostic reporting.
fn normalize_one(line: &str) -> NormalizedLine {
    let event_type = extract_string_field(line, "type").unwrap_or_default();
    let cycle = extract_u64_field(line, "cycle");
    let mut out = strip_field(line, "seq");
    out = strip_field(&out, "vcd_time");
    if event_type == "meta" {
        out = strip_field(&out, "dut_backend");
        out = strip_field(&out, "tool");
    }
    NormalizedLine { normalized: out, cycle, event_type }
}

/// Remove the JSON field `"<name>":<value>` from a flat object's text
/// representation. Handles trailing/leading commas so the result stays a
/// well-formed object. Operates on the assumption that the trace lines
/// emitted by `harc_trace_rt.h` are single-line, flat-keyed objects (the
/// `randomize` payload is the only nested case, and it lives in keys we
/// don't strip).
fn strip_field(line: &str, name: &str) -> String {
    let needle = format!("\"{name}\":");
    let Some(start) = line.find(&needle) else {
        return line.to_string();
    };
    let after_key = start + needle.len();
    let bytes = line.as_bytes();
    let end = match bytes.get(after_key) {
        Some(b'"') => {
            // String value: walk to the closing quote, respecting escapes.
            let mut i = after_key + 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i = (i + 2).min(bytes.len()),
                    b'"' => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
            i
        }
        Some(b'{') | Some(b'[') => {
            // Nested container — count brackets.
            let open = bytes[after_key];
            let close = if open == b'{' { b'}' } else { b']' };
            let mut depth = 1usize;
            let mut i = after_key + 1;
            let mut in_string = false;
            while i < bytes.len() && depth > 0 {
                let c = bytes[i];
                if in_string {
                    if c == b'\\' {
                        i = (i + 2).min(bytes.len());
                        continue;
                    }
                    if c == b'"' {
                        in_string = false;
                    }
                } else {
                    match c {
                        b'"' => in_string = true,
                        c if c == open => depth += 1,
                        c if c == close => depth -= 1,
                        _ => {}
                    }
                }
                i += 1;
            }
            i
        }
        _ => {
            // Number / true / false / null — read until `,` or `}`.
            let mut i = after_key;
            while i < bytes.len() && !matches!(bytes[i], b',' | b'}') {
                i += 1;
            }
            i
        }
    };

    // Splice out [start..end], dropping an adjacent comma so the surrounding
    // object stays valid. Prefer to consume the trailing comma; if none,
    // consume the leading one.
    let mut new_end = end;
    while new_end < bytes.len() && bytes[new_end] == b' ' {
        new_end += 1;
    }
    let mut new_start = start;
    if new_end < bytes.len() && bytes[new_end] == b',' {
        new_end += 1;
    } else if new_start > 0 && bytes[new_start - 1] == b',' {
        new_start -= 1;
    }

    let mut out = String::with_capacity(line.len());
    out.push_str(&line[..new_start]);
    out.push_str(&line[new_end..]);
    out
}

fn extract_string_field(line: &str, name: &str) -> Option<String> {
    let needle = format!("\"{name}\":\"");
    let start = line.find(&needle)? + needle.len();
    let bytes = line.as_bytes();
    let mut i = start;
    let mut out = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => {
                out.push(bytes[i + 1] as char);
                i += 2;
            }
            b'"' => return Some(out),
            c => {
                out.push(c as char);
                i += 1;
            }
        }
    }
    None
}

fn extract_u64_field(line: &str, name: &str) -> Option<u64> {
    let needle = format!("\"{name}\":");
    let start = line.find(&needle)? + needle.len();
    let bytes = line.as_bytes();
    if bytes.get(start) == Some(&b'"') {
        return None;
    }
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    line[start..i].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_traces_have_no_divergence() {
        let a = r#"{"type":"meta","schema_version":1,"tool":"harc","seed":1,"dut_backend":"arch","top":"Top","test":"t"}
{"type":"sim_start","cycle":0,"seq":0,"vcd_time":0,"clock":"clk","clock_cycle":0}
{"type":"log","cycle":3,"seq":1,"vcd_time":30,"clock":"clk","clock_cycle":3,"severity":"INFO","message":"hi"}
{"type":"sim_end","cycle":10,"seq":2,"vcd_time":100,"clock":"clk","clock_cycle":10,"errors":0}
"#;
        let s = r#"{"type":"meta","schema_version":1,"tool":"harc","seed":1,"dut_backend":"verilator","top":"Top","test":"t"}
{"type":"sim_start","cycle":0,"seq":0,"vcd_time":0,"clock":"clk","clock_cycle":0}
{"type":"log","cycle":3,"seq":1,"vcd_time":33,"clock":"clk","clock_cycle":3,"severity":"INFO","message":"hi"}
{"type":"sim_end","cycle":10,"seq":2,"vcd_time":99,"clock":"clk","clock_cycle":10,"errors":0}
"#;
        let divs = diff_trace_strings(a, s).unwrap();
        assert!(divs.is_empty(), "expected no divergence, got: {divs:?}");
    }

    #[test]
    fn different_log_message_diverges() {
        let a = r#"{"type":"sim_start","cycle":0,"seq":0,"vcd_time":0,"clock":"clk","clock_cycle":0}
{"type":"log","cycle":3,"seq":1,"vcd_time":30,"clock":"clk","clock_cycle":3,"severity":"INFO","message":"hi"}
"#;
        let s = r#"{"type":"sim_start","cycle":0,"seq":0,"vcd_time":0,"clock":"clk","clock_cycle":0}
{"type":"log","cycle":3,"seq":1,"vcd_time":33,"clock":"clk","clock_cycle":3,"severity":"INFO","message":"bye"}
"#;
        let divs = diff_trace_strings(a, s).unwrap();
        assert_eq!(divs.len(), 1);
        assert_eq!(divs[0].event_type, "log");
        assert_eq!(divs[0].cycle, Some(3));
        assert!(divs[0].arch_line.contains("\"message\":\"hi\""));
        assert!(divs[0].sv_line.contains("\"message\":\"bye\""));
    }

    #[test]
    fn different_sim_end_errors_diverges() {
        let a = r#"{"type":"sim_end","cycle":10,"seq":0,"vcd_time":100,"clock":"clk","clock_cycle":10,"errors":0}
"#;
        let s = r#"{"type":"sim_end","cycle":10,"seq":0,"vcd_time":100,"clock":"clk","clock_cycle":10,"errors":1}
"#;
        let divs = diff_trace_strings(a, s).unwrap();
        assert_eq!(divs.len(), 1);
        assert_eq!(divs[0].event_type, "sim_end");
    }

    #[test]
    fn short_arch_trace_reports_missing_tail() {
        let a = r#"{"type":"sim_start","cycle":0,"seq":0,"vcd_time":0,"clock":"clk","clock_cycle":0}
"#;
        let s = r#"{"type":"sim_start","cycle":0,"seq":0,"vcd_time":0,"clock":"clk","clock_cycle":0}
{"type":"sim_end","cycle":5,"seq":1,"vcd_time":50,"clock":"clk","clock_cycle":5,"errors":0}
"#;
        let divs = diff_trace_strings(a, s).unwrap();
        assert_eq!(divs.len(), 1);
        assert_eq!(divs[0].arch_line, "<missing>");
        assert_eq!(divs[0].event_type, "sim_end");
    }

    #[test]
    fn divergent_tlm_call_phase_detected() {
        let a = r#"{"type":"tlm_call","cycle":7,"seq":0,"vcd_time":70,"clock":"clk","clock_cycle":7,"component":"i","bus":"m","method":"read","phase":"req","direction":"out","tag":0}
"#;
        let s = r#"{"type":"tlm_call","cycle":7,"seq":0,"vcd_time":70,"clock":"clk","clock_cycle":7,"component":"i","bus":"m","method":"read","phase":"rsp","direction":"in","tag":0}
"#;
        let divs = diff_trace_strings(a, s).unwrap();
        assert_eq!(divs.len(), 1);
        assert_eq!(divs[0].event_type, "tlm_call");
    }

    #[test]
    fn strip_field_handles_string_number_and_trailing_comma() {
        let line = r#"{"a":"x","b":1,"c":"y"}"#;
        assert_eq!(strip_field(line, "a"), r#"{"b":1,"c":"y"}"#);
        assert_eq!(strip_field(line, "b"), r#"{"a":"x","c":"y"}"#);
        assert_eq!(strip_field(line, "c"), r#"{"a":"x","b":1}"#);
    }

    #[test]
    fn meta_dut_backend_stripped() {
        let a = r#"{"type":"meta","schema_version":1,"tool":"harc","seed":1,"dut_backend":"arch","top":"T","test":"t"}"#;
        let s = r#"{"type":"meta","schema_version":1,"tool":"harc","seed":1,"dut_backend":"verilator","top":"T","test":"t"}"#;
        let divs = diff_trace_strings(a, s).unwrap();
        assert!(divs.is_empty(), "meta dut_backend mismatch must be ignored, got: {divs:?}");
    }

    #[test]
    fn max_divergences_cap_respected() {
        let mut a = String::new();
        let mut s = String::new();
        for i in 0..(MAX_DIVERGENCES + 5) {
            a.push_str(&format!(
                r#"{{"type":"log","cycle":{i},"seq":{i},"vcd_time":{i},"clock":"c","clock_cycle":{i},"severity":"INFO","message":"a"}}
"#));
            s.push_str(&format!(
                r#"{{"type":"log","cycle":{i},"seq":{i},"vcd_time":{i},"clock":"c","clock_cycle":{i},"severity":"INFO","message":"b"}}
"#));
        }
        let divs = diff_trace_strings(&a, &s).unwrap();
        assert_eq!(divs.len(), MAX_DIVERGENCES);
    }
}
