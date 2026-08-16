//! Compiler-native HARC code graph.
//!
//! The first storage format is deliberately plain JSONL so it can be consumed
//! by agents and shell tools without adding a graph database dependency.

use crate::ast::*;
use crate::lexer::Span;
use crate::{codegen, ir, parser};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const FILES_JSONL: &str = "files.jsonl";
const NODES_JSONL: &str = "nodes.jsonl";
const EDGES_JSONL: &str = "edges.jsonl";

#[derive(Debug, Default)]
pub struct IndexStats {
    pub files: usize,
    pub nodes: usize,
    pub edges: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone)]
pub struct FileRecord {
    pub id: String,
    pub path: String,
    pub abs_path: Option<String>,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct NodeRecord {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub file: String,
    pub span: Option<SpanInfo>,
    pub doc: Option<String>,
    pub frontmatter: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EdgeRecord {
    pub kind: String,
    pub from: String,
    pub to: String,
    pub file: String,
    pub span: Option<SpanInfo>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct SpanInfo {
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Default)]
struct GraphBuilder {
    files: BTreeMap<String, FileRecord>,
    nodes: BTreeMap<String, NodeRecord>,
    edges: BTreeMap<String, EdgeRecord>,
    known_symbols: HashMap<String, Vec<SymbolDef>>,
}

#[derive(Debug)]
struct ParsedFile {
    display: String,
    source: String,
    ast: SourceFile,
}

#[derive(Debug, Clone)]
struct SymbolDef {
    id: String,
    kind: String,
    file: String,
}

#[derive(Debug, Clone)]
pub struct GraphIndex {
    pub index_root: String,
    pub files: Vec<FileRecord>,
    pub nodes: Vec<NodeRecord>,
    pub edges: Vec<EdgeRecord>,
}

impl GraphBuilder {
    fn add_file(&mut self, path: String, kind: &str) -> String {
        let id = format!("file:{path}");
        let abs_path = absolute_path_string(&path);
        self.files.entry(id.clone()).or_insert(FileRecord {
            id: id.clone(),
            path,
            abs_path,
            kind: kind.to_string(),
        });
        id
    }

    fn add_node(&mut self, node: NodeRecord) {
        if !node.name.is_empty() && is_symbol_node_kind(&node.kind) {
            let defs = self.known_symbols.entry(node.name.clone()).or_default();
            if !defs.iter().any(|def| def.id == node.id) {
                defs.push(SymbolDef {
                    id: node.id.clone(),
                    kind: node.kind.clone(),
                    file: node.file.clone(),
                });
            }
        }
        self.nodes.entry(node.id.clone()).or_insert(node);
    }

    fn add_edge(&mut self, edge: EdgeRecord) {
        let key = format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}",
            edge.kind,
            edge.from,
            edge.to,
            edge.label.as_deref().unwrap_or("")
        );
        self.edges.entry(key).or_insert(edge);
    }
}

pub fn index_paths(inputs: &[PathBuf], out_dir: &Path) -> std::io::Result<IndexStats> {
    let mut builder = GraphBuilder::default();
    let mut harc_paths = Vec::new();
    let mut dut_paths = Vec::new();
    for input in inputs {
        collect_paths(input, &mut harc_paths, &mut dut_paths)?;
    }
    harc_paths.sort();
    harc_paths.dedup();
    dut_paths.sort();
    dut_paths.dedup();

    for path in &dut_paths {
        let display = display_path(path);
        let file_id = builder.add_file(display.clone(), dut_kind(path));
        let node = NodeRecord {
            id: format!("dut:{display}"),
            kind: "dut".to_string(),
            name: path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(&display)
                .to_string(),
            file: display.clone(),
            span: None,
            doc: None,
            frontmatter: None,
            summary: Some(format!("{} DUT source", dut_kind(path))),
        };
        builder.add_node(node.clone());
        builder.add_edge(EdgeRecord {
            kind: "defines".to_string(),
            from: file_id,
            to: node.id,
            file: display,
            span: None,
            label: None,
        });
    }

    let mut parsed = Vec::new();
    let mut skipped = 0usize;
    for path in &harc_paths {
        let display = display_path(path);
        let source = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        match parser::parse_source(&source) {
            Ok(ast) => parsed.push(ParsedFile {
                display,
                source,
                ast,
            }),
            Err(_) => skipped += 1,
        }
    }

    let imported = resolve_imported_bus_files(&parsed);
    parsed.extend(imported);

    for file in &parsed {
        index_ast_file(&mut builder, file, false);
    }
    for file in &parsed {
        index_ast_file(&mut builder, file, true);
    }
    index_lowered_ir(&mut builder, &parsed);

    fs::create_dir_all(out_dir)?;
    write_jsonl(
        out_dir.join(FILES_JSONL),
        builder.files.values().map(file_json),
    )?;
    write_jsonl(
        out_dir.join(NODES_JSONL),
        builder.nodes.values().map(node_json),
    )?;
    write_jsonl(
        out_dir.join(EDGES_JSONL),
        builder.edges.values().map(edge_json),
    )?;

    Ok(IndexStats {
        files: builder.files.len(),
        nodes: builder.nodes.len(),
        edges: builder.edges.len(),
        skipped,
    })
}

