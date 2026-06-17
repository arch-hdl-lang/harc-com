use miette::{IntoDiagnostic, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
struct SemanticEvent {
    typ: String,
    seq: u64,
    cycle: u64,
    vcd_time: u64,
    clock: String,
    clock_cycle: u64,
    component: String,
    bus: String,
    method: String,
    phase: String,
    direction: String,
    tag: Option<u64>,
    label: String,
}

#[derive(Debug, Clone)]
struct SignalIds {
    valid: String,
    event_type: String,
    seq: String,
    cycle: String,
    clock_id: String,
    clock_cycle: String,
    component_id: String,
    bus_id: String,
    method_id: String,
    phase_id: String,
    direction_id: String,
    tag_valid: String,
    tag: String,
    label_id: String,
}

#[derive(Default)]
struct StringTables {
    event_type: IdTable,
    clock: IdTable,
    component: IdTable,
    bus: IdTable,
    method: IdTable,
    phase: IdTable,
    direction: IdTable,
    label: IdTable,
}

#[derive(Default)]
struct IdTable {
    ids: BTreeMap<String, u32>,
}

impl IdTable {
    fn id(&mut self, value: &str) -> u32 {
        if value.is_empty() {
            return 0;
        }
        if let Some(id) = self.ids.get(value) {
            return *id;
        }
        let id = (self.ids.len() + 1) as u32;
        self.ids.insert(value.to_string(), id);
        id
    }
}

pub(crate) fn cmd_trace_merge(
    vcd: &Path,
    trace: &Path,
    out: &Path,
    map_out: Option<&Path>,
) -> Result<()> {
    let vcd_text = fs::read_to_string(vcd).into_diagnostic()?;
    let trace_text = fs::read_to_string(trace).into_diagnostic()?;
    let events = parse_trace_events(&trace_text)?;
    let (merged, maps) = merge_trace_into_vcd(&vcd_text, &events)?;
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).into_diagnostic()?;
        }
    }
    fs::write(out, merged).into_diagnostic()?;
    if let Some(map_path) = map_out {
        if let Some(parent) = map_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).into_diagnostic()?;
            }
        }
        fs::write(map_path, render_map_json(&maps)).into_diagnostic()?;
    }
    eprintln!(
        "merged {} semantic event(s) into {}",
        events.len(),
        out.display()
    );
    Ok(())
}

fn parse_trace_events(trace_text: &str) -> Result<Vec<SemanticEvent>> {
    let mut out = Vec::new();
    for (idx, line) in trace_text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let fields = parse_json_object_top(trimmed).map_err(|e| {
            miette::miette!(
                "semantic trace line {} is not a supported JSON object: {}",
                idx + 1,
                e
            )
        })?;
        let typ = string_field(&fields, "type").unwrap_or_default();
        if typ.is_empty() || typ == "meta" {
            continue;
        }
        let Some(vcd_time) = u64_field(&fields, "vcd_time") else {
            continue;
        };
        let cycle = u64_field(&fields, "cycle").unwrap_or(0);
        let seq = u64_field(&fields, "seq").unwrap_or(out.len() as u64);
        let clock = string_field(&fields, "clock").unwrap_or_default();
        let clock_cycle = u64_field(&fields, "clock_cycle").unwrap_or(cycle);
        let component = string_field(&fields, "component").unwrap_or_default();
        let bus = string_field(&fields, "bus").unwrap_or_default();
        let method = string_field(&fields, "method").unwrap_or_default();
        let phase = string_field(&fields, "phase").unwrap_or_default();
        let direction = string_field(&fields, "direction").unwrap_or_default();
        let tag = u64_field(&fields, "tag");
        let label = event_label(
            &typ,
            &component,
            &bus,
            &method,
            &phase,
            &direction,
            tag,
            string_field(&fields, "severity").as_deref(),
            string_field(&fields, "message").as_deref(),
        );
        out.push(SemanticEvent {
            typ,
            seq,
            cycle,
            vcd_time,
            clock,
            clock_cycle,
            component,
            bus,
            method,
            phase,
            direction,
            tag,
            label,
        });
    }
    out.sort_by_key(|e| (e.vcd_time, e.seq));
    Ok(out)
}