pub fn query(index_dir: &Path, query: &str, limit: usize) -> std::io::Result<String> {
    let nodes = read_nodes(index_dir)?;
    let edges = read_edges(index_dir)?;
    let terms = terms(query);
    let mut hits: Vec<(i32, String)> = Vec::new();
    for node in &nodes {
        let hay = format!(
            "{} {} {} {} {}",
            node.kind,
            node.name,
            node.file,
            node.summary.as_deref().unwrap_or(""),
            node.doc.as_deref().unwrap_or("")
        )
        .to_lowercase();
        let score = score_terms(&hay, &terms);
        if score > 0 {
            hits.push((score, format_node_hit(node)));
        }
    }
    for edge in &edges {
        let hay = format!(
            "{} {} {} {}",
            edge.kind,
            edge.from,
            edge.to,
            edge.label.as_deref().unwrap_or("")
        )
        .to_lowercase();
        let score = score_terms(&hay, &terms);
        if score > 0 {
            hits.push((score, format_edge_hit(edge)));
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(format_hits(hits, limit))
}

pub fn load_index(index_dir: &Path) -> std::io::Result<GraphIndex> {
    Ok(GraphIndex {
        index_root: display_path(index_dir),
        files: read_files(index_dir)?,
        nodes: read_nodes(index_dir)?,
        edges: read_edges(index_dir)?,
    })
}

pub fn render_html(index: &GraphIndex, title: &str) -> std::io::Result<String> {
    render_html_with_source_root(index, title, None)
}

pub fn render_html_with_source_root(
    index: &GraphIndex,
    title: &str,
    source_root: Option<&Path>,
) -> std::io::Result<String> {
    let title_html = html_escape(title);
    let source_root_uri = source_root.map(file_url_for_dir);
    let data = escape_json_for_script(&graph_json(index, source_root_uri.as_deref()));
    let template = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__TITLE__</title>
<style>
:root {
  color-scheme: light;
  --bg: #f7f8fa;
  --panel: #ffffff;
  --line: #d9dee7;
  --text: #1d2430;
  --muted: #687386;
  --accent: #1267a8;
  --accent-soft: #e8f2fb;
  --edge: #4b5565;
  --shadow: 0 1px 2px rgba(16, 24, 40, 0.08);
}
* { box-sizing: border-box; }
body {
  margin: 0;
  min-height: 100vh;
  background: var(--bg);
  color: var(--text);
  font: 14px/1.45 ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
}
header {
  display: flex;
  align-items: center;
  gap: 18px;
  padding: 14px 18px;
  border-bottom: 1px solid var(--line);
  background: var(--panel);
}
h1 { margin: 0; font-size: 18px; font-weight: 650; }
.meta { color: var(--muted); font-size: 12px; }
.shell {
  display: grid;
  grid-template-columns: minmax(300px, 390px) minmax(460px, 1fr);
  height: calc(100vh - 57px);
}
.sidebar {
  border-right: 1px solid var(--line);
  background: var(--panel);
  overflow: hidden;
  display: flex;
  flex-direction: column;
}
.tools {
  padding: 12px;
  border-bottom: 1px solid var(--line);
  display: grid;
  gap: 8px;
}
input, select {
  width: 100%;
  min-height: 34px;
  border: 1px solid var(--line);
  border-radius: 6px;
  padding: 6px 9px;
  color: var(--text);
  background: #fff;
  font: inherit;
}
.counts { color: var(--muted); font-size: 12px; }
.node-list { overflow: auto; padding: 8px; }
.node-row {
  width: 100%;
  text-align: left;
  border: 1px solid transparent;
  border-radius: 6px;
  padding: 8px;
  background: transparent;
  cursor: pointer;
  display: grid;
  gap: 2px;
}
.node-row:hover, .node-row.active { background: var(--accent-soft); border-color: #b9d8ef; }
.kind {
  display: inline-flex;
  align-items: center;
  width: fit-content;
  border: 1px solid var(--line);
  border-radius: 999px;
  padding: 1px 7px;
  color: var(--muted);
  font-size: 11px;
  background: #fff;
}
.node-name { font-weight: 640; overflow-wrap: anywhere; }
.loc {
  color: var(--muted);
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 12px;
  overflow-wrap: anywhere;
}
main { overflow: auto; padding: 18px; }
.detail { max-width: 1180px; display: grid; gap: 14px; }
.panel {
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 8px;
  box-shadow: var(--shadow);
  padding: 14px;
}
.detail-head { display: flex; align-items: flex-start; justify-content: space-between; gap: 12px; }
.detail h2 { margin: 4px 0 2px; font-size: 24px; line-height: 1.2; overflow-wrap: anywhere; }
.link { color: var(--accent); text-decoration: none; }
.link:hover { text-decoration: underline; }
.grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(220px, 1fr)); gap: 10px; }
.kv { border: 1px solid var(--line); border-radius: 6px; padding: 9px; background: #fbfcfe; }
.kv b {
  display: block;
  margin-bottom: 3px;
  color: var(--muted);
  font-size: 11px;
  text-transform: uppercase;
}
pre {
  margin: 0;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 12px;
}
.edges { display: grid; grid-template-columns: repeat(auto-fit, minmax(320px, 1fr)); gap: 14px; }
.focus-graph {
  display: grid;
  grid-template-columns: minmax(160px, 1fr) 90px minmax(180px, 1fr) 90px minmax(160px, 1fr);
  gap: 10px;
  align-items: center;
}
.graph-col { display: grid; gap: 8px; }
.graph-col-title { color: var(--muted); font-size: 11px; font-weight: 650; text-transform: uppercase; }
.graph-node {
  border: 1px solid var(--line);
  border-radius: 6px;
  padding: 8px;
  background: #fbfcfe;
  color: var(--text);
  cursor: pointer;
  text-align: left;
  font: inherit;
  overflow-wrap: anywhere;
}
.graph-node:hover, .graph-node.center { border-color: #9ac7e8; background: var(--accent-soft); }
.graph-arrow {
  color: var(--muted);
  text-align: center;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}
.edge-list { display: grid; gap: 8px; }
.edge-card { border: 1px solid var(--line); border-radius: 6px; padding: 9px; background: #fbfcfe; }
.edge-kind { color: var(--edge); font-weight: 650; font-size: 12px; }
.edge-target {
  display: block;
  margin-top: 3px;
  border: 0;
  padding: 0;
  background: transparent;
  color: var(--accent);
  cursor: pointer;
  font: inherit;
  text-align: left;
  overflow-wrap: anywhere;
}
.edge-missing {
  display: block;
  margin-top: 3px;
  color: var(--muted);
  overflow-wrap: anywhere;
}
.doc { border-left: 3px solid var(--accent); padding-left: 10px; color: #2f3a4b; }
.empty { color: var(--muted); padding: 18px; }
@media (max-width: 860px) {
  .shell { grid-template-columns: 1fr; height: auto; }
  .sidebar { border-right: 0; border-bottom: 1px solid var(--line); max-height: 55vh; }
  main { padding: 12px; }
  .focus-graph { grid-template-columns: 1fr; }
  .graph-arrow { display: none; }
}
</style>
</head>
<body>
<header>
  <h1>__TITLE__</h1>
  <div class="meta" id="summary"></div>
</header>
<div class="shell harc-graph-viewer">
  <aside class="sidebar">
    <div class="tools">
      <input id="search" type="search" placeholder="Search names, kinds, files, docs">
      <select id="kind"></select>
      <div class="counts" id="counts"></div>
    </div>
    <div class="node-list" id="nodeList"></div>
  </aside>
  <main>
    <div class="detail" id="detail"></div>
  </main>
</div>
<script>
const graph = __DATA__;
const files = new Map(graph.files.map(f => [f.path, f]));
const nodes = new Map(graph.nodes.map(n => [n.id, n]));
const outgoing = new Map();
const incoming = new Map();
for (const edge of graph.edges) {
  if (!outgoing.has(edge.from)) outgoing.set(edge.from, []);
  if (!incoming.has(edge.to)) incoming.set(edge.to, []);
  outgoing.get(edge.from).push(edge);
  incoming.get(edge.to).push(edge);
}
const listEl = document.getElementById('nodeList');
const detailEl = document.getElementById('detail');
const searchEl = document.getElementById('search');
const kindEl = document.getElementById('kind');
const countsEl = document.getElementById('counts');
const summaryEl = document.getElementById('summary');
let selectedId = graph.nodes[0]?.id || null;
summaryEl.textContent = `${graph.nodes.length} nodes, ${graph.edges.length} edges${graph.indexRoot ? `, index ${graph.indexRoot}` : ''}`;

function esc(value) {
  return String(value ?? '').replace(/[&<>"']/g, ch => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch]));
}
function label(node) { return node ? node.name : 'unresolved'; }
function loc(node) {
  if (!node) return '';
  return node.span && node.span.line > 0 ? `${node.file}:${node.span.line}` : node.file;
}
function edgeLoc(edge, fallbackNode) {
  if (edge.span && edge.span.line > 0) return `${edge.file}:${edge.span.line}`;
  return fallbackNode ? loc(fallbackNode) : edge.file;
}
function isAbsoluteUri(value) {
  return /^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(value);
}
function pathHref(path) {
  if (!path) return '';
  if (isAbsoluteUri(path)) return path;
  if (path.startsWith('/')) return new URL(path, 'file://').href;
  if (graph.sourceRootUri) return new URL(path, graph.sourceRootUri).href;
  return path;
}
function sourceHref(file) {
  const record = files.get(file);
  return pathHref(record?.abs_path || file);
}
function fileHref(node) {
  if (!node || !node.file) return '';
  let href = sourceHref(node.file);
  if (node.span && node.span.line > 0) href += '#L' + node.span.line;
  return href;
}
function searchText(node) {
  return [node.kind, node.name, node.file, node.summary || '', node.doc || '', node.frontmatter || ''].join(' ').toLowerCase();
}
function populateKinds() {
  const kinds = [...new Set(graph.nodes.map(n => n.kind))].sort();
  kindEl.innerHTML = '<option value="">All node kinds</option>' + kinds.map(k => `<option value="${esc(k)}">${esc(k)}</option>`).join('');
}
function filteredNodes() {
  const q = searchEl.value.trim().toLowerCase();
  const terms = q ? q.split(/\s+/) : [];
  const kind = kindEl.value;
  return graph.nodes.filter(node => {
    if (kind && node.kind !== kind) return false;
    const text = searchText(node);
    return terms.every(term => text.includes(term));
  });
}
function renderList() {
  const rows = filteredNodes();
  countsEl.textContent = `${rows.length} shown`;
  listEl.innerHTML = rows.map(node => `
    <button class="node-row${node.id === selectedId ? ' active' : ''}" data-node-id="${esc(node.id)}">
      <span class="kind">${esc(node.kind)}</span>
      <span class="node-name">${esc(label(node))}</span>
      <span class="loc">${esc(loc(node))}</span>
    </button>
  `).join('') || '<div class="empty">No matching nodes.</div>';
}
function attrsHtml(node) {
  const rows = [];
  if (node.summary) rows.push(['Summary', node.summary]);
  if (node.frontmatter) rows.push(['Frontmatter', node.frontmatter]);
  if (!rows.length) return '';
  return rows.map(([name, value]) => `<div class="kv"><b>${esc(name)}</b><pre>${esc(value)}</pre></div>`).join('');
}
function edgeCards(edges, mode) {
  if (!edges.length) return '<div class="empty">No edges.</div>';
  return edges.map(edge => {
    const otherId = mode === 'out' ? edge.to : edge.from;
    const other = nodes.get(otherId);
    const source = nodes.get(edge.from);
    const where = edgeLoc(edge, source);
    const target = other
      ? `<button class="edge-target" data-node-id="${esc(otherId)}">${esc(`${other.kind} ${label(other)}`)}</button>`
      : `<div class="edge-missing">${esc(otherId)}</div>`;
    return `<div class="edge-card">
      <div class="edge-kind">${esc(edge.kind)} <span class="meta">${edge.label ? ' · ' + esc(edge.label) : ''}</span></div>
      ${target}
      <div class="loc">${esc(where)}</div>
    </div>`;
  }).join('');
}
function graphNodeButton(node, extraClass = '') {
  if (!node) return '';
  return `<button class="graph-node ${extraClass}" data-node-id="${esc(node.id)}">
    <span class="kind">${esc(node.kind)}</span>
    <div>${esc(label(node))}</div>
    <div class="loc">${esc(loc(node))}</div>
  </button>`;
}
function neighborColumn(edges, mode) {
  const seen = new Set();
  const buttons = [];
  for (const edge of edges) {
    const id = mode === 'in' ? edge.from : edge.to;
    if (seen.has(id)) continue;
    seen.add(id);
    const node = nodes.get(id);
    if (node) buttons.push(graphNodeButton(node));
    if (buttons.length >= 8) break;
  }
  return buttons.join('') || '<div class="empty">No linked nodes.</div>';
}
function neighborhoodHtml(node, inEdges, outEdges) {
  return `<section class="panel">
    <h3>Neighborhood</h3>
    <div class="focus-graph">
      <div class="graph-col"><div class="graph-col-title">Incoming</div>${neighborColumn(inEdges, 'in')}</div>
      <div class="graph-arrow">-&gt;</div>
      <div class="graph-col"><div class="graph-col-title">Selected</div>${graphNodeButton(node, 'center')}</div>
      <div class="graph-arrow">-&gt;</div>
      <div class="graph-col"><div class="graph-col-title">Outgoing</div>${neighborColumn(outEdges, 'out')}</div>
    </div>
  </section>`;
}
function renderDetail(id) {
  const node = nodes.get(id);
  if (!node) {
    detailEl.innerHTML = '<div class="panel empty">Select a node.</div>';
    return;
  }
  selectedId = id;
  const href = fileHref(node);
  const outEdges = outgoing.get(id) || [];
  const inEdges = incoming.get(id) || [];
  detailEl.innerHTML = `
    <section class="panel">
      <div class="detail-head">
        <div>
          <span class="kind">${esc(node.kind)}</span>
          <h2>${esc(label(node))}</h2>
          <div class="loc">${esc(node.id)}</div>
        </div>
        ${href ? `<a class="link" href="${esc(href)}">Open source</a>` : ''}
      </div>
    </section>
    <section class="grid">
      <div class="kv"><b>Location</b><div>${esc(loc(node))}</div></div>
      <div class="kv"><b>File</b><div>${esc(node.file)}</div></div>
      <div class="kv"><b>Kind</b><div>${esc(node.kind)}</div></div>
      ${attrsHtml(node)}
    </section>
    ${node.doc ? `<section class="panel doc"><pre>${esc(node.doc)}</pre></section>` : ''}
    ${neighborhoodHtml(node, inEdges, outEdges)}
    <section class="edges">
      <div class="panel"><h3>Outgoing</h3><div class="edge-list">${edgeCards(outEdges, 'out')}</div></div>
      <div class="panel"><h3>Incoming</h3><div class="edge-list">${edgeCards(inEdges, 'in')}</div></div>
    </section>
  `;
  renderList();
}
listEl.addEventListener('click', event => {
  const target = event.target.closest('[data-node-id]');
  if (target) renderDetail(target.dataset.nodeId);
});
detailEl.addEventListener('click', event => {
  const target = event.target.closest('[data-node-id]');
  if (target) renderDetail(target.dataset.nodeId);
});
searchEl.addEventListener('input', renderList);
kindEl.addEventListener('change', renderList);
populateKinds();
renderList();
renderDetail(selectedId);
</script>
</body>
</html>
"#;
    Ok(template
        .replace("__TITLE__", &title_html)
        .replace("__DATA__", &data))
}

pub fn tests_for(index_dir: &Path, symbol: &str, limit: usize) -> std::io::Result<String> {
    let nodes = read_nodes(index_dir)?;
    let edges = read_edges(index_dir)?;
    let mut id_to_node = HashMap::new();
    for node in &nodes {
        id_to_node.insert(node.id.clone(), node);
    }
    let symbol_l = symbol.to_lowercase();
    let targets: BTreeSet<String> = nodes
        .iter()
        .filter(|n| {
            n.name.to_lowercase().contains(&symbol_l)
                || n.id.to_lowercase().contains(&symbol_l)
                || n.summary
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&symbol_l)
        })
        .map(|n| n.id.clone())
        .collect();
    let mut tests = BTreeSet::new();
    for edge in &edges {
        if !targets.contains(&edge.to) {
            continue;
        }
        if matches!(
            edge.kind.as_str(),
            "binds_dut"
                | "uses_arch_dut"
                | "uses_sv_dut"
                | "uses_type"
                | "uses_transactor"
                | "uses_scoreboard"
                | "binds_bus"
        ) {
            if let Some(test_id) = nearest_test(&edge.from, &edges, &id_to_node) {
                tests.insert(test_id);
            }
        }
    }
    let lines = tests
        .into_iter()
        .filter_map(|id| id_to_node.get(&id).map(|n| format_node_hit(n)))
        .take(limit.max(1))
        .collect::<Vec<_>>();
    Ok(if lines.is_empty() {
        "(no matching tests)".to_string()
    } else {
        lines.join("\n")
    })
}

pub fn impact(
    index_dir: &Path,
    symbol: &str,
    depth: usize,
    limit: usize,
) -> std::io::Result<String> {
    let nodes = read_nodes(index_dir)?;
    let edges = read_edges(index_dir)?;
    let mut id_to_node = HashMap::new();
    for node in &nodes {
        id_to_node.insert(node.id.clone(), node);
    }
    let symbol_l = symbol.to_lowercase();
    let mut frontier: BTreeSet<String> = nodes
        .iter()
        .filter(|n| {
            n.name.to_lowercase().contains(&symbol_l) || n.id.to_lowercase().contains(&symbol_l)
        })
        .map(|n| n.id.clone())
        .collect();
    let mut seen = frontier.clone();
    let mut out = Vec::new();
    for _ in 0..depth.max(1) {
        let mut next = BTreeSet::new();
        for edge in &edges {
            let connected = frontier.contains(&edge.from) || frontier.contains(&edge.to);
            if !connected {
                continue;
            }
            out.push(format_edge_hit(edge));
            for id in [&edge.from, &edge.to] {
                if seen.insert(id.clone()) {
                    next.insert(id.clone());
                    if let Some(node) = id_to_node.get(id) {
                        out.push(format_node_hit(node));
                    }
                }
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    out.sort();
    out.dedup();
    Ok(if out.is_empty() {
        "(no impact slice)".to_string()
    } else {
        out.into_iter()
            .take(limit.max(1))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

pub fn context(index_dir: &Path, task: &str, limit: usize) -> std::io::Result<String> {
    let mut out = Vec::new();
    let q = query(index_dir, task, limit.max(1))?;
    out.push("query hits:".to_string());
    out.push(q);
    let first_term = terms(task).into_iter().next().unwrap_or_default();
    if !first_term.is_empty() {
        out.push(String::new());
        out.push("impact:".to_string());
        out.push(impact(index_dir, &first_term, 2, limit.max(1))?);
    }
    Ok(out.join("\n"))
}

fn index_ast_file(builder: &mut GraphBuilder, file: &ParsedFile, relationships: bool) {
    let file_id = builder.add_file(file.display.clone(), "harc");
    if let Some(doc) = &file.ast.inner_doc {
        let doc_id = format!("doc:{}", file.display);
        builder.add_node(NodeRecord {
            id: doc_id.clone(),
            kind: "doc".to_string(),
            name: file.display.clone(),
            file: file.display.clone(),
            span: None,
            doc: Some(doc.clone()),
            frontmatter: file.ast.frontmatter.clone(),
            summary: Some("file-level HARC documentation".to_string()),
        });
        builder.add_edge(EdgeRecord {
            kind: "has_doc".to_string(),
            from: file_id.clone(),
            to: doc_id.clone(),
            file: file.display.clone(),
            span: None,
            label: None,
        });
        if file.ast.frontmatter.is_some() {
            builder.add_edge(EdgeRecord {
                kind: "has_spec".to_string(),
                from: file_id.clone(),
                to: doc_id,
                file: file.display.clone(),
                span: None,
                label: None,
            });
        }
    }

    let mut stack = Vec::new();
    for item in &file.ast.items {
        index_item(builder, item, file, &file_id, &mut stack, relationships);
    }
}

fn index_item(
    builder: &mut GraphBuilder,
    item: &Item,
    file: &ParsedFile,
    file_id: &str,
    stack: &mut Vec<String>,
    relationships: bool,
) {
    let construct = item.as_construct();
    let kind = node_kind(item);
    let name = construct.name().name.clone();
    let node_id = node_id(&kind, &name, &file.display);
    let summary = match item {
        Item::ExternalModule(m) => format!("{} external DUT", m.kind.name),
        _ => format!("{} {name}", construct.kind_label()),
    };
    builder.add_node(NodeRecord {
        id: node_id.clone(),
        kind: kind.clone(),
        name: name.clone(),
        file: file.display.clone(),
        span: Some(span_info(construct.span(), &file.source)),
        doc: construct.doc().map(str::to_string),
        frontmatter: None,
        summary: Some(summary),
    });
    builder.add_edge(EdgeRecord {
        kind: if stack.is_empty() {
            "defines"
        } else {
            "contains"
        }
        .to_string(),
        from: stack.last().cloned().unwrap_or_else(|| file_id.to_string()),
        to: node_id.clone(),
        file: file.display.clone(),
        span: Some(span_info(construct.span(), &file.source)),
        label: None,
    });
    if construct.doc().is_some() {
        let doc_id = format!("doc:{}:{kind}:{name}", file.display);
        builder.add_node(NodeRecord {
            id: doc_id.clone(),
            kind: "doc".to_string(),
            name: name.clone(),
            file: file.display.clone(),
            span: Some(span_info(construct.span(), &file.source)),
            doc: construct.doc().map(str::to_string),
            frontmatter: None,
            summary: Some(format!("documentation for {kind} {name}")),
        });
        builder.add_edge(EdgeRecord {
            kind: "has_doc".to_string(),
            from: node_id.clone(),
            to: doc_id,
            file: file.display.clone(),
            span: Some(span_info(construct.span(), &file.source)),
            label: None,
        });
    }

    if relationships {
        index_item_relationships(builder, item, file, &node_id);
    }

    if let Item::Package(pkg) = item {
        stack.push(node_id);
        for child in &pkg.items {
            index_item(builder, child, file, file_id, stack, relationships);
        }
        stack.pop();
    }
}

fn index_item_relationships(
    builder: &mut GraphBuilder,
    item: &Item,
    file: &ParsedFile,
    owner: &str,
) {
    match item {
        Item::Use(u) => {
            if let Some(last) = u.path.segments.last() {
                edge_to_symbol(builder, "imports", owner, &last.name, file, u.span, None);
            }
        }
        Item::Struct(s) => {
            for field in &s.fields {
                index_type(builder, owner, &field.ty, file, field.span);
            }
            for body in &s.body {
                index_txn_body(builder, owner, body, file);
            }
        }
        Item::Transaction(t) => {
            for body in &t.body {
                index_txn_body(builder, owner, body, file);
            }
        }
        Item::Tseq(t) => {
            if let Some(ty) = &t.return_ty {
                index_type(builder, owner, ty, file, t.span);
            }
            index_block(builder, owner, &t.body, file);
        }
        Item::Relation(r) => match &r.body {
            RelationBody::Block(exprs) => {
                for expr in exprs {
                    index_expr(builder, owner, expr, file);
                }
            }
            RelationBody::Alias(expr) => index_expr(builder, owner, expr, file),
        },
        Item::Agent(c) | Item::Env(c) | Item::Scoreboard(c) | Item::Sequencer(c) => {
            if let Some(ty) = &c.bound_to {
                index_type(builder, owner, ty, file, c.span);
                edge_to_type(builder, "binds_bus", owner, ty, file, c.span);
            }
            for item in &c.items {
                index_component_item(builder, owner, item, file);
            }
        }
        Item::Transactor(t) => {
            if let Some(ty) = &t.bound_to {
                index_type(builder, owner, ty, file, t.span);
                edge_to_type(builder, "binds_bus", owner, ty, file, t.span);
            }
            for item in &t.items {
                index_component_item(builder, owner, item, file);
            }
            if let Some(active) = &t.when_active {
                for item in active {
                    index_component_item(builder, owner, item, file);
                }
            }
        }
        Item::Test(t) => {
            if let Some(tb) = &t.for_testbench {
                edge_to_symbol(builder, "uses_type", owner, &tb.name, file, tb.span, None);
            }
            for item in &t.items {
                index_test_item(builder, owner, item, file);
            }
        }
        Item::Covergroup(c) => {
            if let Some(trigger) = &c.trigger {
                match trigger {
                    CoverTrigger::Clock(expr) => index_expr(builder, owner, expr, file),
                    CoverTrigger::Hook { call, .. } => {
                        edge_to_symbol(
                            builder,
                            "samples",
                            owner,
                            &expr_text(call),
                            file,
                            call.span,
                            None,
                        );
                        index_expr(builder, owner, call, file);
                    }
                }
            }
            for item in &c.items {
                match item {
                    CoverItem::Point(p) => {
                        edge_to_symbol(builder, "covers", owner, &p.name.name, file, p.span, None);
                        index_expr(builder, owner, &p.target, file);
                    }
                    CoverItem::Cross(cross) => {
                        for p in &cross.points {
                            edge_to_symbol(builder, "covers", owner, &p.name, file, p.span, None);
                        }
                    }
                }
            }
        }
        Item::Property(p) => {
            edge_to_symbol(builder, "checks", owner, &p.name.name, file, p.span, None);
            index_expr(builder, owner, &p.body, file);
        }
        Item::Pseq(p) => index_expr(builder, owner, &p.body, file),
        Item::CoverSequence(c) => {
            edge_to_symbol(builder, "covers", owner, &c.name.name, file, c.span, None);
            index_expr(builder, owner, &c.pattern, file);
        }
        Item::Function(f) => {
            for p in &f.params {
                if let Some(ty) = &p.ty {
                    index_type(builder, owner, ty, file, p.span);
                }
            }
            if let Some(ty) = &f.return_ty {
                index_type(builder, owner, ty, file, f.span);
            }
            index_block(builder, owner, &f.body, file);
        }
        Item::ExternFn(f) => {
            for p in &f.params {
                if let Some(ty) = &p.ty {
                    index_type(builder, owner, ty, file, p.span);
                }
            }
        }
        Item::Regblock(r) => {
            edge_to_symbol(
                builder,
                "uses_transactor",
                owner,
                &r.via_helper.name,
                file,
                r.via_helper.span,
                None,
            );
        }
        Item::Addrmap(a) => {
            edge_to_symbol(
                builder,
                "uses_transactor",
                owner,
                &a.via_helper.name,
                file,
                a.via_helper.span,
                None,
            );
            for inst in &a.instances {
                edge_to_symbol(
                    builder,
                    "uses_type",
                    owner,
                    &inst.regblock_ty.name,
                    file,
                    inst.span,
                    None,
                );
            }
        }
        Item::ExternalModule(_) => {}
        Item::Bus(b) => {
            for sig in &b.signals {
                index_type(builder, owner, &sig.ty, file, sig.span);
            }
            for hs in &b.handshakes {
                for sig in &hs.payload {
                    index_type(builder, owner, &sig.ty, file, sig.span);
                }
            }
        }
        Item::Extend(e) => {
            edge_to_symbol(
                builder,
                "contains",
                owner,
                &path_name(&e.target),
                file,
                e.span,
                Some("extend_target"),
            );
            match &e.body {
                ExtendBody::TxnLike(items) => {
                    for item in items {
                        index_txn_body(builder, owner, item, file);
                    }
                }
                ExtendBody::Component(items) => {
                    for item in items {
                        index_component_item(builder, owner, item, file);
                    }
                }
                ExtendBody::Test(items) => {
                    for item in items {
                        index_test_item(builder, owner, item, file);
                    }
                }
            }
        }
        Item::Apply(a) => {
            edge_to_symbol(
                builder,
                "imports",
                owner,
                &path_name(&a.path),
                file,
                a.span,
                Some("apply"),
            );
        }
        Item::Package(_) | Item::Const(_) | Item::Domain(_) | Item::Enum(_) => {}
    }
}

fn index_txn_body(builder: &mut GraphBuilder, owner: &str, item: &TxnBodyItem, file: &ParsedFile) {
    match item {
        TxnBodyItem::Field(f) => index_type(builder, owner, &f.ty, file, f.span),
        TxnBodyItem::Keep(k) => {
            edge_to_symbol(builder, "checks", owner, "keep", file, k.span, None);
            index_expr(builder, owner, &k.expr, file);
        }
        TxnBodyItem::When(w) => {
            index_expr(builder, owner, &w.discriminant, file);
            for item in &w.items {
                index_txn_body(builder, owner, item, file);
            }
        }
    }
}

fn index_component_item(
    builder: &mut GraphBuilder,
    owner: &str,
    item: &ComponentItem,
    file: &ParsedFile,
) {
    match item {
        ComponentItem::Field(f) => {
            index_type(builder, owner, &f.ty, file, f.span);
            if let Some(ty) = &f.bound_to {
                index_type(builder, owner, ty, file, f.span);
                edge_to_type(builder, "binds_bus", owner, ty, file, f.span);
            }
            if f.default.as_ref().is_some_and(is_bind_expr) {
                edge_to_type(builder, "binds_dut", owner, &f.ty, file, f.span);
            }
            classify_use_edge(builder, owner, &f.ty, file, f.span);
        }
        ComponentItem::Connect(c) => {
            for edge in &c.edges {
                edge_to_symbol(
                    builder,
                    "drives",
                    owner,
                    &expr_text(&edge.from),
                    file,
                    edge.span,
                    Some("connect"),
                );
                edge_to_symbol(
                    builder,
                    "monitors",
                    owner,
                    &expr_text(&edge.to),
                    file,
                    edge.span,
                    Some("connect"),
                );
                index_expr(builder, owner, &edge.from, file);
                index_expr(builder, owner, &edge.to, file);
            }
        }
        ComponentItem::OnHandler(h) => {
            edge_to_symbol(
                builder,
                "monitors",
                owner,
                &expr_text(&h.event),
                file,
                h.span,
                Some("on"),
            );
            index_expr(builder, owner, &h.event, file);
            index_block(builder, owner, &h.body, file);
        }
        ComponentItem::TargetTlmThread(t) => {
            edge_to_symbol(
                builder,
                "calls",
                owner,
                &path_name(&t.method),
                file,
                t.span,
                Some("thread"),
            );
            index_block(builder, owner, &t.body, file);
        }
        ComponentItem::Hookable(h) => index_block(builder, owner, &h.body, file),
        ComponentItem::Lifecycle(_, b) => index_block(builder, owner, b, file),
        ComponentItem::Apply(a) => edge_to_symbol(
            builder,
            "imports",
            owner,
            &path_name(&a.path),
            file,
            a.span,
            Some("apply"),
        ),
        ComponentItem::Watchdog(w) => index_block(builder, owner, &w.body, file),
    }
}

fn index_test_item(builder: &mut GraphBuilder, owner: &str, item: &TestItem, file: &ParsedFile) {
    match item {
        TestItem::Apply(a) => edge_to_symbol(
            builder,
            "imports",
            owner,
            &path_name(&a.path),
            file,
            a.span,
            Some("apply"),
        ),
        TestItem::Let(l) => index_let(builder, owner, l, file),
        TestItem::Scope(s) => {
            for b in [&s.setup, &s.run, &s.check, &s.teardown]
                .into_iter()
                .flatten()
            {
                index_block(builder, owner, b, file);
            }
        }
        TestItem::Use(u) => {
            if let Some(last) = u.path.segments.last() {
                edge_to_symbol(builder, "imports", owner, &last.name, file, u.span, None);
            }
        }
        TestItem::Clock(c) => edge_to_symbol(
            builder,
            "drives",
            owner,
            &c.name.name,
            file,
            c.span,
            Some("clock"),
        ),
        TestItem::Stmt(s) => index_stmt(builder, owner, s, file),
        TestItem::Phase(_, b) => index_block(builder, owner, b, file),
    }
}

fn index_let(builder: &mut GraphBuilder, owner: &str, l: &LetStmt, file: &ParsedFile) {
    if let Some(ty) = &l.ty {
        index_type(builder, owner, ty, file, l.span);
        classify_use_edge(builder, owner, ty, file, l.span);
        if l.bind {
            let edge = if is_known_kind(builder, ty, file, "bus") {
                "binds_bus"
            } else {
                "binds_dut"
            };
            edge_to_type(builder, edge, owner, ty, file, l.span);
        }
    }
    if let Some(value) = &l.value {
        index_expr(builder, owner, value, file);
    }
}

fn index_block(builder: &mut GraphBuilder, owner: &str, block: &Block, file: &ParsedFile) {
    for stmt in &block.stmts {
        index_stmt(builder, owner, stmt, file);
    }
}

fn index_stmt(builder: &mut GraphBuilder, owner: &str, stmt: &Stmt, file: &ParsedFile) {
    match &stmt.kind {
        StmtKind::Let(l) => index_let(builder, owner, l, file),
        StmtKind::Assign { target, value } | StmtKind::Send { target, value } => {
            edge_to_symbol(
                builder,
                "drives",
                owner,
                &expr_text(target),
                file,
                stmt.span,
                None,
            );
            index_expr(builder, owner, target, file);
            index_expr(builder, owner, value, file);
        }
        StmtKind::For(f) => {
            index_expr(builder, owner, &f.iter, file);
            index_block(builder, owner, &f.body, file);
        }
        StmtKind::Repeat(r) => {
            index_expr(builder, owner, &r.count, file);
            index_block(builder, owner, &r.body, file);
        }
        StmtKind::Loop(b) => index_block(builder, owner, b, file),
        StmtKind::While { cond, body, .. } => {
            index_expr(builder, owner, cond, file);
            index_block(builder, owner, body, file);
        }
        StmtKind::If(i) => {
            index_expr(builder, owner, &i.cond, file);
            index_block(builder, owner, &i.then_block, file);
            for (cond, block) in &i.elsifs {
                index_expr(builder, owner, cond, file);
                index_block(builder, owner, block, file);
            }
            if let Some(block) = &i.else_block {
                index_block(builder, owner, block, file);
            }
        }
        StmtKind::Fork(f) => {
            for b in &f.branches {
                index_block(builder, owner, b, file);
            }
        }
        StmtKind::Parallel(blocks) | StmtKind::Schedule(blocks) => {
            for b in blocks {
                index_block(builder, owner, b, file);
            }
        }
        StmtKind::Select(arms) => {
            for arm in arms {
                index_expr(builder, owner, &arm.event, file);
                index_block(builder, owner, &arm.action, file);
            }
        }
        StmtKind::On(h) => {
            index_component_item(builder, owner, &ComponentItem::OnHandler(h.clone()), file)
        }
        StmtKind::Emit { name, .. } => edge_to_symbol(
            builder,
            "calls",
            owner,
            &path_name(name),
            file,
            stmt.span,
            Some("emit"),
        ),
        StmtKind::Yield(e) | StmtKind::Return(Some(e)) | StmtKind::Release(e) => {
            index_expr(builder, owner, e, file)
        }
        StmtKind::Assert(v) | StmtKind::Assume(v) => {
            edge_to_symbol(builder, "checks", owner, verify_name(v), file, v.span, None);
            index_verify(builder, owner, v, file);
        }
        StmtKind::Cover(v) => {
            edge_to_symbol(builder, "covers", owner, verify_name(v), file, v.span, None);
            index_verify(builder, owner, v, file);
        }
        StmtKind::Randomize {
            target, with_body, ..
        } => {
            edge_to_symbol(
                builder,
                "randomizes",
                owner,
                &expr_text(target),
                file,
                stmt.span,
                None,
            );
            index_expr(builder, owner, target, file);
            for expr in with_body {
                index_expr(builder, owner, expr, file);
            }
        }
        StmtKind::Log { args, .. } | StmtKind::LogF { args, .. } => {
            for arg in args {
                index_call_arg(builder, owner, arg, file);
            }
        }
        StmtKind::Expr(e) => index_expr(builder, owner, e, file),
        StmtKind::After { duration, body, .. } => {
            index_expr(builder, owner, duration, file);
            index_block(builder, owner, body, file);
        }
        StmtKind::Wait { duration, .. } => index_expr(builder, owner, duration, file),
        StmtKind::WaitUntil {
            conditions,
            timeout,
            ..
        } => {
            for cond in conditions {
                index_expr(builder, owner, cond, file);
            }
            if let Some(timeout) = timeout {
                index_expr(builder, owner, &timeout.cycles, file);
                if let Some(msg) = &timeout.message {
                    index_expr(builder, owner, msg, file);
                }
            }
        }
        StmtKind::Fail { msg, .. } => index_expr(builder, owner, msg, file),
        StmtKind::Break { .. }
        | StmtKind::Continue { .. }
        | StmtKind::JoinAll { .. }
        | StmtKind::Return(None)
        | StmtKind::Apply(_) => {}
    }
}

fn index_verify(builder: &mut GraphBuilder, owner: &str, v: &Verify, file: &ParsedFile) {
    if let Some(named) = &v.named {
        edge_to_symbol(
            builder,
            "checks",
            owner,
            &named.name,
            file,
            named.span,
            Some("property"),
        );
    }
    if let Some(expr) = &v.expr {
        index_expr(builder, owner, expr, file);
    }
    if let Some(expr) = &v.else_fail {
        index_expr(builder, owner, expr, file);
    }
}

fn index_expr(builder: &mut GraphBuilder, owner: &str, expr: &Expr, file: &ParsedFile) {
    match &*expr.kind {
        ExprKind::Ident(_)
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Time(_)
        | ExprKind::String(_)
        | ExprKind::Bool(_)
        | ExprKind::ImplicitSelf => {}
        ExprKind::Field { target, .. } | ExprKind::Index { target, index: _ } => {
            index_expr(builder, owner, target, file)
        }
        ExprKind::BitSlice { target, hi, lo } => {
            index_expr(builder, owner, target, file);
            index_expr(builder, owner, hi, file);
            index_expr(builder, owner, lo, file);
        }
        ExprKind::Call { callee, args } => {
            edge_to_symbol(
                builder,
                "calls",
                owner,
                &expr_text(callee),
                file,
                expr.span,
                None,
            );
            index_expr(builder, owner, callee, file);
            for arg in args {
                index_call_arg(builder, owner, arg, file);
            }
        }
        ExprKind::ForkCall { call } | ExprKind::Paren(call) => {
            index_expr(builder, owner, call, file)
        }
        ExprKind::Cast { expr, ty } => {
            index_expr(builder, owner, expr, file);
            index_type(builder, owner, ty, file, expr.span);
        }
        ExprKind::Send { target, value } => {
            edge_to_symbol(
                builder,
                "drives",
                owner,
                &expr_text(target),
                file,
                expr.span,
                None,
            );
            index_expr(builder, owner, target, file);
            index_expr(builder, owner, value, file);
        }
        ExprKind::Unary { expr, .. }
        | ExprKind::HashHash { expr, .. }
        | ExprKind::SeqRepeat { expr, .. } => index_expr(builder, owner, expr, file),
        ExprKind::Binary { lhs, rhs, .. }
        | ExprKind::Membership {
            expr: lhs,
            set: rhs,
        }
        | ExprKind::CoverArrow { lhs, rhs, .. } => {
            index_expr(builder, owner, lhs, file);
            index_expr(builder, owner, rhs, file);
        }
        ExprKind::Ternary {
            cond,
            then_branch,
            else_branch,
        } => {
            index_expr(builder, owner, cond, file);
            index_expr(builder, owner, then_branch, file);
            index_expr(builder, owner, else_branch, file);
        }
        ExprKind::RangeLit { lo, hi } => {
            if let Some(e) = lo {
                index_expr(builder, owner, e, file);
            }
            if let Some(e) = hi {
                index_expr(builder, owner, e, file);
            }
        }
        ExprKind::SetLit(exprs) => {
            for e in exprs {
                index_expr(builder, owner, e, file);
            }
        }
        ExprKind::DistLit(entries) => index_dist_entries(builder, owner, entries, file),
        ExprKind::SystemCall { args, .. } | ExprKind::SolveOrder { args } => {
            for e in args {
                index_expr(builder, owner, e, file);
            }
        }
        ExprKind::SoftConstraint(sc) => {
            index_expr(builder, owner, &sc.expr, file);
            if let Some(weight) = &sc.weight {
                index_expr(builder, owner, weight, file);
            }
        }
        ExprKind::Randomize {
            target, with_body, ..
        } => {
            edge_to_symbol(
                builder,
                "randomizes",
                owner,
                &expr_text(target),
                file,
                expr.span,
                None,
            );
            index_expr(builder, owner, target, file);
            for e in with_body {
                index_expr(builder, owner, e, file);
            }
        }
        ExprKind::DistDirective { target, entries } => {
            index_expr(builder, owner, target, file);
            index_dist_entries(builder, owner, entries, file);
        }
        ExprKind::NamedArg { value, .. } => index_expr(builder, owner, value, file),
        ExprKind::StructLit { ty, fields } => {
            index_type(builder, owner, ty, file, expr.span);
            for f in fields {
                index_expr(builder, owner, &f.value, file);
            }
        }
        ExprKind::ForEachConstraint { iter, body, .. } => {
            index_expr(builder, owner, iter, file);
            for e in body {
                index_expr(builder, owner, e, file);
            }
        }
    }
}

fn index_call_arg(builder: &mut GraphBuilder, owner: &str, arg: &CallArg, file: &ParsedFile) {
    match arg {
        CallArg::Expr(e) | CallArg::Named { value: e, .. } => index_expr(builder, owner, e, file),
    }
}

fn index_dist_entries(
    builder: &mut GraphBuilder,
    owner: &str,
    entries: &[DistEntry],
    file: &ParsedFile,
) {
    for entry in entries {
        index_expr(builder, owner, &entry.value, file);
        index_expr(builder, owner, &entry.weight, file);
    }
}

fn index_type(
    builder: &mut GraphBuilder,
    owner: &str,
    ty: &TypeExpr,
    file: &ParsedFile,
    span: Span,
) {
    match ty {
        TypeExpr::Named { name, generics, .. } => {
            edge_to_symbol(
                builder,
                "uses_type",
                owner,
                &path_name(name),
                file,
                span,
                None,
            );
            for generic in generics {
                match generic {
                    TypeArg::Expr(e) | TypeArg::Named { value: e, .. } => {
                        index_expr(builder, owner, e, file)
                    }
                    TypeArg::Type(t) => index_type(builder, owner, t, file, span),
                }
            }
        }
        TypeExpr::Builtin { args, .. } => {
            for arg in args {
                match arg {
                    TypeArg::Expr(e) | TypeArg::Named { value: e, .. } => {
                        index_expr(builder, owner, e, file)
                    }
                    TypeArg::Type(t) => index_type(builder, owner, t, file, span),
                }
            }
        }
    }
}

fn index_lowered_ir(builder: &mut GraphBuilder, parsed: &[ParsedFile]) {
    if parsed.is_empty() {
        return;
    }
    let bus_files: Vec<&ParsedFile> = parsed
        .iter()
        .filter(|file| {
            file.ast
                .items
                .iter()
                .all(|item| matches!(item, Item::Bus(_)))
        })
        .collect();
    for file in parsed {
        if file
            .ast
            .items
            .iter()
            .all(|item| matches!(item, Item::Bus(_)))
        {
            continue;
        }
        let mut asts = vec![file.ast.clone()];
        for bus_file in &bus_files {
            if bus_file.display != file.display {
                asts.push(bus_file.ast.clone());
            }
        }
        let Ok(merged) = codegen::merge::merge_for_sim(asts, None) else {
            continue;
        };
        let Ok(program) = ir::lower::lower_program(&merged) else {
            continue;
        };
        for func in &program.functions {
            let kind = "ir_function";
            let id = node_id(kind, &func.name, &file.display);
            builder.add_node(NodeRecord {
                id: id.clone(),
                kind: kind.to_string(),
                name: func.name.clone(),
                file: file.display.clone(),
                span: None,
                doc: None,
                frontmatter: None,
                summary: Some(format!(
                    "lowered TB-IR function ({})",
                    function_kind_label(&func.kind, &program)
                )),
            });
            let source_name = source_name_for_ir_function(func, &program);
            builder.add_edge(EdgeRecord {
                kind: "lowers_to".to_string(),
                from: node_id(source_name.0, &source_name.1, &file.display),
                to: id,
                file: file.display.clone(),
                span: None,
                label: None,
            });
        }
        for (i, site) in program.constraint_sites.iter().enumerate() {
            let id = node_id(
                "constraint_site",
                &format!("constraint_site_{i}"),
                &file.display,
            );
            builder.add_node(NodeRecord {
                id,
                kind: "constraint_site".to_string(),
                name: format!("constraint_site_{i}"),
                file: file.display.clone(),
                span: None,
                doc: None,
                frontmatter: None,
                summary: Some(format!("randomize {}", site.record)),
            });
        }
    }
}

fn source_name_for_ir_function<'a>(
    func: &'a ir::TbFunction,
    program: &'a ir::TbProgram,
) -> (&'static str, String) {
    match &func.kind {
        ir::FunctionKind::Run | ir::FunctionKind::Check => {
            let owner = func
                .owner
                .map(|id| program.testbench(id).name.clone())
                .unwrap_or_else(|| func.name.clone());
            ("testbench", owner)
        }
        ir::FunctionKind::SamplerAuto { covgroup } => (
            "covergroup",
            program.covgroups[covgroup.index()].name.clone(),
        ),
        ir::FunctionKind::Helper => ("function", func.name.clone()),
        ir::FunctionKind::TransactorBody { transactor } => {
            ("transactor", program.transactor(*transactor).name.clone())
        }
        ir::FunctionKind::ComponentMethod { component } => {
            ("env", program.components[component.index()].name.clone())
        }
        ir::FunctionKind::Tseq { .. } => ("pseq", func.name.clone()),
        ir::FunctionKind::TestHook => ("test", func.name.clone()),
    }
}

fn function_kind_label(kind: &ir::FunctionKind, program: &ir::TbProgram) -> String {
    match kind {
        ir::FunctionKind::Run => "run".to_string(),
        ir::FunctionKind::Check => "check".to_string(),
        ir::FunctionKind::SamplerAuto { covgroup } => {
            format!("sampler:{}", program.covgroups[covgroup.index()].name)
        }
        ir::FunctionKind::Helper => "helper".to_string(),
        ir::FunctionKind::TransactorBody { transactor } => {
            format!("transactor:{}", program.transactor(*transactor).name)
        }
        ir::FunctionKind::ComponentMethod { component } => {
            format!("component:{}", program.components[component.index()].name)
        }
        ir::FunctionKind::Tseq { .. } => "tseq".to_string(),
        ir::FunctionKind::TestHook => "test_hook".to_string(),
    }
}

fn node_kind(item: &Item) -> String {
    match item {
        Item::Use(_) => "construct",
        Item::Package(_) => "construct",
        Item::Const(_) => "construct",
        Item::Domain(_) => "construct",
        Item::Struct(_) => "construct",
        Item::Enum(_) => "construct",
        Item::Transaction(_) => "transaction",
        Item::Relation(_) => "construct",
        Item::Tseq(_) => "pseq",
        Item::Agent(_) => "agent",
        Item::Env(c) if c.kind == ComponentKind::Testbench => "testbench",
        Item::Env(_) => "env",
        Item::Scoreboard(_) => "scoreboard",
        Item::Sequencer(_) => "sequencer",
        Item::Test(_) => "test",
        Item::Extend(_) => "construct",
        Item::Covergroup(_) => "covergroup",
        Item::Property(_) => "property",
        Item::Pseq(_) => "pseq",
        Item::CoverSequence(_) => "cover_sequence",
        Item::ExternalModule(_) => "dut",
        Item::Function(_) => "function",
        Item::ExternFn(_) => "extern_fn",
        Item::Apply(_) => "construct",
        Item::Bus(_) => "bus",
        Item::Transactor(_) => "transactor",
        Item::Regblock(_) => "regblock",
        Item::Addrmap(_) => "addrmap",
    }
    .to_string()
}

fn is_symbol_node_kind(kind: &str) -> bool {
    !matches!(
        kind,
        "construct" | "doc" | "ir_function" | "constraint_site"
    )
}

fn classify_use_edge(
    builder: &mut GraphBuilder,
    owner: &str,
    ty: &TypeExpr,
    file: &ParsedFile,
    span: Span,
) {
    if is_known_kind(builder, ty, file, "transactor") {
        edge_to_type(builder, "uses_transactor", owner, ty, file, span);
    }
    if is_known_kind(builder, ty, file, "scoreboard") {
        edge_to_type(builder, "uses_scoreboard", owner, ty, file, span);
    }
}

fn is_known_kind(builder: &GraphBuilder, ty: &TypeExpr, file: &ParsedFile, want: &str) -> bool {
    let Some(name) = type_name(ty) else {
        return false;
    };
    resolve_symbol(builder, name, file).is_some_and(|def| def.kind == want)
}

fn resolve_symbol<'a>(
    builder: &'a GraphBuilder,
    symbol: &str,
    file: &ParsedFile,
) -> Option<&'a SymbolDef> {
    resolve_symbol_in_file(builder, symbol, &file.display)
}

fn resolve_symbol_in_file<'a>(
    builder: &'a GraphBuilder,
    symbol: &str,
    file_display: &str,
) -> Option<&'a SymbolDef> {
    let defs = builder.known_symbols.get(symbol)?;
    let mut same_file = defs.iter().filter(|def| def.file == file_display);
    match (same_file.next(), same_file.next()) {
        (Some(def), None) => return Some(def),
        (Some(_), Some(_)) => return None,
        _ => {}
    }
    if defs.len() == 1 {
        defs.first()
    } else {
        None
    }
}

fn edge_to_type(
    builder: &mut GraphBuilder,
    kind: &str,
    from: &str,
    ty: &TypeExpr,
    file: &ParsedFile,
    span: Span,
) {
    if let Some(name) = type_name(ty) {
        edge_to_symbol(builder, kind, from, name, file, span, None);
    }
}

fn edge_to_symbol(
    builder: &mut GraphBuilder,
    kind: &str,
    from: &str,
    symbol: &str,
    file: &ParsedFile,
    span: Span,
    label: Option<&str>,
) {
    let to = resolve_symbol(builder, symbol, file)
        .map(|def| def.id.clone())
        .unwrap_or_else(|| format!("symbol:{symbol}"));
    builder.add_edge(EdgeRecord {
        kind: kind.to_string(),
        from: from.to_string(),
        to,
        file: file.display.clone(),
        span: Some(span_info(span, &file.source)),
        label: label.map(str::to_string),
    });
}

fn collect_paths(
    path: &Path,
    harc: &mut Vec<PathBuf>,
    dut: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            collect_paths(&entry?.path(), harc, dut)?;
        }
    } else if path.is_file() {
        match path.extension().and_then(|e| e.to_str()) {
            Some("harc") => harc.push(path.to_path_buf()),
            Some("sv") | Some("arch") => dut.push(path.to_path_buf()),
            _ => {}
        }
    }
    Ok(())
}

fn resolve_imported_bus_files(parsed: &[ParsedFile]) -> Vec<ParsedFile> {
    let mut wanted = BTreeSet::new();
    for file in parsed {
        collect_use_names(&file.ast.items, &mut wanted);
    }
    if wanted.is_empty() {
        return Vec::new();
    }

    let mut search_dirs = Vec::new();
    if let Ok(raw) = std::env::var("HARC_LIB_PATH") {
        for part in raw.split(':').filter(|p| !p.is_empty()) {
            search_dirs.push(PathBuf::from(part));
        }
    }
    search_dirs.push(PathBuf::from("stdlib"));
    for file in parsed {
        let base = Path::new(&file.display)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        search_dirs.push(base.join("stdlib"));
        search_dirs.push(base.join("../arch-com/stdlib"));
        search_dirs.push(base.join("../arch-com/examples"));
    }
    search_dirs.sort();
    search_dirs.dedup();

    let mut imported = Vec::new();
    let mut seen_paths = BTreeSet::new();
    for name in wanted {
        for dir in &search_dirs {
            let candidates = [
                dir.join(format!("{name}.harc")),
                dir.join(format!("{name}.arch")),
            ];
            let Some(path) = candidates.into_iter().find(|p| p.exists()) else {
                continue;
            };
            let display = display_path(&path);
            if !seen_paths.insert(display.clone()) {
                break;
            }
            let Ok(source) = fs::read_to_string(&path) else {
                break;
            };
            let Ok(ast) = parser::parse_source(&source) else {
                break;
            };
            let bus_items: Vec<Item> = ast
                .items
                .into_iter()
                .filter(|item| matches!(item, Item::Bus(_)))
                .collect();
            if !bus_items.is_empty() {
                imported.push(ParsedFile {
                    display,
                    source,
                    ast: SourceFile {
                        items: bus_items,
                        inner_doc: None,
                        frontmatter: None,
                    },
                });
            }
            break;
        }
    }
    imported
}

fn collect_use_names(items: &[Item], out: &mut BTreeSet<String>) {
    for item in items {
        match item {
            Item::Use(u) => {
                if let Some(last) = u.path.segments.last() {
                    out.insert(last.name.clone());
                }
            }
            Item::Package(p) => collect_use_names(&p.items, out),
            _ => {}
        }
    }
}

fn nearest_test<'a>(
    from: &str,
    edges: &[EdgeRecord],
    nodes: &HashMap<String, &'a NodeRecord>,
) -> Option<String> {
    if nodes.get(from).is_some_and(|n| n.kind == "test") {
        return Some(from.to_string());
    }
    for edge in edges {
        if edge.to == from && edge.kind == "contains" {
            if nodes.get(&edge.from).is_some_and(|n| n.kind == "test") {
                return Some(edge.from.clone());
            }
        }
    }
    None
}

fn type_name(ty: &TypeExpr) -> Option<&str> {
    match ty {
        TypeExpr::Named { name, .. } => Some(path_name(name)),
        TypeExpr::Builtin { .. } => None,
    }
}

fn is_bind_expr(expr: &Expr) -> bool {
    match &*expr.kind {
        ExprKind::Ident(i) => i.name == "bind",
        ExprKind::Call { callee, .. } => is_bind_expr(callee),
        _ => false,
    }
}

fn verify_name(v: &Verify) -> &str {
    v.named
        .as_ref()
        .map(|i| i.name.as_str())
        .unwrap_or("inline")
}

fn path_name(path: &crate::ast::Path) -> &str {
    path.segments
        .last()
        .map(|s| s.name.as_str())
        .unwrap_or("<empty>")
}

fn node_id(kind: &str, name: &str, file: &str) -> String {
    format!("{kind}:{file}:{name}")
}

fn display_path(path: &Path) -> String {
    let cwd = std::env::current_dir().ok();
    if let Some(cwd) = cwd {
        if let Ok(rel) = path.strip_prefix(&cwd) {
            return rel.display().to_string();
        }
    }
    path.display().to_string()
}

fn absolute_path_string(path: &str) -> Option<String> {
    let path = Path::new(path);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    Some(abs.to_string_lossy().replace('\\', "/"))
}

fn dut_kind(path: &Path) -> &str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("arch") => "arch",
        Some("sv") => "sv",
        _ => "dut",
    }
}

fn span_info(span: Span, src: &str) -> SpanInfo {
    let mut line = 1usize;
    let mut col = 1usize;
    for (idx, ch) in src.char_indices() {
        if idx >= span.start_usize() {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    SpanInfo {
        start: span.start_usize(),
        end: span.end_usize(),
        line,
        col,
    }
}

fn expr_text(expr: &Expr) -> String {
    match &*expr.kind {
        ExprKind::Ident(i) => i.name.clone(),
        ExprKind::Field { target, name } => format!("{}.{}", expr_text(target), name.name),
        ExprKind::Call { callee, .. } => expr_text(callee),
        ExprKind::Paren(e) => expr_text(e),
        ExprKind::String(s) => s.clone(),
        ExprKind::Int(s) => s.clone(),
        _ => format!("{:?}", expr.kind),
    }
}

fn write_jsonl<I>(path: PathBuf, lines: I) -> std::io::Result<()>
where
    I: IntoIterator<Item = String>,
{
    let mut file = fs::File::create(path)?;
    for line in lines {
        writeln!(file, "{line}")?;
    }
    Ok(())
}

fn file_json(file: &FileRecord) -> String {
    let mut fields = vec![
        format!("\"id\":\"{}\"", esc(&file.id)),
        format!("\"path\":\"{}\"", esc(&file.path)),
        format!("\"kind\":\"{}\"", esc(&file.kind)),
    ];
    if let Some(abs_path) = &file.abs_path {
        fields.push(format!("\"abs_path\":\"{}\"", esc(abs_path)));
    }
    format!("{{{}}}", fields.join(","))
}

fn node_json(node: &NodeRecord) -> String {
    let mut fields = vec![
        format!("\"id\":\"{}\"", esc(&node.id)),
        format!("\"kind\":\"{}\"", esc(&node.kind)),
        format!("\"name\":\"{}\"", esc(&node.name)),
        format!("\"file\":\"{}\"", esc(&node.file)),
    ];
    if let Some(span) = node.span {
        fields.push(format!(
            "\"span\":{{\"start\":{},\"end\":{},\"line\":{},\"col\":{}}}",
            span.start, span.end, span.line, span.col
        ));
    }
    if let Some(doc) = &node.doc {
        fields.push(format!("\"doc\":\"{}\"", esc(doc)));
    }
    if let Some(frontmatter) = &node.frontmatter {
        fields.push(format!("\"frontmatter\":\"{}\"", esc(frontmatter)));
    }
    if let Some(summary) = &node.summary {
        fields.push(format!("\"summary\":\"{}\"", esc(summary)));
    }
    format!("{{{}}}", fields.join(","))
}

fn edge_json(edge: &EdgeRecord) -> String {
    let mut fields = vec![
        format!("\"kind\":\"{}\"", esc(&edge.kind)),
        format!("\"from\":\"{}\"", esc(&edge.from)),
        format!("\"to\":\"{}\"", esc(&edge.to)),
        format!("\"file\":\"{}\"", esc(&edge.file)),
    ];
    if let Some(span) = edge.span {
        fields.push(format!(
            "\"span\":{{\"start\":{},\"end\":{},\"line\":{},\"col\":{}}}",
            span.start, span.end, span.line, span.col
        ));
    }
    if let Some(label) = &edge.label {
        fields.push(format!("\"label\":\"{}\"", esc(label)));
    }
    format!("{{{}}}", fields.join(","))
}

fn graph_json(index: &GraphIndex, source_root_uri: Option<&str>) -> String {
    let source_root_json = source_root_uri
        .map(|uri| format!("\"{}\"", esc(uri)))
        .unwrap_or_else(|| "null".to_string());
    format!(
        "{{\"indexRoot\":\"{}\",\"sourceRootUri\":{},\"files\":[{}],\"nodes\":[{}],\"edges\":[{}]}}",
        esc(&index.index_root),
        source_root_json,
        index
            .files
            .iter()
            .map(file_json)
            .collect::<Vec<_>>()
            .join(","),
        index
            .nodes
            .iter()
            .map(node_json)
            .collect::<Vec<_>>()
            .join(","),
        index
            .edges
            .iter()
            .map(edge_json)
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn file_url_for_dir(path: &Path) -> String {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut raw = abs.to_string_lossy().replace('\\', "/");
    if !raw.ends_with('/') {
        raw.push('/');
    }
    format!("file://{}", percent_encode_url_path(&raw))
}

fn percent_encode_url_path(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn escape_json_for_script(value: &str) -> String {
    value
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\'', "\\u0027")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn read_files(index_dir: &Path) -> std::io::Result<Vec<FileRecord>> {
    let file = fs::File::open(index_dir.join(FILES_JSONL))?;
    let reader = std::io::BufReader::new(file);
    Ok(reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| {
            Some(FileRecord {
                id: json_string(&line, "id")?,
                path: json_string(&line, "path")?,
                abs_path: json_string(&line, "abs_path"),
                kind: json_string(&line, "kind")?,
            })
        })
        .collect())
}

fn read_nodes(index_dir: &Path) -> std::io::Result<Vec<NodeRecord>> {
    let file = fs::File::open(index_dir.join(NODES_JSONL))?;
    let reader = std::io::BufReader::new(file);
    Ok(reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| {
            Some(NodeRecord {
                id: json_string(&line, "id")?,
                kind: json_string(&line, "kind")?,
                name: json_string(&line, "name")?,
                file: json_string(&line, "file")?,
                span: json_span(&line),
                doc: json_string(&line, "doc"),
                frontmatter: json_string(&line, "frontmatter"),
                summary: json_string(&line, "summary"),
            })
        })
        .collect())
}

fn read_edges(index_dir: &Path) -> std::io::Result<Vec<EdgeRecord>> {
    let file = fs::File::open(index_dir.join(EDGES_JSONL))?;
    let reader = std::io::BufReader::new(file);
    Ok(reader
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| {
            Some(EdgeRecord {
                kind: json_string(&line, "kind")?,
                from: json_string(&line, "from")?,
                to: json_string(&line, "to")?,
                file: json_string(&line, "file")?,
                span: json_span(&line),
                label: json_string(&line, "label"),
            })
        })
        .collect())
}

fn json_string(line: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = line.find(&needle)? + needle.len();
    let mut out = String::new();
    let mut chars = line[start..].chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                other => out.push(other),
            },
            other => out.push(other),
        }
    }
    None
}

fn json_span(line: &str) -> Option<SpanInfo> {
    let start = json_usize(line, "start")?;
    let end = json_usize(line, "end")?;
    let line_no = json_usize(line, "line")?;
    let col = json_usize(line, "col")?;
    Some(SpanInfo {
        start,
        end,
        line: line_no,
        col,
    })
}

fn json_usize(line: &str, key: &str) -> Option<usize> {
    let needle = format!("\"{key}\":");
    let start = line.find(&needle)? + needle.len();
    let digits: String = line[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

fn score_terms(hay: &str, terms: &[String]) -> i32 {
    if terms.is_empty() {
        return 0;
    }
    terms
        .iter()
        .map(|term| if hay.contains(term) { 10 } else { 0 })
        .sum()
}

fn format_hits(hits: Vec<(i32, String)>, limit: usize) -> String {
    let lines: Vec<String> = hits
        .into_iter()
        .take(limit.max(1))
        .map(|(_, line)| line)
        .collect();
    if lines.is_empty() {
        "(no matches)".to_string()
    } else {
        lines.join("\n")
    }
}

fn format_node_hit(node: &NodeRecord) -> String {
    let loc = node
        .span
        .filter(|span| span.line > 0)
        .map(|span| format!("{}:{}", node.file, span.line))
        .unwrap_or_else(|| node.file.clone());
    format!(
        "{} {} [{}] {}",
        node.kind,
        node.name,
        loc,
        node.summary.as_deref().unwrap_or("")
    )
}

fn format_edge_hit(edge: &EdgeRecord) -> String {
    let loc = edge
        .span
        .filter(|span| span.line > 0)
        .map(|span| format!("{}:{}", edge.file, span.line))
        .unwrap_or_else(|| edge.file.clone());
    format!(
        "{} [{}]: {} -> {}{}",
        edge.kind,
        loc,
        edge.from,
        edge.to,
        edge.label
            .as_ref()
            .map(|l| format!(" ({l})"))
            .unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_escapes_strings_needed_by_query() {
        let node = NodeRecord {
            id: "node:1".into(),
            kind: "test".into(),
            name: "Smoke".into(),
            file: "a.harc".into(),
            span: None,
            doc: Some("line \"one\"\nline two".into()),
            frontmatter: None,
            summary: Some("summary".into()),
        };
        let raw = node_json(&node);
        assert_eq!(
            json_string(&raw, "doc").as_deref(),
            Some("line \"one\"\nline two")
        );
    }

    #[test]
    fn symbol_resolution_prefers_same_file_and_rejects_ambiguous_cross_file_names() {
        let mut builder = GraphBuilder::default();
        builder.known_symbols.insert(
            "AxilXactor".into(),
            vec![
                SymbolDef {
                    id: "transactor:a.harc:AxilXactor".into(),
                    kind: "transactor".into(),
                    file: "a.harc".into(),
                },
                SymbolDef {
                    id: "transactor:b.harc:AxilXactor".into(),
                    kind: "transactor".into(),
                    file: "b.harc".into(),
                },
            ],
        );

        assert_eq!(
            resolve_symbol_in_file(&builder, "AxilXactor", "b.harc").map(|def| def.id.as_str()),
            Some("transactor:b.harc:AxilXactor")
        );
        assert!(resolve_symbol_in_file(&builder, "AxilXactor", "c.harc").is_none());
    }

    #[test]
    fn external_module_definition_pass_registers_canonical_dut_symbol() {
        let source = "module D kind verilator\nend module D\n";
        let ast = parser::parse_source(source).expect("external module parses");
        let file = ParsedFile {
            display: "d.harc".into(),
            source: source.into(),
            ast,
        };
        let mut builder = GraphBuilder::default();
        index_ast_file(&mut builder, &file, false);

        let def = resolve_symbol_in_file(&builder, "D", "d.harc").expect("D resolves");
        assert_eq!(def.id, "dut:d.harc:D");
        assert_eq!(def.kind, "dut");
        assert!(builder.nodes.contains_key("dut:d.harc:D"));
        assert!(!builder.nodes.contains_key("external_module:d.harc:D"));
    }
}