fn merge_trace_into_vcd(
    vcd_text: &str,
    events: &[SemanticEvent],
) -> Result<(String, StringTables)> {
    let lines: Vec<&str> = vcd_text.lines().collect();
    let Some(enddefs_idx) = lines
        .iter()
        .position(|line| line.trim() == "$enddefinitions $end")
    else {
        return Err(miette::miette!(
            "input VCD has no `$enddefinitions $end` marker"
        ));
    };
    let existing_ids = collect_vcd_ids(&lines[..=enddefs_idx]);
    let mut id_gen = VcdIdGen::new(existing_ids);
    let mut by_time: BTreeMap<u64, Vec<SemanticEvent>> = BTreeMap::new();
    for event in events {
        by_time
            .entry(event.vcd_time)
            .or_default()
            .push(event.clone());
    }
    let lane_count = by_time.values().map(Vec::len).max().unwrap_or(1).max(1);
    let signal_ids: Vec<_> = (0..lane_count)
        .map(|_| SignalIds {
            valid: id_gen.next(),
            event_type: id_gen.next(),
            seq: id_gen.next(),
            cycle: id_gen.next(),
            clock_id: id_gen.next(),
            clock_cycle: id_gen.next(),
            component_id: id_gen.next(),
            bus_id: id_gen.next(),
            method_id: id_gen.next(),
            phase_id: id_gen.next(),
            direction_id: id_gen.next(),
            tag_valid: id_gen.next(),
            tag: id_gen.next(),
            label_id: id_gen.next(),
        })
        .collect();
    let mut maps = StringTables::default();
    for event in events {
        intern_event(&mut maps, event);
    }

    let mut out = String::new();
    for line in &lines[..enddefs_idx] {
        out.push_str(line);
        out.push('\n');
    }
    emit_semantic_header(&mut out, &signal_ids, &maps);
    out.push_str(lines[enddefs_idx]);
    out.push('\n');

    let mut active = false;
    let pending_times: Vec<u64> = by_time.keys().copied().collect();
    let mut pending_idx = 0;
    for line in &lines[enddefs_idx + 1..] {
        if let Some(time) = parse_vcd_time_line(line.trim()) {
            while pending_idx < pending_times.len() && pending_times[pending_idx] < time {
                let event_time = pending_times[pending_idx];
                out.push_str(&format!("#{event_time}\n"));
                emit_events_at_time(&mut out, &signal_ids, &by_time[&event_time], &mut maps);
                active = true;
                pending_idx += 1;
            }
            out.push_str(line);
            out.push('\n');
            if pending_idx < pending_times.len() && pending_times[pending_idx] == time {
                emit_events_at_time(&mut out, &signal_ids, &by_time[&time], &mut maps);
                active = true;
                pending_idx += 1;
            } else if active {
                emit_valid_zero(&mut out, &signal_ids);
                active = false;
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    let mut last_time = lines
        .iter()
        .filter_map(|line| parse_vcd_time_line(line.trim()))
        .max()
        .unwrap_or(0);
    while pending_idx < pending_times.len() {
        let event_time = pending_times[pending_idx];
        if event_time < last_time {
            return Err(miette::miette!(
                "semantic trace event at time {} would make VCD time go backwards",
                event_time
            ));
        }
        out.push_str(&format!("#{event_time}\n"));
        emit_events_at_time(&mut out, &signal_ids, &by_time[&event_time], &mut maps);
        active = true;
        last_time = event_time;
        pending_idx += 1;
    }
    if active {
        out.push_str(&format!("#{}\n", last_time.saturating_add(1)));
        emit_valid_zero(&mut out, &signal_ids);
    }
    Ok((out, maps))
}

fn emit_semantic_header(out: &mut String, signal_ids: &[SignalIds], maps: &StringTables) {
    out.push_str("$scope module harc_semantic $end\n");
    out.push_str("$comment HARC semantic trace lanes; string ID mappings follow as HARC_TRACE_MAP comments. $end\n");
    emit_map_comments(out, "event_type", &maps.event_type);
    emit_map_comments(out, "clock", &maps.clock);
    emit_map_comments(out, "component", &maps.component);
    emit_map_comments(out, "bus", &maps.bus);
    emit_map_comments(out, "method", &maps.method);
    emit_map_comments(out, "phase", &maps.phase);
    emit_map_comments(out, "direction", &maps.direction);
    emit_map_comments(out, "label", &maps.label);
    for (lane, ids) in signal_ids.iter().enumerate() {
        out.push_str(&format!(
            "$var wire 1 {} event{lane}_valid $end\n",
            ids.valid
        ));
        out.push_str(&format!(
            "$var integer 32 {} event{lane}_type $end\n",
            ids.event_type
        ));
        out.push_str(&format!(
            "$var integer 64 {} event{lane}_seq $end\n",
            ids.seq
        ));
        out.push_str(&format!(
            "$var integer 32 {} event{lane}_cycle $end\n",
            ids.cycle
        ));
        out.push_str(&format!(
            "$var integer 32 {} event{lane}_clock_id $end\n",
            ids.clock_id
        ));
        out.push_str(&format!(
            "$var integer 64 {} event{lane}_clock_cycle $end\n",
            ids.clock_cycle
        ));
        out.push_str(&format!(
            "$var integer 32 {} event{lane}_component_id $end\n",
            ids.component_id
        ));
        out.push_str(&format!(
            "$var integer 32 {} event{lane}_bus_id $end\n",
            ids.bus_id
        ));
        out.push_str(&format!(
            "$var integer 32 {} event{lane}_method_id $end\n",
            ids.method_id
        ));
        out.push_str(&format!(
            "$var integer 32 {} event{lane}_phase_id $end\n",
            ids.phase_id
        ));
        out.push_str(&format!(
            "$var integer 32 {} event{lane}_direction_id $end\n",
            ids.direction_id
        ));
        out.push_str(&format!(
            "$var wire 1 {} event{lane}_tag_valid $end\n",
            ids.tag_valid
        ));
        out.push_str(&format!(
            "$var integer 64 {} event{lane}_tag $end\n",
            ids.tag
        ));
        out.push_str(&format!(
            "$var integer 32 {} event{lane}_label_id $end\n",
            ids.label_id
        ));
    }
    out.push_str("$upscope $end\n");
}

fn emit_events_at_time(
    out: &mut String,
    signal_ids: &[SignalIds],
    events: &[SemanticEvent],
    maps: &mut StringTables,
) {
    emit_valid_zero(out, signal_ids);
    for (lane, event) in events.iter().enumerate() {
        let ids = &signal_ids[lane];
        let tag_valid = event.tag.is_some();
        emit_int(
            out,
            &ids.event_type,
            maps.event_type.id(&event.typ) as u64,
            32,
        );
        emit_int(out, &ids.seq, event.seq, 64);
        emit_int(out, &ids.cycle, event.cycle, 32);
        emit_int(out, &ids.clock_id, maps.clock.id(&event.clock) as u64, 32);
        emit_int(out, &ids.clock_cycle, event.clock_cycle, 64);
        emit_int(
            out,
            &ids.component_id,
            maps.component.id(&event.component) as u64,
            32,
        );
        emit_int(out, &ids.bus_id, maps.bus.id(&event.bus) as u64, 32);
        emit_int(
            out,
            &ids.method_id,
            maps.method.id(&event.method) as u64,
            32,
        );
        emit_int(out, &ids.phase_id, maps.phase.id(&event.phase) as u64, 32);
        emit_int(
            out,
            &ids.direction_id,
            maps.direction.id(&event.direction) as u64,
            32,
        );
        emit_int(out, &ids.tag, event.tag.unwrap_or(0), 64);
        emit_int(out, &ids.label_id, maps.label.id(&event.label) as u64, 32);
        out.push_str(if tag_valid { "1" } else { "0" });
        out.push_str(&ids.tag_valid);
        out.push('\n');
        out.push_str("1");
        out.push_str(&ids.valid);
        out.push('\n');
    }
}

fn emit_valid_zero(out: &mut String, signal_ids: &[SignalIds]) {
    for ids in signal_ids {
        out.push('0');
        out.push_str(&ids.valid);
        out.push('\n');
        out.push('0');
        out.push_str(&ids.tag_valid);
        out.push('\n');
    }
}

fn emit_int(out: &mut String, id: &str, value: u64, width: usize) {
    out.push('b');
    for bit in (0..width).rev() {
        out.push(if ((value >> bit) & 1) != 0 { '1' } else { '0' });
    }
    out.push(' ');
    out.push_str(id);
    out.push('\n');
}

fn intern_event(maps: &mut StringTables, event: &SemanticEvent) {
    maps.event_type.id(&event.typ);
    maps.clock.id(&event.clock);
    maps.component.id(&event.component);
    maps.bus.id(&event.bus);
    maps.method.id(&event.method);
    maps.phase.id(&event.phase);
    maps.direction.id(&event.direction);
    maps.label.id(&event.label);
}

fn emit_map_comments(out: &mut String, name: &str, table: &IdTable) {
    for (value, id) in &table.ids {
        out.push_str(&format!(
            "$comment HARC_TRACE_MAP {name} {id} {} $end\n",
            vcd_comment_escape(value)
        ));
    }
}

fn render_map_json(maps: &StringTables) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    render_json_table(&mut out, "event_type", &maps.event_type, true);
    render_json_table(&mut out, "clock", &maps.clock, false);
    render_json_table(&mut out, "component", &maps.component, false);
    render_json_table(&mut out, "bus", &maps.bus, false);
    render_json_table(&mut out, "method", &maps.method, false);
    render_json_table(&mut out, "phase", &maps.phase, false);
    render_json_table(&mut out, "direction", &maps.direction, false);
    render_json_table(&mut out, "label", &maps.label, false);
    out.push_str("\n}\n");
    out
}

fn render_json_table(out: &mut String, name: &str, table: &IdTable, first: bool) {
    if !first {
        out.push_str(",\n");
    }
    out.push_str(&format!("  \"{}\": {{", json_escape(name)));
    let mut first_entry = true;
    for (value, id) in &table.ids {
        if !first_entry {
            out.push(',');
        }
        first_entry = false;
        out.push_str(&format!("\n    \"{}\": \"{}\"", id, json_escape(value)));
    }
    if !table.ids.is_empty() {
        out.push('\n');
        out.push_str("  ");
    }
    out.push('}');
}

fn collect_vcd_ids(lines: &[&str]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for line in lines {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() >= 5 && parts[0] == "$var" {
            ids.insert(parts[3].to_string());
        }
    }
    ids
}

struct VcdIdGen {
    used: HashSet<String>,
    next: usize,
}

impl VcdIdGen {
    fn new(used: HashSet<String>) -> Self {
        Self { used, next: 0 }
    }

    fn next(&mut self) -> String {
        loop {
            let id = format!("HARC{}", self.next);
            self.next += 1;
            if self.used.insert(id.clone()) {
                return id;
            }
        }
    }
}

fn parse_vcd_time_line(line: &str) -> Option<u64> {
    let rest = line.strip_prefix('#')?;
    rest.parse().ok()
}

fn event_label(
    typ: &str,
    component: &str,
    bus: &str,
    method: &str,
    phase: &str,
    direction: &str,
    tag: Option<u64>,
    severity: Option<&str>,
    message: Option<&str>,
) -> String {
    if typ == "tlm_call" {
        let mut label = format!("{direction}:{phase}");
        if !component.is_empty() {
            label.push(' ');
            label.push_str(component);
        }
        if !bus.is_empty() || !method.is_empty() {
            label.push(' ');
            if !bus.is_empty() {
                label.push_str(bus);
                label.push('.');
            }
            label.push_str(method);
        }
        if let Some(tag) = tag {
            label.push_str(&format!(" tag={tag}"));
        }
        return label;
    }
    if typ == "log" || typ == "assertion_failure" {
        let sev = severity.unwrap_or("");
        let msg = message.unwrap_or("");
        return if sev.is_empty() {
            format!("{typ}: {msg}")
        } else {
            format!("{typ}:{sev}: {msg}")
        };
    }
    typ.to_string()
}

#[derive(Debug, Clone)]
enum JsonValue {
    String(String),
    Number(i128),
    Raw,
}

fn string_field(fields: &HashMap<String, JsonValue>, key: &str) -> Option<String> {
    match fields.get(key)? {
        JsonValue::String(s) => Some(s.clone()),
        JsonValue::Number(n) => Some(n.to_string()),
        JsonValue::Raw => None,
    }
}

fn u64_field(fields: &HashMap<String, JsonValue>, key: &str) -> Option<u64> {
    match fields.get(key)? {
        JsonValue::Number(n) => (*n).try_into().ok(),
        JsonValue::String(s) => s.parse().ok(),
        JsonValue::Raw => None,
    }
}

fn parse_json_object_top(input: &str) -> std::result::Result<HashMap<String, JsonValue>, String> {
    let bytes = input.as_bytes();
    let mut i = skip_ws(bytes, 0);
    if bytes.get(i) != Some(&b'{') {
        return Err("expected `{`".into());
    }
    i += 1;
    let mut out = HashMap::new();
    loop {
        i = skip_ws(bytes, i);
        if bytes.get(i) == Some(&b'}') {
            return Ok(out);
        }
        let (key, next) = parse_json_string(bytes, i)?;
        i = skip_ws(bytes, next);
        if bytes.get(i) != Some(&b':') {
            return Err("expected `:`".into());
        }
        i = skip_ws(bytes, i + 1);
        let (value, next) = parse_json_value(bytes, i)?;
        out.insert(key, value);
        i = skip_ws(bytes, next);
        match bytes.get(i) {
            Some(b',') => i += 1,
            Some(b'}') => return Ok(out),
            _ => return Err("expected `,` or `}`".into()),
        }
    }
}

fn parse_json_value(bytes: &[u8], i: usize) -> std::result::Result<(JsonValue, usize), String> {
    match bytes.get(i) {
        Some(b'"') => {
            let (s, next) = parse_json_string(bytes, i)?;
            Ok((JsonValue::String(s), next))
        }
        Some(b'-' | b'0'..=b'9') => parse_json_number(bytes, i),
        Some(b'{') | Some(b'[') => Ok((JsonValue::Raw, skip_json_container(bytes, i)?)),
        Some(b't') if bytes.get(i..i + 4) == Some(b"true") => Ok((JsonValue::Raw, i + 4)),
        Some(b'f') if bytes.get(i..i + 5) == Some(b"false") => Ok((JsonValue::Raw, i + 5)),
        Some(b'n') if bytes.get(i..i + 4) == Some(b"null") => Ok((JsonValue::Raw, i + 4)),
        _ => Err("unsupported JSON value".into()),
    }
}

fn parse_json_number(
    bytes: &[u8],
    mut i: usize,
) -> std::result::Result<(JsonValue, usize), String> {
    let start = i;
    if bytes.get(i) == Some(&b'-') {
        i += 1;
    }
    while matches!(bytes.get(i), Some(b'0'..=b'9')) {
        i += 1;
    }
    if matches!(bytes.get(i), Some(b'.' | b'e' | b'E')) {
        while let Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') = bytes.get(i) {
            i += 1;
        }
        return Ok((JsonValue::Raw, i));
    }
    let s = std::str::from_utf8(&bytes[start..i]).map_err(|e| e.to_string())?;
    let n = s.parse::<i128>().map_err(|e| e.to_string())?;
    Ok((JsonValue::Number(n), i))
}

fn parse_json_string(bytes: &[u8], mut i: usize) -> std::result::Result<(String, usize), String> {
    if bytes.get(i) != Some(&b'"') {
        return Err("expected JSON string".into());
    }
    i += 1;
    let mut out = String::new();
    while let Some(&b) = bytes.get(i) {
        match b {
            b'"' => return Ok((out, i + 1)),
            b'\\' => {
                i += 1;
                let Some(&esc) = bytes.get(i) else {
                    return Err("unterminated JSON escape".into());
                };
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let hex = bytes
                            .get(i + 1..i + 5)
                            .ok_or_else(|| "short JSON unicode escape".to_string())?;
                        let s = std::str::from_utf8(hex).map_err(|e| e.to_string())?;
                        let cp = u16::from_str_radix(s, 16).map_err(|e| e.to_string())?;
                        if let Some(ch) = char::from_u32(cp as u32) {
                            out.push(ch);
                        }
                        i += 4;
                    }
                    _ => return Err("unsupported JSON escape".into()),
                }
            }
            _ => out.push(b as char),
        }
        i += 1;
    }
    Err("unterminated JSON string".into())
}

fn skip_json_container(bytes: &[u8], mut i: usize) -> std::result::Result<usize, String> {
    let opener = *bytes
        .get(i)
        .ok_or_else(|| "missing container".to_string())?;
    let closer = if opener == b'{' { b'}' } else { b']' };
    let mut depth = 0usize;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'"' => {
                let (_, next) = parse_json_string(bytes, i)?;
                i = next;
                continue;
            }
            x if x == opener => depth += 1,
            x if x == closer => {
                depth -= 1;
                if depth == 0 {
                    return Ok(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    Err("unterminated JSON container".into())
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while matches!(bytes.get(i), Some(b' ' | b'\n' | b'\r' | b'\t')) {
        i += 1;
    }
    i
}

fn json_escape(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn vcd_comment_escape(s: &str) -> String {
    s.replace('\n', "\\n").replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_generated_trace_events() {
        let trace = r#"{"type":"meta","schema_version":1}
{"type":"tlm_call","cycle":3,"seq":7,"vcd_time":42,"clock":"clk","clock_cycle":3,"component":"drv","bus":"b","method":"read","phase":"request","direction":"initiator","tag":1}
{"type":"randomize","cycle":4,"seq":8,"vcd_time":43,"clock":"clk","clock_cycle":4,"target":"t","fields":{"addr":5}}"#;
        let events = parse_trace_events(trace).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].typ, "tlm_call");
        assert_eq!(events[0].tag, Some(1));
        assert_eq!(events[1].typ, "randomize");
    }

    #[test]
    fn merges_semantic_scope_and_events_into_vcd() {
        let vcd = r#"$date today $end
$scope module top $end
$var wire 1 ! clk $end
$upscope $end
$enddefinitions $end
#0
0!
#5
1!
#10
0!
"#;
        let events = parse_trace_events(
            r#"{"type":"tlm_call","cycle":1,"seq":2,"vcd_time":5,"clock":"clk","clock_cycle":1,"component":"drv","bus":"b","method":"read","phase":"request","direction":"initiator"}
{"type":"log","cycle":1,"seq":3,"vcd_time":5,"clock":"clk","clock_cycle":1,"severity":"INFO","message":"hello"}"#,
        )
        .unwrap();
        let (merged, maps) = merge_trace_into_vcd(vcd, &events).unwrap();
        assert!(merged.contains("$scope module harc_semantic $end"));
        assert!(merged.contains("HARC_TRACE_MAP event_type 1 tlm_call"));
        assert!(merged.contains("HARC_TRACE_MAP event_type 2 log"));
        assert!(merged.contains("$var wire 1 HARC0 event0_valid $end"));
        assert!(merged.contains("$var wire 1 HARC14 event1_valid $end"));
        assert!(merged.contains("#5\n0HARC0\n0HARC11\n0HARC14\n0HARC25\n"));
        assert!(merged.contains("1HARC0"));
        assert!(merged.contains("1HARC14"));
        assert!(render_map_json(&maps).contains("\"tlm_call\""));
    }
}
